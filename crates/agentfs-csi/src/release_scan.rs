//! The node plugin's release scan (design §14.2, F8 D1): it pushes every
//! worktree on this node that no pod stages, and finishes and publishes
//! the forks whose deletion asked for it (design §11.5).
//!
//! A worktree with unpushed commits and no history service (between pods,
//! after its claim was deleted, or kept by a static PV with `Retain`)
//! would otherwise sit on the node's disk with nothing pushing it. The
//! scan runs at plugin start, every `--release-scan-interval`, and at once
//! when DeleteVolume answers `Wait` ([`ReleaseScan::wake`]). For every
//! `<base>/<h>/csi.json` or `csi.retained.json` whose `<h>` has no staged
//! record on this node:
//!
//! - a fork with `finish_requested`, active or finished, gets `agentfs
//!   fork finish` (until its `--timeout`, [`launcher::FINISH_TIMEOUT`], so
//!   the lock is free between scans for an operator's `fork abandon`); a
//!   refused or abandoned fork is left to the operator and never pushed,
//!   and a published one is done;
//! - every other worktree whose journal holds records history lacks gets
//!   `agentfs flush` ([`launcher::flush_spec`]) first: a history service
//!   stopped with `SIGKILL` at NodeUnstage, or crash-looping then, leaves
//!   them, and nothing else would ever commit them. The push follows at
//!   the next scan, once the journal is committed;
//! - every other worktree gets `agentfs push`
//!   ([`launcher::push_spec`]) unless the last push of it exited 0 at its
//!   current branch head with the same remotes (a remote added since, or
//!   given another URL, is pushed to). That is kept in memory: a restarted
//!   plugin pushes each once more (one listing each), and DeleteVolume
//!   finding a remote that lacks the head makes the scan forget it
//!   ([`ReleaseScan::forget`]). A flush or push that fails backs off per
//!   worktree, from the interval, doubling up to the cap; each failed
//!   push raises `PushFailed` through the child's own alarms and
//!   OpenTelemetry export, which is what alarms while unpushed commits sit
//!   in an unstaged worktree. Exit 75 (another process holds the state
//!   area) is tried again at the next scan without backing off.
//!
//! One commit path and one push path: the scan never commits or pushes
//! itself, and DeleteVolume never does either; both rely on `agentfs
//! push` exiting 0 only when every remote succeeded.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{Notify, watch};
// Tokio's clock, which is the system's outside a paused test runtime: the
// backoff tests pause it, so a slow runner cannot outrun a delay.
use tokio::time::Instant;

use agentfs_core::fork::finish::{self, ForkState};
use agentfs_core::volume::{self as core_volume, StateDir};

use crate::launcher::{self, Exit, Launcher, PluginIdentity, Spawned};
use crate::layout::{Layout, validate_handle};
use crate::records::{self, VolumeRecord};
use crate::supervisor::Backoff;

/// What one [`ReleaseScan::scan`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanReport {
    /// `agentfs fork finish` started (forks whose deletion asked for it).
    pub finishing: Vec<String>,
    /// `agentfs flush` started (a journal history lacks).
    pub flushing: Vec<String>,
    /// `agentfs push` started.
    pub pushing: Vec<String>,
    /// A child from an earlier scan still runs.
    pub running: Vec<String>,
    /// The last push exited 0 at the current head and remotes: nothing
    /// to do.
    pub held: Vec<String>,
    pub backing_off: Vec<(String, Duration)>,
    pub unreadable: Vec<(PathBuf, String)>,
}

/// What one push is of: the branch head, to the remotes the settings
/// declare (`<name> <push URL>`, in settings order).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    head: String,
    remotes: Vec<String>,
}

/// What the scan does for one worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Work {
    Finish,
    /// Commit the journal's backlog; the push follows at a later scan.
    Flush,
    /// Push `target` (`None`: no history, nothing to push).
    Push {
        target: Option<Target>,
    },
}

/// One worktree's memory between scans.
#[derive(Debug, Default)]
struct Slot {
    /// The child an earlier scan started, and for what.
    child: Option<(Spawned, Work)>,
    /// What the last push that exited 0 pushed.
    held_at: Option<Target>,
    /// After a failed push: not before this instant; the delay that set it.
    backoff: Option<(Instant, Duration)>,
}

