//! The node plugin's supervisor against a fake launcher, whose hosts are
//! tokio tasks on the child end of a socket pair: each writes its hello,
//! answers every request by its id (a request the test holds, once the test
//! releases it), records it, and exits when the test says so (or at EOF,
//! when the test asked for that). A fake mounter keeps an in-memory mount
//! table. Time is paused, so the restart backoff runs at once.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agentfs_contracts::host::{
    Envelope, HOST_PROTOCOL, Refusal, RefusalKind, Reply, Request, TreeRef,
};
use agentfs_contracts::name::{ClientLabel, ForkKey, Label, VolumeName};
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::tree::TreeId;
use agentfs_contracts::trees::{ForkSettings, Landing};
use agentfs_csi::error::Error;
use agentfs_csi::launcher::{Exit, Launcher, ProcessSpec, Spawned};
use agentfs_csi::layout::{Layout, target_key};
use agentfs_csi::mounter::Mounter;
use agentfs_csi::records::{self, ClaimKind, ClaimRecord, PublishedRecord, RECORD_VERSION};
use agentfs_csi::supervisor::{Backoff, NodeConfig, Supervisor, ViewSpec};
use agentfs_fuse::MountState;
use agentfs_os::file_lock::{FileLock, Mode};
use rustix::process::Signal;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::sync::{Mutex as AsyncMutex, oneshot, watch};

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
    /// What hosts answer a fork create or end with, instead of `forked`
    /// (fork 1) or `ended`.
    fork_refusal: Option<RefusalKind>,
    /// The version hosts' hellos name, instead of this protocol's.
    hello_v: Option<u32>,
    /// A target every host but the first refuses to open (`fork_taken`).
    refuse_reopen: Option<PathBuf>,
    /// How many of the next hosts exit 75 before their hello, as a host
    /// does while another process holds the volume's lease.
    busy_starts: usize,
}

#[derive(Clone)]
struct Fake {
    world: Arc<Mutex<World>>,
    /// Every event, in order: `spawn host v`, `open <target>`, `close
    /// <target>`, `reconcile`, `eof host v`.
    log: Arc<watch::Sender<Vec<String>>>,
    /// Requests (as `describe` names them) whose replies wait until the
    /// test releases them.
    held: Arc<watch::Sender<BTreeSet<String>>>,
}

impl Fake {
    fn new() -> Self {
        Self {
            world: Arc::new(Mutex::new(World {
                eof_exit: Some(0),
                ..World::default()
            })),
            log: Arc::new(watch::channel(Vec::new()).0),
            held: Arc::new(watch::channel(BTreeSet::new()).0),
        }
    }

    /// Replies to `request` (as `describe` names it) wait from now on.
    fn hold(&self, request: &str) {
        self.held.send_modify(|h| {
            h.insert(request.to_owned());
        });
    }

