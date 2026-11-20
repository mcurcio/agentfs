//! The node plugin's supervisor against a fake launcher, whose hosts are
//! tokio tasks on the child end of a socket pair: each writes its hello,
//! answers every request, records it, and exits when the test says so (or
//! at EOF, when the test asked for that). A fake mounter keeps an in-memory
//! mount table. Time is paused, so the restart backoff runs at once.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agentfs_contracts::host::{Reply, Request, TreeRef};
use agentfs_contracts::name::{ClientLabel, Label, VolumeName};
use agentfs_contracts::pod::PodContext;
use agentfs_csi::error::Error;
use agentfs_csi::launcher::{Exit, Launcher, ProcessSpec, Spawned};
use agentfs_csi::layout::{Layout, target_key};
use agentfs_csi::mounter::Mounter;
use agentfs_csi::records::{self, ClaimKind, ClaimRecord, PublishedRecord, RECORD_VERSION};
use agentfs_csi::supervisor::{NodeConfig, Supervisor, ViewSpec};
use agentfs_fuse::MountState;
use rustix::process::Signal;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::sync::{oneshot, watch};

/// One fake host.
struct FakeHost {
    volume: String,
    requests: Arc<Mutex<Vec<Request>>>,
    exit: Arc<watch::Sender<Option<Exit>>>,
    kill: Option<oneshot::Sender<()>>,
}

#[derive(Default)]
struct World {
    hosts: Vec<FakeHost>,
    specs: Vec<ProcessSpec>,
    /// At EOF a host exits with this code; `None`: when the test says.
    eof_exit: Option<i32>,
}

#[derive(Clone)]
struct Fake {
    world: Arc<Mutex<World>>,
    /// Every event, in order: `spawn host v`, `open <target>`, `close
    /// <target>`, `reconcile`, `eof host v`.
    log: Arc<watch::Sender<Vec<String>>>,
}

impl Fake {
    fn new() -> Self {
        Self {
            world: Arc::new(Mutex::new(World {
                eof_exit: Some(0),
                ..World::default()
            })),
            log: Arc::new(watch::channel(Vec::new()).0),
        }
    }

    fn event(log: &watch::Sender<Vec<String>>, event: String) {
        log.send_modify(|l| l.push(event));
    }

    fn events(&self) -> Vec<String> {
        self.log.borrow().clone()
    }

    /// Waits for `needle` to have been logged `n` times: an event.
    async fn until(&self, needle: &str, n: usize) {
        let mut rx = self.log.subscribe();
        rx.wait_for(|l| l.iter().filter(|e| *e == needle).count() >= n)
            .await
            .unwrap();
    }

    fn hosts(&self) -> usize {
        self.world.lock().unwrap().hosts.len()
    }

    fn requests(&self, host: usize) -> Vec<Request> {
        self.world.lock().unwrap().hosts[host]
            .requests
            .lock()
            .unwrap()
            .clone()
    }

    /// Crashes host `i`: its stream closes and it exits SIGKILL.
    fn kill(&self, host: usize) {
        let kill = self.world.lock().unwrap().hosts[host].kill.take();
        kill.unwrap().send(()).unwrap();
    }

    fn exit(&self, host: usize, exit: Exit) {
        let tx = self.world.lock().unwrap().hosts[host].exit.clone();
        tx.send(Some(exit)).unwrap();
    }
}

fn arg_after(spec: &ProcessSpec, flag: &str) -> Option<String> {
    let i = spec.args.iter().position(|a| a == flag)?;
    Some(spec.args.get(i + 1)?.to_string_lossy().into_owned())
}

fn describe(request: &Request) -> String {
    match request {
        Request::Open(view) => format!("open {}", view.at.display()),
        Request::Close { at } => format!("close {}", at.display()),
        Request::Reconcile => "reconcile".into(),
        other => format!("{other:?}"),
    }
}