pub struct ReleaseScan {
    layout: Arc<Layout>,
    launcher: Arc<dyn Launcher>,
    identity: PluginIdentity,
    interval: Duration,
    backoff: Backoff,
    /// One slot per handle on this node's disk.
    slots: Mutex<HashMap<String, Slot>>,
    wake: Notify,
}

impl ReleaseScan {
    pub fn new(
        layout: Arc<Layout>,
        launcher: Arc<dyn Launcher>,
        identity: PluginIdentity,
        interval: Duration,
        backoff: Backoff,
    ) -> Self {
        Self {
            layout,
            launcher,
            identity,
            interval,
            backoff,
            slots: Mutex::new(HashMap::new()),
            wake: Notify::new(),
        }
    }

    /// One pass over `<base>/*/csi.json` and `csi.retained.json` (see the
    /// module docs).
    pub async fn scan(&self) -> ScanReport {
        let layout = self.layout.clone();
        let (wanted, unreadable) = tokio::task::spawn_blocking(move || to_release(&layout))
            .await
            .unwrap_or_else(|e| {
                let why = format!("the scan failed: {e}");
                (Vec::new(), vec![(PathBuf::new(), why)])
            });
        let mut report = ScanReport {
            unreadable,
            ..ScanReport::default()
        };
        let now = Instant::now();
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        // Staged or gone: its next scan starts afresh.
        slots
            .retain(|handle, slot| slot.child.is_some() || wanted.iter().any(|(h, _)| h == handle));
        for (handle, work) in wanted {
            let slot = slots.entry(handle.clone()).or_default();
            if let Some((child, _)) = &slot.child {
                match child.exit() {
                    None => {
                        report.running.push(handle);
                        continue;
                    }
                    Some(exit) => {
                        let (_, done) = slot.child.take().expect("checked above");
                        self.exited(&handle, slot, done, exit, now);
                    }
                }
            }
            match &work {
                Work::Finish => {}
                Work::Push { target: None } => {
                    report.held.push(handle);
                    continue;
                }
                Work::Push {
                    target: Some(target),
                } if slot.held_at.as_ref() == Some(target) => {
                    report.held.push(handle);
                    continue;
                }
                Work::Flush | Work::Push { .. } => {
                    if let Some((at, _)) = slot.backoff
                        && now < at
                    {
                        report.backing_off.push((handle, at - now));
                        continue;
                    }
                }
            }
            let spec = match &work {
                Work::Finish => launcher::fork_finish_spec(&self.layout, &handle, &self.identity),
                Work::Flush => launcher::flush_spec(&self.layout, &handle, &self.identity),
                Work::Push { .. } => launcher::push_spec(&self.layout, &handle, &self.identity),
            };
            match self.launcher.spawn(spec) {
                Ok(child) => {
                    match &work {
                        Work::Finish => {
                            eprintln!("fork {handle}: finishing and publishing it");
                            report.finishing.push(handle.clone());
                        }
                        Work::Flush => {
                            eprintln!(
                                "release scan: committing the journal of {handle}, which no pod \
                                 stages on this node"
                            );
                            report.flushing.push(handle.clone());
                        }
                        Work::Push { .. } => {
                            eprintln!(
                                "release scan: pushing {handle}, which no pod stages on this node"
                            );
                            report.pushing.push(handle.clone());
                        }
                    }
                    slot.child = Some((child, work));
                }
                Err(e) => report.unreadable.push((
                    self.layout.volume_dir(&handle),
                    format!("starting agentfs: {e}"),
                )),
            }
        }
        report
    }

    /// Bookkeeping for a child that exited: a push that exited 0 holds its
    /// target; 75 is retried at the next scan; any other exit of a flush
    /// or push backs off.
    fn exited(&self, handle: &str, slot: &mut Slot, work: Work, exit: Exit, now: Instant) {
        let what = match work {
            Work::Finish => return,
            Work::Flush => "commit of the journal",
            Work::Push { .. } => "push",
        };
        match exit {
            Exit::Code(0) => {
                if let Work::Push { target } = work {
                    slot.held_at = target;
                }
                slot.backoff = None;
            }
            Exit::Code(75) => {}
            other => {
                let cap = self.backoff.max.max(self.interval);
                let delay = match slot.backoff {
                    Some((_, prev)) => (prev * 2).min(cap),
                    None => self.interval.min(cap),
                };
                eprintln!(
                    "release scan: the {what} of {handle} failed ({other}); the next in {delay:?}"
                );
                slot.backoff = Some((now + delay, delay));
            }
        }
    }

