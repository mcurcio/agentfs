//! The node plugin's supervisor against a fake launcher (processes that
//! write their address, mount their source, crash on demand) and a fake
//! mounter (an in-memory mount table): staging, publishing, recovery,
//! teardown and reconcile (design §14.2).

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentfs_core::volume::StateDir;
use agentfs_csi::error::Error;
use agentfs_csi::launcher::{Exit, Launcher, ProcessSpec, Spawned, push_line};
use agentfs_csi::layout::{Layout, target_key};
use agentfs_csi::mounter::Mounter;
use agentfs_csi::params::{OnDelete, PodInfo, VolumeParams};
use agentfs_csi::records::{self, PublishedRecord, StagedRecord};
use agentfs_csi::supervisor::{Backoff, NodeConfig, PublishRequest, StageRequest, Supervisor};
use agentfs_fuse::MountState;
use rustix::fs::{FlockOperation, flock};
use rustix::process::Signal;
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceMode {
    /// Writes its address at once.
    Ready,
    /// Never writes it.
    Slow,
    /// Exits with this code at once.
    Fails(i32),
}

struct Proc {
    exit: watch::Sender<Option<Exit>>,
    /// A front's mount and where.
    mount: Option<(u64, PathBuf)>,
    /// A front's lease, held like the real one.
    lease: Option<File>,
    address: Option<PathBuf>,
}

#[derive(Default)]
struct World {
    next_pid: u32,
    next_mount: u64,
    procs: HashMap<u32, Proc>,
    /// Every mount's liveness.
    live: HashMap<u64, bool>,
    table: HashMap<PathBuf, Vec<u64>>,
    events: Vec<String>,
    spawns: Vec<ProcessSpec>,
    service_mode: Option<ServiceMode>,
    ignore_term: bool,
    /// Spawns of these label prefixes fail, once each (a failed fork).
    fail_spawns: Vec<&'static str>,
    /// The next front does not mount.
    hang_next_front: bool,
    /// Fronts do not mount.
    fronts_hang: bool,
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<World>>);

fn arg_after(spec: &ProcessSpec, flag: &str) -> Option<String> {
    let i = spec.args.iter().position(|a| a == flag)?;
    Some(spec.args.get(i + 1)?.to_string_lossy().into_owned())
}

impl Fake {
    fn world(&self) -> std::sync::MutexGuard<'_, World> {
        self.0.lock().unwrap()
    }

    fn events(&self) -> Vec<String> {
        self.world().events.clone()
    }

    fn spawns(&self, kind: &str) -> Vec<ProcessSpec> {
        self.world()
            .spawns
            .iter()
            .filter(|s| s.label.starts_with(kind))
            .cloned()
            .collect()
    }

    /// A process dies without cleaning up: its mount goes stale.
    fn crash(&self, pid: u32) {
        let mut w = self.world();
        w.events.push(format!("crash {pid}"));
        let proc = w.procs.remove(&pid).unwrap();
        if let Some((id, _)) = proc.mount {
            w.live.insert(id, false);
        }
        let _ = proc.exit.send(Some(Exit::Signal(9)));
    }

    fn stack(&self, path: &Path) -> Vec<bool> {
        let w = self.world();
        w.table
            .get(path)
            .map(|s| s.iter().map(|id| w.live[id]).collect())
            .unwrap_or_default()
    }

    fn exit(&self, pid: u32, exit: Exit, clean: bool) {
        let mut w = self.world();
        let Some(proc) = w.procs.remove(&pid) else {
            return;
        };
        if let Some((id, source)) = proc.mount {
            w.live.insert(id, false);
            if clean && let Some(stack) = w.table.get_mut(&source) {
                stack.retain(|m| *m != id);
            }
        }
        if clean && let Some(address) = proc.address {
            let _ = fs::remove_file(address);
        }
        let _ = proc.exit.send(Some(exit));
    }
}