    /// Replies to `request` are written again, the held ones at once.
    fn release(&self, request: &str) {
        self.held.send_modify(|h| {
            h.remove(request);
        });
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
        Request::ForkCreate { key, .. } => format!("fork_create {}", key.as_str()),
        Request::ForkEnd { key } => format!("fork_end {}", key.as_str()),
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
        let (eof_exit, hello_v, refuse, busy) = {
            let mut world = self.world.lock().unwrap();
            let busy = world.busy_starts > 0;
            world.busy_starts = world.busy_starts.saturating_sub(1);
            let refuse = if world.hosts.is_empty() {
                None
            } else {
                world.refuse_reopen.clone()
            };
            world.hosts.push(FakeHost {
                volume: volume.clone(),
                requests: requests.clone(),
                exit: exit_tx.clone(),
                kill: Some(kill),
            });
            (
                world.eof_exit,
                world.hello_v.unwrap_or(HOST_PROTOCOL),
                refuse,
                busy,
            )
        };
        Self::event(&self.log, format!("spawn host {volume}"));
        if busy {
            // No hello: its stream ends, and it exits 75.
            drop(theirs);
            exit_tx.send(Some(Exit::Code(75))).unwrap();
            return Ok(Spawned {
                pid: 2,
                exited,
                recent_log: Arc::default(),
                control: Some(ours),
            });
        }
        let log = self.log.clone();
        let world = self.world.clone();
        let held = self.held.clone();
        tokio::spawn(async move {
            let (read, write) = theirs.into_split();
            let write = Arc::new(AsyncMutex::new(write));
            let hello = format!(
                "{{\"v\":{hello_v},\"volume\":\"{name}\",\"id\":\"01234567-89ab-4def-8123-456789abcdef\"}}\n"
            );
            write
                .lock()
                .await
                .write_all(hello.as_bytes())
                .await
                .unwrap();
            let mut lines = BufReader::new(read).lines();
            loop {
                tokio::select! {
                    _ = &mut killed => {
                        // Held replies are never written: the stream ends.
                        let _ = write.lock().await.shutdown().await;
                        drop(lines);
                        exit_tx.send(Some(Exit::Signal(9))).unwrap();
                        return;
                    }
                    line = lines.next_line() => {
                        let Ok(Some(line)) = line else { break };
                        let Envelope { id, body: request } =
                            serde_json::from_str::<Envelope<Request>>(&line).unwrap();
                        Fake::event(&log, describe(&request));
                        let reply = match &request {
                            Request::Open(view) if refuse.as_ref() == Some(&view.at) => {
                                Reply::Refused(Refusal {
                                    kind: RefusalKind::ForkTaken,
                                    message: "refused: ForkTaken".into(),
                                })
                            }
                            Request::Open(_) => Reply::Opened {
                                tree: agentfs_contracts::tree::TreeId::Root,
                                stacked: false,
                            },
                            Request::Close { .. } => Reply::Closed,
                            Request::Reconcile => Reply::Reconciled { ended: 0 },
                            Request::ForkCreate { .. } | Request::ForkEnd { .. } => {
                                let refusal = world.lock().unwrap().fork_refusal;
                                match (refusal, &request) {
                                    (Some(kind), _) => Reply::Refused(Refusal {
                                        kind,
                                        message: format!("refused: {kind:?}"),
                                    }),
                                    (None, Request::ForkCreate { .. }) => Reply::Forked {
                                        tree: TreeId::Fork(1),
                                    },
                                    (None, _) => Reply::Ended,
                                }
                            }
                        };
                        let name = describe(&request);
                        requests.lock().unwrap().push(request);
                        let mut line = serde_json::to_string(&Envelope { id, body: reply }).unwrap();
                        line.push('\n');
                        let is_held = held.borrow().contains(&name);
                        let mut free = held.subscribe();
                        let writer = write.clone();
                        let replied = async move {
                            let _ = free.wait_for(|h| !h.contains(&name)).await;
                            writer.lock().await.write_all(line.as_bytes()).await.is_ok()
                        };
                        if is_held {
                            tokio::spawn(replied);
                        } else if !replied.await {
                            break;
                        }
                    }
                }
            }
            drop(lines);
            let _ = write.lock().await.shutdown().await;
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

fn key(text: &str) -> ForkKey {
    ForkKey::parse(text).unwrap()
}

fn automatic() -> ForkSettings {
    ForkSettings {
        landing: Landing::Automatic,
    }
}

/// A fork claim's CreateVolume with no host running: one is started for
/// the call, asked to create the fork under the claim's handle from the
/// right upstream, and stopped after it (EOF).
#[tokio::test(start_paused = true)]
async fn create_fork_starts_a_host_for_the_call_and_stops_it() {
    let n = Node::new();
    n.volume("v");
    let tree = n
        .sup
        .create_fork("v", TreeRef::Root, key("f"), automatic())
        .await
        .unwrap();
    assert_eq!(tree, TreeId::Fork(1));
    assert_eq!(
        n.fake.events(),
        ["spawn host v", "reconcile", "fork_create f", "eof host v"]
    );
    assert_eq!(
        n.fake.requests(0)[1],
        Request::ForkCreate {
            upstream: TreeRef::Root,
            key: key("f"),
            settings: automatic(),
        }
    );
    // A fork claim's fork as upstream goes as its key.
    n.sup
        .create_fork("v", TreeRef::Fork(key("f")), key("g"), automatic())
        .await
        .unwrap();
    assert_eq!(
        n.fake.requests(1)[1],
        Request::ForkCreate {
            upstream: TreeRef::Fork(key("f")),
            key: key("g"),
            settings: automatic(),
        }
    );
    // Refusals: an ended key is ALREADY_EXISTS, Trees' refusals
    // INVALID_ARGUMENT; the host is stopped after each.
    for (kind, code) in [
        (RefusalKind::KeyEnded, "AlreadyExists"),
        (RefusalKind::NestedNotBuilt, "InvalidArgument"),
        (RefusalKind::ApprovalNotBuilt, "InvalidArgument"),
        (RefusalKind::KeyUnknown, "FailedPrecondition"),
        (RefusalKind::Draining, "Unavailable"),
    ] {
        n.fake.world.lock().unwrap().fork_refusal = Some(kind);
        let err = n
            .sup
            .create_fork("v", TreeRef::Root, key("f"), automatic())
            .await
            .unwrap_err();
        assert!(format!("{err:?}").starts_with(code), "{kind:?}: {err:?}");
        assert_eq!(err.to_string(), format!("refused: {kind:?}"));
    }
    let hosts = n.fake.hosts();
    n.fake.until("eof host v", hosts).await;
}

/// With the volume's host running for a view, a fork create and end use
/// it and leave it running.
#[tokio::test(start_paused = true)]
async fn forks_on_a_running_host_reuse_it_and_leave_it_running() {
    let n = Node::new();
    n.volume("v");
    let a = n.target("a");
    n.sup.publish(n.view("v", "v", &a)).await.unwrap();
    n.sup
        .create_fork("v", TreeRef::Root, key("f"), automatic())
        .await
        .unwrap();
    n.sup.end_fork("v", key("f")).await.unwrap();
    assert_eq!(n.fake.hosts(), 1);
    assert_eq!(
        n.fake.events(),
        [
            "spawn host v".to_owned(),
            "reconcile".to_owned(),
            format!("open {}", a.display()),
            "fork_create f".to_owned(),
            "fork_end f".to_owned(),
        ]
    );
}

/// DeleteVolume of a fork claim is idempotent: a fork ended already, or
/// no fork under the key, is Ok; another refusal is not. Each call starts
/// the host when none runs and stops it after.
#[tokio::test(start_paused = true)]
async fn end_fork_maps_an_unknown_or_ended_key_to_ok() {
    let n = Node::new();
    n.volume("v");
    n.sup.end_fork("v", key("f")).await.unwrap();
    assert_eq!(
        n.fake.events(),
        ["spawn host v", "reconcile", "fork_end f", "eof host v"]
    );
    for kind in [RefusalKind::KeyUnknown, RefusalKind::KeyEnded] {
        n.fake.world.lock().unwrap().fork_refusal = Some(kind);
        n.sup.end_fork("v", key("f")).await.unwrap();
    }
    n.fake.world.lock().unwrap().fork_refusal = Some(RefusalKind::Stopped);
    assert!(matches!(
        n.sup.end_fork("v", key("f")).await.unwrap_err(),
        Error::Unavailable(_)
    ));
    assert_eq!(n.fake.hosts(), 4);
    n.fake.until("eof host v", 4).await;
}

/// A fork claim's view is on its source volume's host: it makes the
/// source in use, and is found as its own claim's.
#[tokio::test(start_paused = true)]
async fn a_fork_claims_view_puts_its_source_in_use() {
    let n = Node::new();
    n.volume("v");
    let b = n.target("b");
    let mut view = n.view("f", "v", &b);
    view.tree = TreeRef::Fork(key("f"));
    n.sup.publish(view).await.unwrap();
    assert_eq!(n.sup.in_use("v").await.map(|r| r.handle), Some("f".into()));
    assert_eq!(
        n.sup.claim_in_use("v", "f").await.map(|r| r.target),
        Some(b.clone())
    );
    assert_eq!(n.sup.claim_in_use("v", "v").await, None);
}

/// Under the paused clock, the runtime's idle point: every task has run
/// until it waits on something. Not a guess at a duration: time advances
/// only once nothing can run.
async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
}

fn spawn_unpublish(
    n: &Node,
    handle: &str,
    target: &Path,
) -> tokio::task::JoinHandle<Result<(), Error>> {
    let sup = n.sup.clone();
    let (handle, target) = (handle.to_owned(), target.to_path_buf());
    tokio::spawn(async move { sup.unpublish(&handle, &target).await })
}

fn spawn_publish(n: &Node, view: ViewSpec) -> tokio::task::JoinHandle<Result<(), Error>> {
    let sup = n.sup.clone();
    tokio::spawn(async move { sup.publish(view).await })
}

/// A close the host holds (a file held open in the view) stalls only its
/// own claim: a publish of another claim of the volume completes meanwhile,
/// and a second NodeUnpublish or NodePublish of the stalled claim is
/// ABORTED at once rather than queued. The close's end completes the
/// unpublish; the other claim's record keeps the host.
#[tokio::test(start_paused = true)]
async fn a_held_close_stalls_only_its_claim() {
    let n = Node::new();
    n.volume("v");
    let (a, b) = (n.target("a"), n.target("b"));
    n.sup.publish(n.view("a", "v", &a)).await.unwrap();
    let close_a = format!("close {}", a.display());
    n.fake.hold(&close_a);
    let unpublish = spawn_unpublish(&n, "a", &a);
    n.fake.until(&close_a, 1).await;
    for again in [
        n.sup.unpublish("a", &a).await,
        n.sup.publish(n.view("a", "v", &a)).await,
    ] {
        assert!(matches!(again, Err(Error::Aborted(_))), "{again:?}");
    }
    n.sup.publish(n.view("b", "v", &b)).await.unwrap();
    assert!(!unpublish.is_finished(), "a's close is still held");
    n.fake.release(&close_a);
    unpublish.await.unwrap().unwrap();
    assert!(!a.exists());
    assert_eq!(n.fake.hosts(), 1);
    assert!(
        !n.fake.events().contains(&"eof host v".to_owned()),
        "b's view keeps the host: {:?}",
        n.fake.events()
    );
    n.sup.unpublish("b", &b).await.unwrap();
    n.fake.until("eof host v", 1).await;
}

/// The last record's unpublish racing another claim's publish whose open
/// is in flight: the volume is not idle, so the host is not stopped, and
/// the publish's view ends on the running host with its record written.
#[tokio::test(start_paused = true)]
async fn a_publish_in_flight_keeps_its_host() {
    let n = Node::new();
    n.volume("v");
    let (a, b) = (n.target("a"), n.target("b"));
    n.sup.publish(n.view("a", "v", &a)).await.unwrap();
    let open_b = format!("open {}", b.display());
    n.fake.hold(&open_b);
    let publish = spawn_publish(&n, n.view("b", "v", &b));
    n.fake.until(&open_b, 1).await;
    n.sup.unpublish("a", &a).await.unwrap();
    assert!(
        !n.fake.events().contains(&"eof host v".to_owned()),
        "b's open in flight keeps the host: {:?}",
        n.fake.events()
    );
    n.fake.release(&open_b);
    publish.await.unwrap().unwrap();
    assert_eq!(n.fake.hosts(), 1);
    let records = records::load_published(&n.layout).0;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].target, b);
    assert!(!n.fake.events().contains(&"eof host v".to_owned()));
    n.sup.unpublish("b", &b).await.unwrap();
    n.fake.until("eof host v", 1).await;
}

/// A publish whose caller gives up while its open is with the host still
/// runs to the end: the record is written and the view stays open.
#[tokio::test(start_paused = true)]
async fn a_cancelled_publish_still_records_its_view() {
    let n = Node::new();
    n.volume("v");
    let a = n.target("a");
    let open_a = format!("open {}", a.display());
    n.fake.hold(&open_a);
    let publish = spawn_publish(&n, n.view("a", "v", &a));
    n.fake.until(&open_a, 1).await;
    publish.abort();
    assert!(publish.await.unwrap_err().is_cancelled());
    n.fake.release(&open_a);
    // The kubelet retries: ABORTED while the first publish runs, then Ok,
    // finding the record that publish wrote rather than opening again.
    let mut retries = 0;
    while let Err(e) = n.sup.publish(n.view("a", "v", &a)).await {
        assert!(matches!(e, Error::Aborted(_)), "{e:?}");
        retries += 1;
        assert!(retries < 1000, "the first publish never finished");
        settle().await;
    }
    let records = records::load_published(&n.layout).0;
    assert_eq!(records.len(), 1, "the record is written");
    assert_eq!(records[0].target, a);
    let events = n.fake.events();
    assert_eq!(
        events.iter().filter(|e| **e == open_a).count(),
        1,
        "one open: {events:?}"
    );
    assert!(!events.iter().any(|e| e.starts_with("close")), "{events:?}");
    assert!(!events.contains(&"eof host v".to_owned()));
}

/// The plugin shuts down while a spawned unpublish, whose caller gave up,
/// waits on a held close: the host's channel is closed anyway, so it sees
/// EOF at once, and the shutdown ends.
#[tokio::test(start_paused = true)]
async fn shutdown_closes_a_host_an_unpublish_still_waits_on() {
    let n = Node::new();
    n.volume("v");
    let a = n.target("a");
    n.sup.publish(n.view("a", "v", &a)).await.unwrap();
    let close_a = format!("close {}", a.display());
    n.fake.hold(&close_a);
    let unpublish = spawn_unpublish(&n, "a", &a);
    n.fake.until(&close_a, 1).await;
    unpublish.abort();
    assert!(unpublish.await.unwrap_err().is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(60), n.sup.shutdown())
        .await
        .expect("the shutdown ends");
    n.fake.until("eof host v", 1).await;
    settle().await;
    assert!(
        n.layout.published("a").exists(),
        "the view is left for the next plugin, its record kept"
    );
    assert!(n.table.detaches.lock().unwrap().is_empty());
}

/// A host that exits while a publish's open is in flight, and whose
/// restart refuses that view (a fork's writer its reconcile ended): the
/// publish fails with the refusal and leaves no record, rather than giving
/// the pod OK for a view no host serves.
#[tokio::test(start_paused = true)]
async fn a_publish_the_restarted_host_refuses_fails_and_records_nothing() {
    let n = Node::new();
    n.volume("v");
    let (b, c) = (n.target("b"), n.target("c"));
    n.sup.publish(n.view("c", "v", &c)).await.unwrap();
    n.fake.world.lock().unwrap().refuse_reopen = Some(b.clone());
    let (open_b, open_c) = (
        format!("open {}", b.display()),
        format!("open {}", c.display()),
    );
    n.fake.hold(&open_b);
    let publish = spawn_publish(&n, n.view("b", "v", &b));
    n.fake.until(&open_b, 1).await;
    // The restart's replay is held at c's open, under the start lock, so
    // b's record waits for it.
    n.fake.hold(&open_c);
    n.fake.exit(0, Exit::Code(1));
    n.fake.until(&open_c, 2).await;
    n.fake.release(&open_b);
    settle().await;
    assert!(
        !publish.is_finished(),
        "the record waits for the start lock"
    );
    n.fake.release(&open_c);
    let result = publish.await.unwrap();
    assert!(
        matches!(result, Err(Error::FailedPrecondition(_))),
        "{result:?}"
    );
    let on_new: Vec<String> = n.fake.requests(1).iter().map(describe).collect();
    assert!(
        on_new.contains(&open_b),
        "b was opened on the new host: {on_new:?}"
    );
    let records = records::load_published(&n.layout).0;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].handle, "c");
}

