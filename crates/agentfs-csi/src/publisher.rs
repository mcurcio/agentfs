//! The node plugin's fork publisher (design §11.5, §14.2): it finishes
//! and publishes the forks whose deletion asked for it.
//!
//! DeleteVolume of a fork sets `finish_requested` in its `csi.json` and
//! answers `UNAVAILABLE` until the fork is published. The publisher scans
//! the base path at reconcile and then every `--fork-scan-interval`; for
//! each fork asked to finish, not staged on this node, whose status is
//! active or finished and that has no finisher running here, it starts
//! `agentfs fork finish`, which takes the fork's state lock, finishes it
//! once and publishes it with backoff until its fork remote holds it. A
//! finisher that exits 75 found the fork busy (another node or process
//! holds it); the next scan tries again. The request is on disk, so a
//! restarted plugin picks it up at its first scan.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::watch;

use agentfs_core::fork::finish::{self, ForkState};
use agentfs_core::volume::StateDir;

use crate::launcher::{self, Launcher, PluginIdentity, Spawned};
use crate::layout::{Layout, validate_handle};
use crate::records;

/// What one [`Publisher::scan`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanReport {
    /// Forks whose finisher this scan started.
    pub started: Vec<String>,
    /// Forks whose finisher is still running from an earlier scan.
    pub running: Vec<String>,
    /// Records or fork statuses that could not be read, with why.
    pub unreadable: Vec<(PathBuf, String)>,
}

pub struct Publisher {
    layout: Arc<Layout>,
    launcher: Arc<dyn Launcher>,
    identity: PluginIdentity,
    interval: Duration,
    /// The finisher started for each fork, until it exits.
    running: Mutex<HashMap<String, Spawned>>,
}

impl Publisher {
    pub fn new(
        layout: Arc<Layout>,
        launcher: Arc<dyn Launcher>,
        identity: PluginIdentity,
        interval: Duration,
    ) -> Self {
        Self {
            layout,
            launcher,
            identity,
            interval,
            running: Mutex::new(HashMap::new()),
        }
    }

    /// One pass over `<base>/*/csi.json` (see the module docs).
    pub async fn scan(&self) -> ScanReport {
        let layout = self.layout.clone();
        let (wanted, unreadable) = tokio::task::spawn_blocking(move || to_finish(&layout))
            .await
            .unwrap_or_else(|e| {
                let why = format!("the scan failed: {e}");
                (Vec::new(), vec![(PathBuf::new(), why)])
            });
        let mut report = ScanReport {
            unreadable,
            ..ScanReport::default()
        };
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        running.retain(|_, child| child.exit().is_none());
        for handle in wanted {
            if running.contains_key(&handle) {
                report.running.push(handle);
                continue;
            }
            let spec = launcher::fork_finish_spec(&self.layout, &handle, &self.identity);
            match self.launcher.spawn(spec) {
                Ok(child) => {
                    eprintln!("fork {handle}: finishing and publishing it");
                    running.insert(handle.clone(), child);
                    report.started.push(handle);
                }
                Err(e) => report.unreadable.push((
                    self.layout.volume_dir(&handle),
                    format!("starting agentfs fork finish: {e}"),
                )),
            }
        }
        report
    }

    /// [`Publisher::scan`] now, then every interval, until `stop` is true.
    pub async fn run(self: Arc<Self>, mut stop: watch::Receiver<bool>) {
        loop {
            let report = self.scan().await;
            for (path, why) in &report.unreadable {
                eprintln!("fork publisher: {}: {why}", path.display());
            }
            if *stop.borrow() {
                return;
            }
            tokio::select! {
                () = tokio::time::sleep(self.interval) => {}
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        return;
                    }
                }
            }
        }
    }
}