impl Launcher for Fake {
    fn spawn(&self, spec: ProcessSpec) -> io::Result<Spawned> {
        let mut w = self.world();
        if let Some(i) = w.fail_spawns.iter().position(|p| spec.label.starts_with(p)) {
            w.fail_spawns.remove(i);
            w.events.push(format!("spawn failed {}", spec.label));
            return Err(io::Error::from_raw_os_error(11));
        }
        w.next_pid += 1;
        let pid = 1000 + w.next_pid;
        w.spawns.push(spec.clone());
        w.events.push(format!("spawn {pid} {}", spec.label));
        let (tx, rx) = watch::channel(None);
        let recent = agentfs_csi::launcher::RecentLog::default();
        push_line(&recent, format!("{} started", spec.label));
        let mut proc = Proc {
            exit: tx,
            mount: None,
            lease: None,
            address: None,
        };
        let state = StateDir::new(arg_after(&spec, "--state").unwrap());
        if spec.label.starts_with("service") {
            match w.service_mode.unwrap_or(ServiceMode::Ready) {
                ServiceMode::Ready => {
                    let address = state.history_dir().join("address");
                    fs::create_dir_all(state.history_dir())?;
                    fs::write(
                        &address,
                        format!("{}\n", arg_after(&spec, "--listen").unwrap()),
                    )?;
                    proc.address = Some(address);
                }
                ServiceMode::Slow => {}
                ServiceMode::Fails(code) => {
                    push_line(&recent, "error: the journal is unreadable".into());
                    let _ = proc.exit.send(Some(Exit::Code(code)));
                    return Ok(Spawned {
                        pid,
                        exited: rx,
                        recent_log: recent,
                    });
                }
            }
        } else {
            let preferred = arg_after(&spec, "--history-slot").map(|s| s.parse::<u16>().unwrap());
            fs::create_dir_all(state.fronts_dir())?;
            let order = preferred.into_iter().chain(1..=1023);
            for slot in order {
                let mut file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(state.front_lease(slot))?;
                if flock(&file, FlockOperation::NonBlockingLockExclusive).is_ok() {
                    file.set_len(0)?;
                    writeln!(file, "pid={pid} command=mount since=x")?;
                    proc.lease = Some(file);
                    break;
                }
            }
            let hang = w.fronts_hang || std::mem::take(&mut w.hang_next_front);
            if !hang {
                let source = PathBuf::from(spec.args.last().unwrap());
                w.next_mount += 1;
                let id = w.next_mount;
                w.live.insert(id, true);
                w.table.entry(source.clone()).or_default().push(id);
                proc.mount = Some((id, source));
            }
        }
        w.procs.insert(pid, proc);
        Ok(Spawned {
            pid,
            exited: rx,
            recent_log: recent,
        })
    }

    fn signal(&self, pid: u32, signal: Signal) -> io::Result<()> {
        let ignore = {
            let mut w = self.world();
            let name = match signal {
                Signal::TERM => "TERM",
                Signal::KILL => "KILL",
                _ => "other",
            };
            w.events.push(format!("signal {pid} {name}"));
            w.ignore_term
        };
        match signal {
            Signal::TERM if ignore => {}
            Signal::TERM => self.exit(pid, Exit::Code(0), true),
            _ => self.exit(pid, Exit::Signal(9), false),
        }
        Ok(())
    }
}

impl Mounter for Fake {
    fn state(&self, path: &Path) -> io::Result<MountState> {
        let w = self.world();
        Ok(match w.table.get(path).and_then(|s| s.last()) {
            None => MountState::NotMounted,
            Some(id) if w.live[id] => MountState::Live {
                source: "agentfs:v".into(),
            },
            Some(_) => MountState::Stale {
                source: "agentfs:v".into(),
            },
        })
    }

    fn bind(&self, source: &Path, target: &Path, readonly: bool) -> io::Result<()> {
        let mut w = self.world();
        let id = *w
            .table
            .get(source)
            .and_then(|s| s.last())
            .ok_or_else(|| io::Error::other("nothing mounted at the source"))?;
        w.table.entry(target.to_path_buf()).or_default().push(id);
        w.events.push(format!(
            "bind {} {} ro={readonly}",
            source.display(),
            target.display()
        ));
        Ok(())
    }