/// An unpublish racing a restarted host's replay: the replay holds the
/// start lock (an open the test holds) when the unpublish finds no host,
/// and has reopened the view by the time the unpublish gets the lock. The
/// unpublish then closes it on the new host, so no view is left open
/// without its record.
#[tokio::test(start_paused = true)]
async fn an_unpublish_racing_a_replay_closes_the_replayed_view() {
    let n = Node::new();
    n.volume("v");
    let (a, b) = (n.target("a"), n.target("b"));
    n.sup.publish(n.view("a", "v", &a)).await.unwrap();
    n.sup.publish(n.view("b", "v", &b)).await.unwrap();
    let open_b = format!("open {}", b.display());
    n.fake.hold(&open_b);
    n.fake.kill(0);
    // The restart's replay is held at b's open, under the start lock.
    n.fake.until(&open_b, 2).await;
    let unpublish = spawn_unpublish(&n, "a", &a);
    settle().await;
    assert!(!unpublish.is_finished(), "it waits for the start lock");
    n.fake.release(&open_b);
    unpublish.await.unwrap().unwrap();
    let on_new: Vec<String> = n.fake.requests(1).iter().map(describe).collect();
    assert!(
        on_new.contains(&format!("open {}", a.display())),
        "the replay reopened a: {on_new:?}"
    );
    assert_eq!(
        on_new.last(),
        Some(&format!("close {}", a.display())),
        "and the unpublish closed it there: {on_new:?}"
    );
    assert!(n.table.detaches.lock().unwrap().is_empty());
    let records = records::load_published(&n.layout).0;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].handle, "b");
    assert!(!a.exists());
}