    /// [`ReleaseScan::scan`] now, then every interval and at every
    /// [`ReleaseScan::wake`], until `stop` is true.
    pub async fn run(self: Arc<Self>, mut stop: watch::Receiver<bool>) {
        loop {
            let report = self.scan().await;
            for (path, why) in &report.unreadable {
                eprintln!("release scan: {}: {why}", path.display());
            }
            if *stop.borrow() {
                return;
            }
            tokio::select! {
                () = tokio::time::sleep(self.interval) => {}
                () = self.wake.notified() => {}
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        return;
                    }
                }
            }
        }
    }

    /// Runs a scan now (DeleteVolume answering Wait calls it). What was
    /// held stays held, and what backs off keeps its delay: a wake starts
    /// what is due, such as a fork's finisher, sooner.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// Forgets that a push of `handle` held its target, so the next scan
    /// pushes it again: DeleteVolume found a remote that lacks the head
    /// (its branch deleted by hand since, say).
    pub fn forget(&self, handle: &str) {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = slots.get_mut(handle) {
            slot.held_at = None;
        }
    }
}

/// Paths that could not be read, with why ([`ScanReport::unreadable`]).
type Unreadable = Vec<(PathBuf, String)>;

/// The worktrees to release, in name order: every `csi.json` or
/// `csi.retained.json` whose handle is not staged on this node, with what
/// to do; and what could not be read, with why.
fn to_release(layout: &Layout) -> (Vec<(String, Work)>, Unreadable) {
    let mut wanted = Vec::new();
    let mut unreadable = Vec::new();
    let Ok(entries) = std::fs::read_dir(&layout.base) else {
        return (wanted, unreadable);
    };
    let mut handles: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|h| validate_handle(h).is_ok())
        .collect();
    handles.sort();
    for handle in handles {
        if layout.staged_record(&handle).exists() {
            continue;
        }
        let mut record = None;
        for path in [layout.csi_json(&handle), layout.retained_json(&handle)] {
            match records::load::<VolumeRecord>(&path) {
                Ok(Some(found)) => {
                    record = Some(found);
                    break;
                }
                Ok(None) => {}
                Err(e) => {
                    unreadable.push((path, e.to_string()));
                    break;
                }
            }
        }
        let Some(record) = record else { continue };
        let state = StateDir::new(layout.state(&handle));
        let fork = match finish::status(&state) {
            Ok(fork) => fork,
            Err(e) => {
                unreadable.push((layout.state(&handle), e.to_string()));
                continue;
            }
        };
        let work = match fork.map(|status| status.state) {
            Some(ForkState::Active | ForkState::Finished) if record.finish_requested => {
                Work::Finish
            }
            // Published: done; refused or abandoned: the operator's, and
            // nothing pushes them.
            Some(ForkState::Published | ForkState::Refused | ForkState::Abandoned) => continue,
            // Any other worktree, a fork between pods too.
            _ => match core_volume::inspect(&state) {
                Ok((_, inspection)) if inspection.backlog > 0 => Work::Flush,
                Ok((history, inspection)) => Work::Push {
                    target: inspection.head.map(|head| Target {
                        head: head.to_string(),
                        remotes: history
                            .settings()
                            .remotes
                            .pushed()
                            .iter()
                            .map(|r| {
                                format!("{} {}", r.name, r.push_url.as_deref().unwrap_or(&r.url))
                            })
                            .collect(),
                    }),
                },
                Err(e) => {
                    unreadable.push((layout.state(&handle), e.to_string()));
                    continue;
                }
            },
        };
        wanted.push((handle, work));
    }
    (wanted, unreadable)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::io;

    use git2::{Oid, Repository};
    use rustix::process::Signal;

    use agentfs_core::fork::finish::{
        AbandonedMark, ForkMark, PublishedMark, RefusedMark, RefusedReason,
    };
    use agentfs_core::fork::{ForkIdentity, ForkLink, PushTarget};

    use super::*;
    use crate::launcher::ProcessSpec;
    use crate::params::{OnDelete, VolumeParams};

    /// Records what it starts; the children run until `exit` is sent.
    #[derive(Default)]
    struct Fake {
        started: Mutex<Vec<ProcessSpec>>,
        exits: Mutex<Vec<watch::Sender<Option<Exit>>>>,
    }

    impl Fake {
        fn exit(&self, n: usize, code: i32) {
            self.exits.lock().unwrap()[n]
                .send(Some(Exit::Code(code)))
                .unwrap();
        }

        fn labels(&self) -> Vec<String> {
            self.started
                .lock()
                .unwrap()
                .iter()
                .map(|s| s.label.clone())
                .collect()
        }
    }

    impl Launcher for Fake {
        fn spawn(&self, spec: ProcessSpec) -> io::Result<Spawned> {
            let (tx, rx) = watch::channel(None);
            self.exits.lock().unwrap().push(tx);
            let mut started = self.started.lock().unwrap();
            started.push(spec);
            Ok(Spawned {
                pid: started.len() as u32,
                exited: rx,
                recent_log: Arc::default(),
            })
        }

        fn signal(&self, _: u32, _: Signal) -> io::Result<()> {
            Ok(())
        }
    }

    fn layout(dir: &std::path::Path) -> Layout {
        Layout {
            base: dir.join("base"),
            node_state: dir.join("node"),
            mount_dir: dir.join("mnt"),
            profiles: dir.join("profiles"),
        }
    }

    fn record(handle: &str, requested: bool) -> VolumeRecord {
        VolumeRecord {
            params: VolumeParams {
                profile: "p".into(),
                volume: handle.into(),
                on_delete: OnDelete::Delete,
                start_from: None,
                workload_mounts: Vec::new(),
            },
            capacity_bytes: 0,
            created: 1,
            static_volume: false,
            finish_requested: requested,
        }
    }

    /// A volume `handle`; a fork when `fork`, asked to finish when
    /// `requested`.
    fn volume(layout: &Layout, handle: &str, fork: bool, requested: bool) {
        volume_linked(layout, handle, fork.then_some(ForkLink::Linked), requested);
    }

    /// [`volume`], a fork with `link` when there is one.
    fn volume_linked(layout: &Layout, handle: &str, link: Option<ForkLink>, requested: bool) {
        fs::create_dir_all(layout.backing(handle)).unwrap();
        let state = StateDir::new(layout.state(handle));
        if let Some(link) = link {
            let identity = ForkIdentity {
                fork: handle.into(),
                source: "src".into(),
                base: Oid::from_str(&"1".repeat(40)).unwrap(),
                push_target: PushTarget::Branch,
                link,
            };
            agentfs_core::volume::init_fork(&state, &identity, &[]).unwrap();
        } else {
            agentfs_core::volume::init(&state, handle, None).unwrap();
        }
        crate::volume::store(layout, handle, &record(handle, requested)).unwrap();
    }

    /// A commit on `handle`'s branch, as a history service makes one: it
    /// holds the journal through sequence 0.
    fn commit(layout: &Layout, handle: &str) -> Oid {
        let repo = Repository::open_bare(layout.state(handle).join("git")).unwrap();
        let sig = git2::Signature::now("t", "t@example.com").unwrap();
        let tree = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree).unwrap();
        let branch = format!("refs/heads/agentfs/{handle}");
        let parent = repo
            .refname_to_id(&branch)
            .ok()
            .map(|id| repo.find_commit(id).unwrap());
        let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
        let message = "write\n\nAgentfs-Seq: 0\nAgentfs-Trigger: close\n";
        repo.commit(Some(&branch), &sig, &sig, message, &tree, &parents)
            .unwrap()
    }

    /// Writes a finish commit as the fork's head and the published mark
    /// naming it.
    fn publish(layout: &Layout, handle: &str) {
        let repo = Repository::open_bare(layout.state(handle).join("git")).unwrap();
        let sig = git2::Signature::now("t", "t@example.com").unwrap();
        let tree = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree).unwrap();
        let message = format!(
            "finish fork {handle}\n\nAgentfs-Seq: 0\nAgentfs-Trigger: finish\n\
             Agentfs-Fork: {handle}\nAgentfs-Source: src\nAgentfs-Base: {}\n\
             Agentfs-Fork-Remote: origin\nAgentfs-Unrecorded: 0\n",
            "1".repeat(40)
        );
        let finish = repo
            .commit(
                Some(&format!("refs/heads/agentfs/{handle}")),
                &sig,
                &sig,
                &message,
                &tree,
                &[],
            )
            .unwrap();
        let state = StateDir::new(layout.state(handle));
        agentfs_core::fsutil::write_json_atomic(
            &state.fork_mark(),
            &ForkMark {
                published: Some(PublishedMark {
                    finish: finish.to_string(),
                    change: None,
                    remote: "origin".into(),
                    inbox: None,
                    parent_finished: None,
                    at_ms: 1,
                }),
                ..ForkMark::empty()
            },
        )
        .unwrap();
        assert_eq!(
            finish::status(&state).unwrap().unwrap().state,
            ForkState::Published
        );
    }

    /// Marks the published fork `handle` refused instead, or abandoned.
    fn refuse_or_abandon(layout: &Layout, handle: &str, abandon: bool) {
        publish(layout, handle);
        let state = StateDir::new(layout.state(handle));
        let mut mark: ForkMark =
            serde_json::from_slice(&fs::read(state.fork_mark()).unwrap()).unwrap();
        let finish = mark.published.take().unwrap().finish;
        if abandon {
            mark.abandoned = Some(AbandonedMark {
                reason: "not wanted".into(),
                at_ms: 2,
                uid: 0,
                state: "finished".into(),
            });
        } else {
            mark.refused = Some(RefusedMark {
                finish,
                remote: "origin".into(),
                reason: RefusedReason::AnotherWriter,
                message: "another writer".into(),
                at_ms: 2,
            });
        }
        agentfs_core::fsutil::write_json_atomic(&state.fork_mark(), &mark).unwrap();
        let wanted = if abandon {
            ForkState::Abandoned
        } else {
            ForkState::Refused
        };
        assert_eq!(finish::status(&state).unwrap().unwrap().state, wanted);
    }

    fn args(spec: &ProcessSpec) -> Vec<String> {
        spec.args
            .iter()
            .map(|a: &OsString| a.to_string_lossy().into_owned())
            .collect()
    }

    fn identity() -> PluginIdentity {
        PluginIdentity {
            namespace: "ns".into(),
            pod: "plugin-0".into(),
            node: "node-1".into(),
        }
    }

    fn scanner(layout: &Arc<Layout>, fake: &Arc<Fake>) -> ReleaseScan {
        ReleaseScan::new(
            layout.clone(),
            fake.clone(),
            identity(),
            Duration::from_millis(200),
            Backoff {
                initial: Duration::from_millis(50),
                max: Duration::from_millis(800),
                reset_after: Duration::from_secs(5),
            },
        )
    }

    /// A14: one finisher per fork asked to finish, none for a refused,
    /// abandoned or published one, and a new scan (a restarted plugin)
    /// starts it again.
    #[tokio::test]
    async fn one_finisher_per_fork_asked_to_finish() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Arc::new(layout(dir.path()));
        volume(&layout, "fork-a", true, true);
        volume(&layout, "done", true, true);
        publish(&layout, "done");
        volume(&layout, "refused", true, true);
        refuse_or_abandon(&layout, "refused", false);
        volume(&layout, "dropped", true, true);
        refuse_or_abandon(&layout, "dropped", true);
        volume(&layout, "staged", true, true);
        fs::create_dir_all(layout.staged_dir()).unwrap();
        fs::write(layout.staged_record("staged"), "{}").unwrap();
        // An unlinked fork is never finished: it is pushed like any
        // worktree, even when its deletion asked (design §11.5).
        volume_linked(&layout, "solo", Some(ForkLink::Unlinked), true);
        commit(&layout, "solo");

        let fake = Arc::new(Fake::default());
        let scan = scanner(&layout, &fake);
        let report = scan.scan().await;
        assert_eq!(report.finishing, ["fork-a"], "{report:?}");
        assert_eq!(report.pushing, ["solo"], "{report:?}");
        assert!(report.unreadable.is_empty(), "{report:?}");
        let started = fake.started.lock().unwrap().clone();
        assert_eq!(started.len(), 2);
        assert_eq!(started[1].label, "push solo");
        // Its push worked: held at its head from now on.
        fake.exit(1, 0);
        let state = layout.state("fork-a").display().to_string();
        let root = layout.backing("fork-a").display().to_string();
        assert_eq!(
            args(&started[0]),
            [
                "--state",
                state.as_str(),
                "--root",
                root.as_str(),
                "fork",
                "finish",
                "--timeout",
                "300000ms",
                "--namespace",
                "ns",
                "--pod",
                "plugin-0",
                "--node",
                "node-1",
            ]
        );
        // Still running: not started twice.
        let report = scan.scan().await;
        assert!(report.finishing.is_empty());
        assert_eq!(report.running, ["fork-a"]);
        // It exited (busy, say): the next scan starts it again.
        fake.exit(0, 75);
        assert_eq!(scan.scan().await.finishing, ["fork-a"]);
        // A restarted plugin: the request is on disk.
        let restarted = scanner(&layout, &fake);
        assert_eq!(restarted.scan().await.finishing, ["fork-a"]);
        assert!(
            fake.labels()
                .iter()
                .all(|l| l == "fork fork-a" || l == "push solo"),
            "a finisher started for a refused, abandoned or published fork: {:?}",
            fake.labels()
        );
    }

    /// F8 D1: every unstaged worktree is pushed until a push exits 0 at
    /// its head; a failed push backs off; exit 75 is retried at once.
    #[tokio::test(start_paused = true)]
    async fn it_pushes_every_unstaged_worktree_until_held() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Arc::new(layout(dir.path()));
        volume(&layout, "lost", false, false);
        commit(&layout, "lost");
        volume(&layout, "retained", false, false);
        commit(&layout, "retained");
        let retained = layout.csi_json("retained");
        fs::rename(&retained, layout.retained_json("retained")).unwrap();
        volume(&layout, "empty", false, false);
        volume(&layout, "staged", false, false);
        commit(&layout, "staged");
        fs::create_dir_all(layout.staged_dir()).unwrap();
        fs::write(layout.staged_record("staged"), "{}").unwrap();
        // A fork not asked to finish pushes like any worktree.
        volume(&layout, "fork-b", true, false);
        commit(&layout, "fork-b");
        // An abandoned fork is never pushed, asked to finish or not.
        volume(&layout, "abandoned", true, false);
        refuse_or_abandon(&layout, "abandoned", true);

        let fake = Arc::new(Fake::default());
        let scan = scanner(&layout, &fake);
        let report = scan.scan().await;
        assert_eq!(report.pushing, ["fork-b", "lost", "retained"], "{report:?}");
        assert_eq!(report.held, ["empty"], "no history: nothing to push");
        let started = fake.started.lock().unwrap().clone();
        let state = layout.state("lost").display().to_string();
        assert_eq!(args(&started[1]), ["--state", state.as_str(), "push"]);
        assert_eq!(started[1].label, "push lost");
        let env: Vec<(String, String)> = started[1]
            .env
            .iter()
            .map(|(k, v)| (k.to_string_lossy().into(), v.to_string_lossy().into()))
            .collect();
        assert_eq!(
            env,
            [
                ("AGENTFS_NAMESPACE".to_owned(), "ns".to_owned()),
                ("AGENTFS_POD".to_owned(), "plugin-0".to_owned()),
                ("AGENTFS_NODE".to_owned(), "node-1".to_owned()),
            ]
        );
        // fork-b pushed; lost failed; retained was busy.
        fake.exit(0, 0);
        fake.exit(1, 1);
        fake.exit(2, 75);
        let report = scan.scan().await;
        assert_eq!(report.held, ["empty", "fork-b"], "{report:?}");
        assert_eq!(report.pushing, ["retained"], "75: again at once");
        assert_eq!(report.backing_off.len(), 1, "{report:?}");
        assert_eq!(report.backing_off[0].0, "lost");
        // Backed off, then tried again; a new commit is pushed again.
        tokio::time::sleep(Duration::from_millis(250)).await;
        fake.exit(3, 0);
        let report = scan.scan().await;
        assert_eq!(report.pushing, ["lost"], "{report:?}");
        assert_eq!(report.held, ["empty", "fork-b", "retained"]);
        fake.exit(4, 1);
        let report = scan.scan().await;
        let (_, wait) = &report.backing_off[0];
        assert!(
            *wait > Duration::from_millis(250),
            "the delay doubles: {wait:?}"
        );
        commit(&layout, "retained");
        assert_eq!(scan.scan().await.pushing, ["retained"]);
        // A wake keeps what is held: nothing is pushed twice at a head.
        fake.exit(5, 0);
        assert!(scan.scan().await.held.contains(&"retained".to_owned()));
        scan.wake();
        let report = scan.scan().await;
        assert!(report.pushing.is_empty(), "{report:?}");
        assert_eq!(report.held, ["empty", "fork-b", "retained"]);
        assert!(
            !fake
                .labels()
                .iter()
                .any(|l| l.contains("staged") || l.contains("abandoned")),
            "a staged worktree is its history service's, and an abandoned fork is never \
             pushed: {:?}",
            fake.labels()
        );
    }

    /// A record journaled and never committed (a history service killed
    /// at NodeUnstage), as the record's call left it: sequence 1, past
    /// what [`commit`] holds.
    fn journal_a_record(layout: &Layout, handle: &str) {
        use agentfs_core::journal::Journal;
        use agentfs_core::paths::RepoPath;
        use agentfs_core::record::{Caller, Change, Draft, Trigger, Writer};
        let state = StateDir::new(layout.state(handle));
        let (journal, _) = Journal::open(state.journal(), 1, Box::new(|| Ok(()))).unwrap();
        journal
            .record(Draft {
                trigger: Trigger::Unlink,
                writer: Writer::agentfs(Caller {
                    uid: 0,
                    gid: 0,
                    pid: 1,
                }),
                time_ms: 0,
                changes: vec![Change::Remove {
                    path: RepoPath::new("gone").unwrap(),
                }],
                operation: None,
                atomic: false,
                origin: None,
            })
            .unwrap();
    }

    /// A journal history lacks is committed (`agentfs flush`, backing off
    /// when it fails) before anything is pushed, with or without a branch.
    #[tokio::test(start_paused = true)]
    async fn the_journal_is_committed_before_the_push() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Arc::new(layout(dir.path()));
        volume(&layout, "fresh", false, false);
        journal_a_record(&layout, "fresh");
        volume(&layout, "late", false, false);
        commit(&layout, "late");
        journal_a_record(&layout, "late");

        let fake = Arc::new(Fake::default());
        let scan = scanner(&layout, &fake);
        let report = scan.scan().await;
        assert_eq!(report.flushing, ["fresh", "late"], "{report:?}");
        assert!(
            report.pushing.is_empty() && report.held.is_empty(),
            "{report:?}"
        );
        let started = fake.started.lock().unwrap().clone();
        let state = layout.state("fresh").display().to_string();
        let root = layout.backing("fresh").display().to_string();
        assert_eq!(
            args(&started[0]),
            ["--state", state.as_str(), "--root", root.as_str(), "flush"]
        );
        assert_eq!(started[0].label, "flush fresh");
        assert_eq!(
            started[0].env.len(),
            3,
            "AGENTFS_NAMESPACE, AGENTFS_POD, AGENTFS_NODE"
        );
        // fresh's flush failed: it backs off; late's committed the journal.
        fake.exit(0, 1);
        fs::remove_file(StateDir::new(layout.state("late")).journal()).unwrap();
        commit(&layout, "late");
        fake.exit(1, 0);
        let report = scan.scan().await;
        assert_eq!(report.pushing, ["late"], "{report:?}");
        assert_eq!(report.backing_off.len(), 1, "{report:?}");
        assert_eq!(report.backing_off[0].0, "fresh");
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(scan.scan().await.flushing, ["fresh"]);
    }

    /// A push that held its head is pushed again once the remotes change
    /// (a remote added to the profile), and once DeleteVolume found a
    /// remote lacking the head ([`ReleaseScan::forget`]).
    #[tokio::test]
    async fn a_held_head_is_pushed_again_to_new_remotes_or_once_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Arc::new(layout(dir.path()));
        volume(&layout, "v", false, false);
        commit(&layout, "v");

        let fake = Arc::new(Fake::default());
        let scan = scanner(&layout, &fake);
        assert_eq!(scan.scan().await.pushing, ["v"]);
        fake.exit(0, 0);
        assert_eq!(scan.scan().await.held, ["v"], "no remote: held at once");
        let repo = Repository::open_bare(layout.state("v").join("git")).unwrap();
        let remote = dir.path().join("remote.git");
        repo.config()
            .unwrap()
            .set_str("remote.origin.url", &remote.display().to_string())
            .unwrap();
        assert_eq!(scan.scan().await.pushing, ["v"], "a remote added");
        fake.exit(1, 0);
        assert_eq!(scan.scan().await.held, ["v"]);
        scan.forget("v");
        assert_eq!(scan.scan().await.pushing, ["v"], "forgotten");
        fake.exit(2, 0);
        assert_eq!(scan.scan().await.held, ["v"]);
    }
}