    fn detach(&self, path: &Path) -> io::Result<()> {
        let mut w = self.world();
        w.events.push(format!("detach {}", path.display()));
        if let Some(s) = w.table.get_mut(path) {
            s.pop();
        }
        Ok(())
    }
}

struct Node {
    _dir: tempfile::TempDir,
    root: PathBuf,
    layout: Layout,
    fake: Fake,
    sup: Arc<Supervisor>,
}

fn config(layout: &Layout) -> NodeConfig {
    let mut cfg = NodeConfig::new("node-1", layout.clone());
    cfg.stage_wait = Duration::from_millis(500);
    cfg.publish_wait = Duration::from_millis(500);
    cfg.unpublish_grace = Duration::from_millis(200);
    cfg.unstage_grace = Duration::from_millis(200);
    cfg.poll = Duration::from_millis(5);
    cfg.backoff = Backoff {
        initial: Duration::from_millis(50),
        max: Duration::from_millis(400),
        reset_after: Duration::from_secs(5),
    };
    cfg
}

fn params(volume: &str) -> VolumeParams {
    VolumeParams {
        profile: "p".into(),
        volume: volume.into(),
        branch: None,
        on_delete: OnDelete::Delete,
    }
}

impl Node {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let layout = Layout {
            base: root.join("base"),
            node_state: root.join("node"),
            mount_dir: root.join("mounts"),
            profiles: root.join("profiles"),
        };
        fs::create_dir_all(&layout.profiles).unwrap();
        fs::write(layout.profiles.join("p.config"), "").unwrap();
        for v in ["v", "w"] {
            agentfs_csi::volume::create(&layout, v, &params(v), 0, false).unwrap();
        }
        let fake = Fake::default();
        let sup = Supervisor::new(
            config(&layout),
            Arc::new(fake.clone()),
            Arc::new(fake.clone()),
        );
        Self {
            _dir: dir,
            root,
            layout,
            fake,
            sup,
        }
    }

    /// A second plugin on the same node state, as after a restart.
    fn restart(&self) -> Arc<Supervisor> {
        Supervisor::new(
            config(&self.layout),
            Arc::new(self.fake.clone()),
            Arc::new(self.fake.clone()),
        )
    }

    async fn stage(&self, handle: &str) -> Result<(), Error> {
        self.sup
            .stage(StageRequest {
                handle: handle.into(),
                volume_context: HashMap::new(),
            })
            .await
    }

    fn target(&self, name: &str) -> PathBuf {
        self.root.join("pods").join(name).join("mount")
    }

    async fn publish(&self, handle: &str, target: &Path, single_writer: bool) -> Result<(), Error> {
        self.sup
            .publish(PublishRequest {
                handle: handle.into(),
                target: target.to_path_buf(),
                readonly: false,
                single_writer,
                pod: Some(PodInfo {
                    namespace: "ns".into(),
                    name: "web-0".into(),
                    uid: "uid-0".into(),
                    service_account: "default".into(),
                }),
            })
            .await
    }

    fn record(&self, handle: &str, target: &Path) -> Option<PublishedRecord> {
        records::load(&self.layout.published_record(handle, &target_key(target))).unwrap()
    }
}