/// A host that answers with another protocol's hello is refused with
/// FAILED_PRECONDITION naming both versions, and is not started again.
#[tokio::test(start_paused = true)]
async fn a_host_of_another_protocol_is_refused_and_not_retried() {
    let n = Node::new();
    n.volume("v");
    n.fake.world.lock().unwrap().hello_v = Some(HOST_PROTOCOL + 1);
    let a = n.target("a");
    let err = n.sup.publish(n.view("a", "v", &a)).await.unwrap_err();
    let Error::FailedPrecondition(message) = &err else {
        panic!("{err:?}");
    };
    assert!(
        message.contains(&format!("host protocol v{}", HOST_PROTOCOL + 1))
            && message.contains(&format!("this plugin v{HOST_PROTOCOL}")),
        "{message}"
    );
    assert_eq!(n.fake.hosts(), 1);
    n.fake.until("eof host v", 1).await;
    // At plugin start, a record of the volume: one try, no restart.
    records::store_published(
        &n.layout,
        &PublishedRecord {
            v: RECORD_VERSION,
            handle: "a".into(),
            volume: "v".into(),
            tree: TreeRef::Root,
            target: a.clone(),
            read_only: false,
            pod: PodContext::default(),
            label: ClientLabel::parse("csi-x").unwrap(),
        },
    )
    .unwrap();
    fs::create_dir_all(&a).unwrap();
    let report = n.sup.reconcile().await;
    assert_eq!(report.hosts, 0);
    tokio::time::sleep(std::time::Duration::from_secs(600)).await;
    assert_eq!(n.fake.hosts(), 2, "not tried again: {:?}", n.fake.events());
}