/// The forks to finish: asked to by DeleteVolume, not staged on this node
/// (its history service holds the fork), and active or finished.
fn to_finish(layout: &Layout) -> (Vec<String>, Vec<(PathBuf, String)>) {
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
        let path = layout.csi_json(&handle);
        let record = match records::load_volume(&path) {
            Ok(Some((record, _))) => record,
            Ok(None) => continue,
            Err(e) => {
                unreadable.push((path, e.to_string()));
                continue;
            }
        };
        if !record.finish_requested || layout.staged_record(&handle).exists() {
            continue;
        }
        match finish::status(&StateDir::new(layout.state(&handle))) {
            Ok(Some(status)) if matches!(status.state, ForkState::Active | ForkState::Finished) => {
                wanted.push(handle);
            }
            Ok(_) => {}
            Err(e) => unreadable.push((layout.state(&handle), e.to_string())),
        }
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

    use agentfs_core::fork::finish::{ForkMark, PublishedMark};
    use agentfs_core::fork::{ForkIdentity, PushTarget};

    use super::*;
    use crate::launcher::{Exit, ProcessSpec};
    use crate::params::{OnDelete, VolumeParams};
    use crate::records::VolumeRecord;

    /// Records what it starts; the children run until `exit` is sent.
    #[derive(Default)]
    struct Fake {
        started: Mutex<Vec<ProcessSpec>>,
        exits: Mutex<Vec<watch::Sender<Option<Exit>>>>,
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

    /// A volume `handle`; a fork when `fork`, asked to finish when
    /// `requested`.
    fn volume(layout: &Layout, handle: &str, fork: bool, requested: bool) {
        fs::create_dir_all(layout.backing(handle)).unwrap();
        let state = StateDir::new(layout.state(handle));
        if fork {
            let identity = ForkIdentity {
                fork: handle.into(),
                source: "src".into(),
                base: Oid::from_str(&"1".repeat(40)).unwrap(),
                push_target: PushTarget::Branch,
            };
            agentfs_core::volume::init_fork(&state, &identity, &[]).unwrap();
        } else {
            agentfs_core::volume::init(&state, handle, None).unwrap();
        }
        let record = VolumeRecord {
            params: VolumeParams {
                profile: "p".into(),
                volume: handle.into(),
                branch: None,
                on_delete: OnDelete::Delete,
                workload_mounts: Vec::new(),
            },
            capacity_bytes: 0,
            created: 1,
            published_node: None,
            static_volume: false,
            finish_requested: requested,
        };
        crate::volume::store(layout, handle, &record).unwrap();
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
                version: 1,
                published: Some(PublishedMark {
                    finish: finish.to_string(),
                    change: None,
                    remote: "origin".into(),
                    at_ms: 1,
                }),
            },
        )
        .unwrap();
        assert_eq!(
            finish::status(&state).unwrap().unwrap().state,
            ForkState::Published
        );
    }

    fn args(spec: &ProcessSpec) -> Vec<String> {
        spec.args
            .iter()
            .map(|a: &OsString| a.to_string_lossy().into_owned())
            .collect()
    }

    /// A14: one finisher per fork asked to finish, none for the rest, and
    /// a new publisher (a restarted plugin) starts it again.
    #[tokio::test]
    async fn one_finisher_per_fork_asked_to_finish() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Arc::new(layout(dir.path()));
        volume(&layout, "fork-a", true, true);
        volume(&layout, "fork-b", true, false);
        volume(&layout, "plain", false, true);
        volume(&layout, "done", true, true);
        publish(&layout, "done");
        volume(&layout, "staged", true, true);
        fs::create_dir_all(layout.staged_dir()).unwrap();
        fs::write(layout.staged_record("staged"), "{}").unwrap();

        let fake = Arc::new(Fake::default());
        let identity = PluginIdentity {
            namespace: "ns".into(),
            pod: "plugin-0".into(),
            node: "node-1".into(),
        };
        let publisher = Publisher::new(
            layout.clone(),
            fake.clone(),
            identity.clone(),
            Duration::from_secs(30),
        );
        let report = publisher.scan().await;
        assert_eq!(report.started, ["fork-a"], "{report:?}");
        assert!(report.unreadable.is_empty(), "{report:?}");
        let started = fake.started.lock().unwrap().clone();
        assert_eq!(started.len(), 1);
        let argv = args(&started[0]);
        let state = layout.state("fork-a").display().to_string();
        let root = layout.backing("fork-a").display().to_string();
        assert_eq!(
            argv,
            [
                "--state",
                state.as_str(),
                "--root",
                root.as_str(),
                "fork",
                "finish",
                "--namespace",
                "ns",
                "--pod",
                "plugin-0",
                "--node",
                "node-1",
            ]
        );
        // Still running: not started twice.
        let report = publisher.scan().await;
        assert!(report.started.is_empty());
        assert_eq!(report.running, ["fork-a"]);
        assert_eq!(fake.started.lock().unwrap().len(), 1);
        // It exited (busy, say): the next scan starts it again.
        fake.exits.lock().unwrap()[0]
            .send(Some(Exit::Code(75)))
            .unwrap();
        let report = publisher.scan().await;
        assert_eq!(report.started, ["fork-a"]);
        // A restarted plugin: the request is on disk.
        let restarted = Publisher::new(layout, fake.clone(), identity, Duration::from_secs(30));
        assert_eq!(restarted.scan().await.started, ["fork-a"]);
    }
}