/// Polls `f` for up to 5 s.
async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn position(events: &[String], prefix: &str) -> usize {
    events
        .iter()
        .position(|e| e.starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix:?} in {events:#?}"))
}

#[tokio::test]
async fn stage_starts_one_service_and_waits_for_its_address() {
    let n = Node::new();
    n.stage("v").await.unwrap();
    n.stage("v").await.unwrap();
    assert_eq!(n.fake.spawns("service").len(), 1);
    let spec = &n.fake.spawns("service")[0];
    assert_eq!(
        arg_after(spec, "--listen").unwrap(),
        format!("unix:{}", n.layout.socket("v").display())
    );
    let staged: StagedRecord = records::load(&n.layout.staged_record("v"))
        .unwrap()
        .unwrap();
    assert_eq!(staged.params, params("v"));
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(n.layout.run_dir("v"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
    assert!(n.sup.service_pid("v").is_some());

    // An unknown volume without a profile in its context.
    let err = n
        .sup
        .stage(StageRequest {
            handle: "nope".into(),
            volume_context: HashMap::new(),
        })
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");
}

#[tokio::test]
async fn a_slow_service_is_unavailable_and_kept_a_failing_one_is_refused() {
    let n = Node::new();
    n.fake.world().service_mode = Some(ServiceMode::Slow);
    let err = n.stage("v").await.unwrap_err();
    assert!(matches!(err, Error::Unavailable(_)), "{err:?}");
    let pid = n.sup.service_pid("v").unwrap();
    // The kubelet's retry waits for the same service.
    let err = n.stage("v").await.unwrap_err();
    assert!(matches!(err, Error::Unavailable(_)), "{err:?}");
    assert_eq!(n.sup.service_pid("v"), Some(pid));
    assert_eq!(n.fake.spawns("service").len(), 1);

    n.fake.world().service_mode = Some(ServiceMode::Fails(1));
    let err = n.stage("w").await.unwrap_err();
    let Error::FailedPrecondition(msg) = err else {
        panic!("{err:?}")
    };
    assert!(msg.contains("journal is unreadable"), "{msg}");
    assert!(n.sup.service_pid("w").is_none());
    assert!(!n.layout.staged_record("w").exists());
}

#[tokio::test]
async fn publish_binds_a_live_front_and_records_its_slot() {
    let n = Node::new();
    n.stage("v").await.unwrap();
    let target = n.target("a");
    n.publish("v", &target, false).await.unwrap();
    let events = n.fake.events();
    let source = n.layout.front_source("v", &target_key(&target));
    assert!(
        position(&events, "spawn 1002 front") < position(&events, "bind"),
        "{events:#?}"
    );
    assert_eq!(n.fake.stack(&target), [true]);
    let rec = n.record("v", &target).unwrap();
    assert_eq!(rec.slot, Some(1));
    assert_eq!(rec.front_pid, n.sup.front_pid(&target));
    assert_eq!(rec.pod.as_ref().unwrap().name, "web-0");
    let spec = &n.fake.spawns("front")[0];
    assert_eq!(arg_after(spec, "--pod").unwrap(), "web-0");
    assert_eq!(arg_after(spec, "--pod-uid").unwrap(), "uid-0");
    assert_eq!(arg_after(spec, "--replica").unwrap(), "web-0");
    assert_eq!(arg_after(spec, "--node").unwrap(), "node-1");
    assert_eq!(PathBuf::from(spec.args.last().unwrap()), source);

    // Idempotent.
    n.publish("v", &target, false).await.unwrap();
    assert_eq!(n.fake.spawns("front").len(), 1);
    assert_eq!(n.fake.stack(&target), [true]);

    // A second pod gets its own front and slot.
    let b = n.target("b");
    n.publish("v", &b, false).await.unwrap();
    assert_eq!(n.record("v", &b).unwrap().slot, Some(2));

    // Not staged.
    let err = n.publish("w", &n.target("c"), false).await.unwrap_err();
    assert!(matches!(err, Error::FailedPrecondition(_)), "{err:?}");
}

#[tokio::test]
async fn a_single_writer_volume_is_published_once() {
    let n = Node::new();
    n.stage("v").await.unwrap();
    n.publish("v", &n.target("a"), true).await.unwrap();
    let err = n.publish("v", &n.target("b"), true).await.unwrap_err();
    assert!(matches!(err, Error::FailedPrecondition(_)), "{err:?}");
    let err = n.publish("v", &n.target("b"), false).await.unwrap_err();
    assert!(matches!(err, Error::FailedPrecondition(_)), "{err:?}");
}

#[tokio::test]
async fn a_front_that_never_mounts_is_unavailable() {
    let n = Node::new();
    n.stage("v").await.unwrap();
    n.fake.world().fronts_hang = true;
    let err = n.publish("v", &n.target("a"), false).await.unwrap_err();
    assert!(matches!(err, Error::Unavailable(_)), "{err:?}");
    assert!(n.sup.front_pid(&n.target("a")).is_some());
}

#[tokio::test]
async fn a_crashed_front_is_restarted_in_its_slot_and_stacked_over_the_target() {
    let n = Node::new();
    n.stage("v").await.unwrap();
    let target = n.target("a");
    n.publish("v", &n.target("other"), false).await.unwrap();
    n.publish("v", &target, false).await.unwrap();
    let old = n.record("v", &target).unwrap();
    assert_eq!(old.slot, Some(2));
    // Slot 1 frees up, so a plain lease would take it.
    n.sup.unpublish("v", &n.target("other")).await.unwrap();

    n.fake.crash(old.front_pid.unwrap());
    assert_eq!(n.fake.stack(&target), [false]);
    eventually("the front's recovery", || {
        n.fake.stack(&target) == [false, true]
    })
    .await;
    let fronts = n.fake.spawns("front");
    let respawn = fronts.last().unwrap();
    assert_eq!(arg_after(respawn, "--history-slot").as_deref(), Some("2"));
    assert_eq!(arg_after(respawn, "--pod").as_deref(), Some("web-0"));
    assert_eq!(arg_after(respawn, "--pod-uid").as_deref(), Some("uid-0"));
    let events = n.fake.events();
    let crash = position(&events, "crash");
    let source = n.layout.front_source("v", &target_key(&target));
    let after: Vec<&String> = events[crash..].iter().collect();
    // The stale source is detached; the target is not.
    assert!(
        after
            .iter()
            .any(|e| **e == format!("detach {}", source.display())),
        "{after:#?}"
    );
    assert!(
        !after
            .iter()
            .any(|e| **e == format!("detach {}", target.display())),
        "{after:#?}"
    );
    eventually("the record's update", || {
        n.record("v", &target).unwrap().front_pid != old.front_pid
    })
    .await;
    let new = n.record("v", &target).unwrap();
    assert_eq!(new.slot, Some(2));
    assert_eq!(new.front_pid, n.sup.front_pid(&target));

    // Unpublish peels both layers.
    n.sup.unpublish("v", &target).await.unwrap();
    assert!(n.fake.stack(&target).is_empty());
    assert!(!target.exists());
}

#[tokio::test]
async fn a_front_whose_target_is_gone_is_forgotten() {
    let n = Node::new();
    n.stage("v").await.unwrap();
    let target = n.target("a");
    n.publish("v", &target, false).await.unwrap();
    let pid = n.sup.front_pid(&target).unwrap();
    // The kubelet removed the pod's directory.
    n.fake.world().table.remove(&target);
    fs::remove_dir_all(target.parent().unwrap()).unwrap();
    n.fake.crash(pid);
    eventually("the record's removal", || n.record("v", &target).is_none()).await;
    assert_eq!(n.fake.spawns("front").len(), 1);
    // Nothing published: unstage works.
    n.sup.unstage("v").await.unwrap();
}

#[tokio::test]
async fn a_crashed_service_is_restarted_with_backoff() {
    let n = Node::new();
    n.stage("v").await.unwrap();
    let first = n.sup.service_pid("v").unwrap();
    let crashed = Instant::now();
    n.fake.crash(first);
    eventually("the first restart", || {
        n.sup.service_pid("v").is_some_and(|p| p != first)
    })
    .await;
    let one = crashed.elapsed();
    assert!(one >= Duration::from_millis(50), "{one:?}");
    let second = n.sup.service_pid("v").unwrap();
    let crashed = Instant::now();
    n.fake.crash(second);
    eventually("the second restart", || {
        n.sup.service_pid("v").is_some_and(|p| p != second)
    })
    .await;
    let two = crashed.elapsed();
    assert!(two >= Duration::from_millis(100), "doubled: {two:?}");
    assert_eq!(n.fake.spawns("service").len(), 3);
    // Ready again.
    n.stage("v").await.unwrap();
}

#[tokio::test]
async fn unpublish_detaches_the_target_before_stopping_the_front() {
    let n = Node::new();
    n.stage("v").await.unwrap();
    let target = n.target("a");
    n.publish("v", &target, false).await.unwrap();
    let pid = n.sup.front_pid(&target).unwrap();
    n.fake.world().ignore_term = true;
    n.sup.unpublish("v", &target).await.unwrap();
    let events = n.fake.events();
    let detach = position(&events, &format!("detach {}", target.display()));
    let term = position(&events, &format!("signal {pid} TERM"));
    let kill = position(&events, &format!("signal {pid} KILL"));
    assert!(detach < term && term < kill, "{events:#?}");
    assert!(!target.exists());
    assert!(n.record("v", &target).is_none());
    // Idempotent, and unknown targets are fine.
    n.sup.unpublish("v", &target).await.unwrap();
    n.sup.unpublish("v", &n.target("never")).await.unwrap();
    assert_eq!(n.fake.spawns("front").len(), 1);
}

#[tokio::test]
async fn unstage_is_refused_while_published() {
    let n = Node::new();
    n.stage("v").await.unwrap();
    let target = n.target("a");
    n.publish("v", &target, false).await.unwrap();
    let err = n.sup.unstage("v").await.unwrap_err();
    assert!(matches!(err, Error::FailedPrecondition(_)), "{err:?}");
    n.sup.unpublish("v", &target).await.unwrap();
    let pid = n.sup.service_pid("v").unwrap();
    n.sup.unstage("v").await.unwrap();
    assert!(n.fake.events().contains(&format!("signal {pid} TERM")));
    assert!(n.sup.service_pid("v").is_none());
    assert!(!n.layout.staged_record("v").exists());
    assert!(!n.layout.run_dir("v").exists());
    // No restart of a stopped service.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(n.fake.spawns("service").len(), 1);
    n.sup.unstage("v").await.unwrap();
}

#[tokio::test]
async fn reconcile_rebuilds_services_and_fronts_from_the_records() {
    let n = Node::new();
    n.stage("v").await.unwrap();
    let live = n.target("live");
    let rebooted = n.target("rebooted");
    let gone = n.target("gone");
    for t in [&live, &rebooted, &gone] {
        n.publish("v", t, false).await.unwrap();
    }
    let slot = n.record("v", &live).unwrap().slot;
    // The plugin dies: its children with it, without cleaning up.
    let pids: Vec<u32> = n.fake.world().procs.keys().copied().collect();
    let old = n.sup.clone();
    old.forget();
    for pid in pids {
        n.fake.crash(pid);
    }
    n.fake.world().table.remove(&rebooted);
    n.fake.world().table.remove(&gone);
    fs::remove_dir_all(gone.parent().unwrap()).unwrap();

    let sup = n.restart();
    let report = sup.reconcile().await.unwrap();
    assert_eq!(report.services, 1);
    assert_eq!(report.recovering, 1);
    assert_eq!(report.dropped, 2);
    assert!(n.layout.node_marker("node-1").exists());
    eventually("the recovered front", || {
        n.fake.stack(&live) == [false, true]
    })
    .await;
    let respawn = n.fake.spawns("front").last().cloned().unwrap();
    assert_eq!(
        arg_after(&respawn, "--history-slot"),
        slot.map(|s| s.to_string())
    );
    assert!(n.record("v", &rebooted).is_none());
    assert!(n.record("v", &gone).is_none());
    sup.shutdown().await;
}

#[tokio::test]
async fn a_service_restart_that_cannot_spawn_is_tried_again() {
    let n = Node::new();
    n.stage("v").await.unwrap();
    let first = n.sup.service_pid("v").unwrap();
    n.fake.world().fail_spawns = vec!["service", "service"];
    n.fake.crash(first);
    eventually("the restart after two failed spawns", || {
        n.sup.service_pid("v").is_some_and(|p| p != first)
    })
    .await;
    let failed = n
        .fake
        .events()
        .iter()
        .filter(|e| e.starts_with("spawn failed service"))
        .count();
    assert_eq!(failed, 2);
    n.stage("v").await.unwrap();
}

#[tokio::test]
async fn a_front_restart_that_cannot_spawn_is_tried_again() {
    let n = Node::new();
    n.stage("v").await.unwrap();
    let target = n.target("a");
    n.publish("v", &target, false).await.unwrap();
    let pid = n.sup.front_pid(&target).unwrap();
    n.fake.world().fail_spawns = vec!["front"];
    n.fake.crash(pid);
    eventually("the front's recovery after a failed spawn", || {
        n.fake.stack(&target) == [false, true]
    })
    .await;
    assert!(
        n.fake
            .events()
            .iter()
            .any(|e| e.starts_with("spawn failed front"))
    );
    eventually("the record's update", || {
        n.record("v", &target).unwrap().front_pid != Some(pid)
    })
    .await;
}

#[tokio::test]
async fn reconcile_keeps_targets_whose_service_cannot_start_yet() {
    let n = Node::new();
    n.stage("v").await.unwrap();
    let target = n.target("a");
    n.publish("v", &target, false).await.unwrap();
    let pids: Vec<u32> = n.fake.world().procs.keys().copied().collect();
    n.sup.forget();
    for pid in pids {
        n.fake.crash(pid);
    }
    n.fake.world().fail_spawns = vec!["service"];
    let sup = n.restart();
    let report = sup.reconcile().await.unwrap();
    assert_eq!(report.services, 0);
    assert_eq!(report.recovering, 1);
    assert_eq!(report.dropped, 0);
    assert!(n.record("v", &target).is_some());
    eventually("the service's retry", || sup.service_pid("v").is_some()).await;
    eventually("the recovered front", || {
        n.fake.stack(&target) == [false, true]
    })
    .await;
    sup.shutdown().await;
}

#[tokio::test]
async fn a_pending_publish_does_not_hold_up_another_pods_recovery() {
    let n = Node::new();
    n.stage("v").await.unwrap();
    let a = n.target("a");
    n.publish("v", &a, false).await.unwrap();
    let pid = n.sup.front_pid(&a).unwrap();
    // b's front never mounts: its publish waits publish_wait (500 ms).
    n.fake.world().hang_next_front = true;
    let pending = {
        let sup = n.sup.clone();
        let b = n.target("b");
        tokio::spawn(async move {
            sup.publish(PublishRequest {
                handle: "v".into(),
                target: b,
                readonly: false,
                single_writer: false,
                pod: None,
            })
            .await
        })
    };
    eventually("b's front", || n.fake.spawns("front").len() == 2).await;
    n.fake.crash(pid);
    eventually("a's recovery", || n.fake.stack(&a) == [false, true]).await;
    assert!(
        !pending.is_finished(),
        "a recovered only after b's publish gave up"
    );
    let err = pending.await.unwrap().unwrap_err();
    assert!(matches!(err, Error::Unavailable(_)), "{err:?}");
}

#[tokio::test]
async fn publishing_a_target_again_with_other_flags_is_already_exists() {
    let n = Node::new();
    n.stage("v").await.unwrap();
    let target = n.target("a");
    n.publish("v", &target, false).await.unwrap();
    let again = |readonly: bool, single_writer: bool| PublishRequest {
        handle: "v".into(),
        target: target.clone(),
        readonly,
        single_writer,
        pod: None,
    };
    let err = n.sup.publish(again(true, false)).await.unwrap_err();
    assert!(matches!(err, Error::AlreadyExists(_)), "{err:?}");
    let err = n.sup.publish(again(false, true)).await.unwrap_err();
    assert!(matches!(err, Error::AlreadyExists(_)), "{err:?}");
    // The same flags are idempotent.
    n.sup.publish(again(false, false)).await.unwrap();
    assert_eq!(n.fake.spawns("front").len(), 1);
}