/// A start that exits 75 (another process holds the volume's lease) is
/// tried again once the lease is free, not after a timer: a crash's restart
/// and a reconcile's start alike.
#[tokio::test(start_paused = true)]
async fn a_busy_restart_waits_for_the_lease() {
    let max = Backoff::default().max;
    let n = Node::new();
    n.volume("v");
    let a = n.target("a");
    n.sup.publish(n.view("v", "v", &a)).await.unwrap();
    let lease = FileLock::try_take(&n.layout.state("v").join("actor.lock"), Mode::Exclusive)
        .unwrap()
        .expect("the test takes the lease");
    n.fake.world.lock().unwrap().busy_starts = 1;
    n.fake.kill(0);
    n.fake.until("spawn host v", 2).await;
    settle().await;
    tokio::time::advance(10 * max).await;
    settle().await;
    assert_eq!(
        n.fake.hosts(),
        2,
        "no timer starts it: {:?}",
        n.fake.events()
    );
    drop(lease);
    n.fake.until("spawn host v", 3).await;
    n.fake.until("reconcile", 2).await;
    let replayed: Vec<String> = n.fake.requests(2).iter().map(describe).collect();
    assert_eq!(
        replayed,
        [format!("open {}", a.display()), "reconcile".to_owned()]
    );
    n.sup.shutdown().await;

    // At plugin start, a recorded view whose host exits 75.
    let n = Node::new();
    n.volume("v");
    let a = n.target("a");
    fs::create_dir_all(&a).unwrap();
    records::store_published(
        &n.layout,
        &PublishedRecord {
            v: RECORD_VERSION,
            handle: "v".into(),
            volume: "v".into(),
            tree: TreeRef::Root,
            target: a.clone(),
            read_only: false,
            pod: PodContext::default(),
            label: ClientLabel::parse(&format!("csi-{}", target_key(&a))).unwrap(),
        },
    )
    .unwrap();
    let lease = FileLock::try_take(&n.layout.state("v").join("actor.lock"), Mode::Exclusive)
        .unwrap()
        .expect("the test takes the lease");
    n.fake.world.lock().unwrap().busy_starts = 1;
    let report = n.sup.reconcile().await;
    assert_eq!(report.hosts, 0);
    settle().await;
    tokio::time::advance(10 * max).await;
    settle().await;
    assert_eq!(
        n.fake.hosts(),
        1,
        "no timer starts it: {:?}",
        n.fake.events()
    );
    drop(lease);
    n.fake.until("spawn host v", 2).await;
    n.fake.until("reconcile", 1).await;
    let replayed: Vec<String> = n.fake.requests(1).iter().map(describe).collect();
    assert_eq!(
        replayed,
        [format!("open {}", a.display()), "reconcile".to_owned()]
    );
    n.sup.shutdown().await;
}