impl Launcher for Fake {
    fn spawn(&self, spec: ProcessSpec) -> io::Result<Spawned> {
        let state = PathBuf::from(arg_after(&spec, "--state").unwrap());
        self.world.lock().unwrap().specs.push(spec.clone());
        let (exit_tx, exited) = watch::channel(None);
        let exit_tx = Arc::new(exit_tx);
        if !spec.control {
            // `agentfs init`: the volume made.
            fs::create_dir_all(state.join("history.git")).unwrap();
            Self::event(&self.log, spec.label.clone());
            exit_tx.send(Some(Exit::Code(0))).unwrap();
            return Ok(Spawned {
                pid: 1,
                exited,
                recent_log: Arc::default(),
                control: None,
            });
        }
        let volume = spec.label.strip_prefix("host ").unwrap().to_owned();
        let name = arg_after(&spec, "--volume").unwrap();
        let (ours, theirs) = tokio::net::UnixStream::pair()?;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (kill, mut killed) = oneshot::channel();
        let eof_exit = {
            let mut world = self.world.lock().unwrap();
            world.hosts.push(FakeHost {
                volume: volume.clone(),
                requests: requests.clone(),
                exit: exit_tx.clone(),
                kill: Some(kill),
            });
            world.eof_exit
        };
        Self::event(&self.log, format!("spawn host {volume}"));
        let log = self.log.clone();
        tokio::spawn(async move {
            let (read, mut write) = theirs.into_split();
            let hello = format!(
                "{{\"v\":1,\"volume\":\"{name}\",\"id\":\"01234567-89ab-4def-8123-456789abcdef\"}}\n"
            );
            write.write_all(hello.as_bytes()).await.unwrap();
            let mut lines = BufReader::new(read).lines();
            loop {
                tokio::select! {
                    _ = &mut killed => {
                        drop((lines, write));
                        exit_tx.send(Some(Exit::Signal(9))).unwrap();
                        return;
                    }
                    line = lines.next_line() => {
                        let Ok(Some(line)) = line else { break };
                        let request: Request = serde_json::from_str(&line).unwrap();
                        Fake::event(&log, describe(&request));
                        let reply = match &request {
                            Request::Open(_) => Reply::Opened {
                                tree: agentfs_contracts::tree::TreeId::Root,
                                stacked: false,
                            },
                            Request::Close { .. } => Reply::Closed,
                            Request::Reconcile => Reply::Reconciled { ended: 0 },
                            other => panic!("unexpected {other:?}"),
                        };
                        requests.lock().unwrap().push(request);
                        let mut line = serde_json::to_string(&reply).unwrap();
                        line.push('\n');
                        if write.write_all(line.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                }
            }
            drop((lines, write));
            Fake::event(&log, format!("eof host {volume}"));
            if let Some(code) = eof_exit {
                exit_tx.send(Some(Exit::Code(code))).unwrap();
            }
        });
        Ok(Spawned {
            pid: 2,
            exited,
            recent_log: Arc::default(),
            control: Some(ours),
        })
    }

    fn signal(&self, _: u32, _: Signal) -> io::Result<()> {
        Ok(())
    }
}

/// An in-memory mount table; every detach is counted.
#[derive(Default)]
struct Table {
    mounts: Mutex<HashMap<PathBuf, Vec<MountState>>>,
    detaches: Mutex<Vec<PathBuf>>,
}

impl Mounter for Table {
    fn detach(&self, path: &Path) -> io::Result<()> {
        self.detaches.lock().unwrap().push(path.to_path_buf());
        self.mounts.lock().unwrap().get_mut(path).and_then(Vec::pop);
        Ok(())
    }
    fn state(&self, path: &Path) -> io::Result<MountState> {
        Ok(self
            .mounts
            .lock()
            .unwrap()
            .get(path)
            .and_then(|s| s.last().cloned())
            .unwrap_or(MountState::NotMounted))
    }
}

struct Node {
    dir: tempfile::TempDir,
    layout: Layout,
    fake: Fake,
    table: Arc<Table>,
    sup: Arc<Supervisor>,
}

impl Node {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout {
            base: dir.path().join("base"),
            node_state: dir.path().join("node"),
            profiles: dir.path().join("profiles"),
        };
        fs::create_dir_all(&layout.profiles).unwrap();
        fs::write(layout.profiles.join("p.config"), "").unwrap();
        fs::write(layout.profiles.join("p.exclude"), "*.tmp\n").unwrap();
        let fake = Fake::new();
        let table = Arc::new(Table::default());
        let sup = Supervisor::new(
            NodeConfig::new("node-1", layout.clone()),
            Arc::new(fake.clone()),
            table.clone(),
        );
        Self {
            dir,
            layout,
            fake,
            table,
            sup,
        }
    }

    /// A claim's volume `v`, as CreateVolume leaves it.
    fn volume(&self, v: &str) {
        fs::create_dir_all(self.layout.state(v).join("history.git")).unwrap();
        records::store_claim(
            &self.layout,
            v,
            &ClaimRecord {
                v: RECORD_VERSION,
                capacity_bytes: 0,
                claim: ClaimKind::Volume {
                    name: VolumeName::parse(&format!("n-{v}")).unwrap(),
                    profile: "p".into(),
                },
            },
        )
        .unwrap();
    }

    /// A kubelet target that exists.
    fn target(&self, name: &str) -> PathBuf {
        let target = self.dir.path().join("pods").join(name).join("mount");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        target
    }

    fn view(&self, handle: &str, volume: &str, target: &Path) -> ViewSpec {
        ViewSpec {
            handle: handle.into(),
            volume: volume.into(),
            tree: TreeRef::Root,
            target: target.to_path_buf(),
            read_only: false,
            pod: PodContext {
                pod: Some(Label::parse("p-0").unwrap()),
                ..PodContext::default()
            },
        }
    }
}

fn open_at(request: &Request) -> Option<&Path> {
    match request {
        Request::Open(view) => Some(&view.at),
        _ => None,
    }
}

#[tokio::test(start_paused = true)]
async fn a_second_publish_of_a_claim_elsewhere_is_refused_and_opens_nothing() {
    let n = Node::new();
    n.volume("v");
    let (a, b) = (n.target("a"), n.target("b"));
    n.sup.publish(n.view("v", "v", &a)).await.unwrap();
    assert!(a.is_dir(), "the target directory is made");
    let err = n.sup.publish(n.view("v", "v", &b)).await.unwrap_err();
    assert_eq!(
        err,
        Error::FailedPrecondition(format!(
            "volume v is published at {} on this node: one pod per volume; several pods on one \
             volume are not built (docs/csi.md)",
            a.display()
        ))
    );
    let opens: Vec<_> = n
        .fake
        .requests(0)
        .iter()
        .filter_map(|r| open_at(r).map(Path::to_path_buf))
        .collect();
    assert_eq!(opens, vec![a.clone()], "no open for the second target");
    // The open carried the label of its target and the pod.
    let Request::Open(view) = &n.fake.requests(0)[1] else {
        panic!("{:?}", n.fake.requests(0));
    };
    assert_eq!(
        view.label,
        ClientLabel::parse(&format!("csi-{}", target_key(&a))).unwrap()
    );
    assert_eq!(view.pod.pod.as_ref().unwrap().as_str(), "p-0");
    let record = records::load_published(&n.layout).0;
    assert_eq!(record.len(), 1);
    assert_eq!(record[0].target, a);
}

/// One host serves every view of its volume; publishing the same target
/// again is idempotent, with another read-only flag ALREADY_EXISTS;
/// unpublishing the last view drops the client (the host sees EOF) and
/// waits for the host's exit.
#[tokio::test(start_paused = true)]
async fn one_host_per_volume_and_the_last_view_stops_it() {
    let n = Node::new();
    n.volume("v");
    {
        let mut world = n.fake.world.lock().unwrap();
        world.eof_exit = None;
    }
    let (a, b) = (n.target("a"), n.target("b"));
    n.sup.publish(n.view("v", "v", &a)).await.unwrap();
    // A second claim's view on the same volume (as a fork claim's is).
    n.sup.publish(n.view("w", "v", &b)).await.unwrap();
    n.sup.publish(n.view("v", "v", &a)).await.unwrap();
    let mut ro = n.view("v", "v", &a);
    ro.read_only = true;
    assert!(matches!(
        n.sup.publish(ro).await.unwrap_err(),
        Error::AlreadyExists(_)
    ));
    assert_eq!(n.fake.hosts(), 1, "{:?}", n.fake.events());
    assert_eq!(
        n.fake.events(),
        [
            "spawn host v".to_owned(),
            "reconcile".to_owned(),
            format!("open {}", a.display()),
            format!("open {}", b.display()),
        ]
    );
    // The profile's rules were installed at the host's start.
    assert_eq!(
        fs::read_to_string(n.layout.state("v").join("history.git/info/exclude")).unwrap(),
        "*.tmp\n"
    );
    assert_eq!(
        n.sup.in_use("v").await.map(|r| r.target),
        Some(a.clone()),
        "in use while a view is open"
    );
    assert!(matches!(
        n.sup.release("v").await.unwrap_err(),
        Error::FailedPrecondition(_)
    ));

    n.sup.unpublish("w", &b).await.unwrap();
    assert!(!b.exists(), "the target directory is removed");
    assert!(
        !n.fake.events().contains(&"eof host v".to_owned()),
        "a view remains: the host serves on"
    );
    let sup = n.sup.clone();
    let a2 = a.clone();
    let last = tokio::spawn(async move { sup.unpublish("v", &a2).await });
    n.fake.until("eof host v", 1).await;
    tokio::task::yield_now().await;
    assert!(!last.is_finished(), "unpublish waits for the host's exit");
    n.fake.exit(0, Exit::Code(0));
    last.await.unwrap().unwrap();
    assert!(!a.exists());
    assert_eq!(n.sup.in_use("v").await, None);
    assert!(records::load_published(&n.layout).0.is_empty());
    assert!(
        n.table.detaches.lock().unwrap().is_empty(),
        "the host closed both"
    );
}

/// A host that crashes is started again after the backoff, and opens
/// every recorded view again before it reconciles, in that order.
#[tokio::test(start_paused = true)]
async fn a_crashed_host_is_restarted_and_replays_its_views_then_reconciles() {
    let n = Node::new();
    n.volume("v");
    let (a, b) = (n.target("a"), n.target("b"));
    n.sup.publish(n.view("v", "v", &a)).await.unwrap();
    n.sup.publish(n.view("w", "v", &b)).await.unwrap();
    let crashed = tokio::time::Instant::now();
    n.fake.kill(0);
    n.fake.until("spawn host v", 2).await;
    assert!(
        crashed.elapsed() >= NodeConfig::new("x", n.layout.clone()).backoff.initial,
        "after the backoff"
    );
    // The restart holds the volume's lock from before its spawn until the
    // host has replayed and reconciled: the next call on the volume comes
    // after it.
    assert!(n.sup.in_use("v").await.is_some());
    let replayed: Vec<String> = n.fake.requests(1).iter().map(describe).collect();
    assert_eq!(
        replayed,
        [
            format!("open {}", a.display()),
            format!("open {}", b.display()),
            "reconcile".to_owned(),
        ]
    );
    // Each replayed view is the recorded one: same label, same pod.
    let Request::Open(first) = &n.fake.requests(1)[0] else {
        panic!()
    };
    let Request::Open(original) = &n.fake.requests(0)[1] else {
        panic!()
    };
    assert_eq!(first, original);
    // The restarted host serves: an unpublish closes through it.
    n.sup.unpublish("w", &b).await.unwrap();
    assert_eq!(
        n.fake.requests(1).last().map(describe),
        Some(format!("close {}", b.display()))
    );
}

/// With no host running, unpublish detaches the target itself.
#[tokio::test(start_paused = true)]
async fn unpublish_without_a_host_detaches_the_target() {
    let n = Node::new();
    let target = n.target("a");
    fs::create_dir_all(&target).unwrap();
    n.table.mounts.lock().unwrap().insert(
        target.clone(),
        vec![MountState::Stale {
            source: "agentfs:n-v".into(),
        }],
    );
    records::store_published(
        &n.layout,
        &PublishedRecord {
            v: RECORD_VERSION,
            handle: "v".into(),
            volume: "v".into(),
            tree: TreeRef::Root,
            target: target.clone(),
            read_only: false,
            pod: PodContext::default(),
            label: ClientLabel::parse("csi-x").unwrap(),
        },
    )
    .unwrap();
    n.sup.unpublish("v", &target).await.unwrap();
    assert_eq!(*n.table.detaches.lock().unwrap(), vec![target.clone()]);
    assert!(!target.exists());
    assert!(records::load_published(&n.layout).0.is_empty());
    assert_eq!(n.fake.hosts(), 0);
}

/// At plugin start a record whose target is gone is dropped; the volume's
/// host opens the rest again and reconciles.
#[tokio::test(start_paused = true)]
async fn reconcile_drops_a_record_whose_target_is_gone() {
    let n = Node::new();
    n.volume("v");
    let kept = n.target("kept");
    fs::create_dir_all(&kept).unwrap();
    let gone = n.dir.path().join("pods/gone/mount");
    for (handle, target) in [("v", &kept), ("w", &gone)] {
        records::store_published(
            &n.layout,
            &PublishedRecord {
                v: RECORD_VERSION,
                handle: handle.into(),
                volume: "v".into(),
                tree: TreeRef::Root,
                target: target.clone(),
                read_only: false,
                pod: PodContext::default(),
                label: ClientLabel::parse(&format!("csi-{}", target_key(target))).unwrap(),
            },
        )
        .unwrap();
    }
    // An older driver's node state is listed and left alone.
    fs::create_dir_all(n.layout.published_dir().join("old")).unwrap();
    let report = n.sup.reconcile().await;
    assert_eq!(report.hosts, 1);
    assert_eq!(report.views, 1);
    assert_eq!(report.dropped, 1);
    assert_eq!(report.unreadable.len(), 1, "{:?}", report.unreadable);
    assert!(n.layout.published_dir().join("old").exists());
    let requests: Vec<String> = n.fake.requests(0).iter().map(describe).collect();
    assert_eq!(
        requests,
        [format!("open {}", kept.display()), "reconcile".to_owned()]
    );
    assert!(!n.layout.published("w").exists());
    assert!(n.layout.published("v").exists());
    n.sup.shutdown().await;
    n.fake.until("eof host v", 1).await;
}

/// Release starts the volume's host with no view and stops it: Ok on its
/// exit 0, UNAVAILABLE on another.
#[tokio::test(start_paused = true)]
async fn release_starts_the_host_and_stops_it() {
    let n = Node::new();
    n.volume("v");
    n.sup.release("v").await.unwrap();
    assert_eq!(n.fake.hosts(), 1, "a host was started");
    assert_eq!(n.fake.world.lock().unwrap().hosts[0].volume, "v");
    assert_eq!(n.fake.events(), ["spawn host v", "reconcile", "eof host v"]);
    n.fake.world.lock().unwrap().eof_exit = Some(1);
    let err = n.sup.release("v").await.unwrap_err();
    assert!(matches!(err, Error::Unavailable(_)), "{err:?}");
    assert!(err.to_string().contains("exit code 1"), "{err}");
    // Nothing restarts it: no view needs it.
    assert_eq!(n.fake.hosts(), 2);
}

/// Ensure runs `agentfs init` for the claim and installs the profile's
/// rules; a missing profile is INVALID_ARGUMENT.
#[tokio::test(start_paused = true)]
async fn ensure_runs_init_and_installs_the_rules() {
    let n = Node::new();
    let params = agentfs_csi::params::VolumeParams {
        profile: "p".into(),
        name: VolumeName::parse("apps.data.pvc-1").unwrap(),
    };
    n.sup.ensure("pvc-1", &params).await.unwrap();
    let spec = n.fake.world.lock().unwrap().specs[0].clone();
    assert_eq!(spec.label, "init pvc-1");
    assert_eq!(
        arg_after(&spec, "--volume").as_deref(),
        Some("apps.data.pvc-1")
    );
    assert_eq!(
        fs::read_to_string(n.layout.state("pvc-1").join("history.git/info/exclude")).unwrap(),
        "*.tmp\n"
    );
    let missing = agentfs_csi::params::VolumeParams {
        profile: "nope".into(),
        ..params
    };
    assert!(matches!(
        n.sup.ensure("pvc-1", &missing).await.unwrap_err(),
        Error::InvalidArgument(_)
    ));
}
