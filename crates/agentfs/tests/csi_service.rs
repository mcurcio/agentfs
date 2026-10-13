//! The CSI node plugin's supervisor running the real `agentfs
//! serve-history` (design §14.2): NodeStage brings the service up on its
//! node-local socket, fronts (`record --history-service`, which takes the
//! same path as a front mount) commit under their own pods' identities,
//! a `kill -9` of the service is recovered by the supervisor with the
//! journaled record committed under its original committer, and
//! NodeUnstage makes the final commit. No FUSE, no root.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentfs_csi::launcher::{AgentfsLauncher, PluginIdentity};
use agentfs_csi::layout::Layout;
use agentfs_csi::mounter::Mounter;
use agentfs_csi::params::{OnDelete, VolumeParams};
use agentfs_csi::supervisor::{Backoff, NodeConfig, StageRequest, Supervisor};
use agentfs_fuse::MountState;

/// Stage needs no mounts.
struct NoMounts;

impl Mounter for NoMounts {
    fn state(&self, _: &Path) -> io::Result<MountState> {
        Ok(MountState::NotMounted)
    }
    fn bind(&self, _: &Path, _: &Path, _: bool) -> io::Result<()> {
        Err(io::Error::other("no mounts here"))
    }
    fn detach(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }
}

fn record(layout: &Layout, pod: &str, path: &str) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
    for (name, _) in std::env::vars_os() {
        let n = name.to_string_lossy();
        if n.starts_with("OTEL_") || n.starts_with("AGENTFS_") {
            cmd.env_remove(name);
        }
    }
    cmd.arg("--state")
        .arg(layout.state("pvc-1"))
        .arg("--root")
        .arg(layout.backing("pvc-1"))
        .args(["record", "--history-service"])
        .arg(format!("unix:{}", layout.socket("pvc-1").display()))
        .args(["--history-wait", "20s"])
        .args(["--namespace", "team", "--pod", pod, "--node", "node-1"])
        .arg("--pod-uid")
        .arg(format!("{pod}-uid"))
        .args(["--replica", pod, path])
        .output()
        .unwrap()
}

/// `(committer name, Agentfs-Pod trailer)` of every commit, oldest first.
fn commits(layout: &Layout) -> Vec<(String, String)> {
    let Ok(repo) = git2::Repository::open_bare(layout.state("pvc-1").join("git")) else {
        return Vec::new();
    };
    let Ok(head) = repo
        .find_reference("refs/heads/agentfs/demo")
        .and_then(|r| r.peel_to_commit())
    else {
        return Vec::new();
    };
    let mut walk = repo.revwalk().unwrap();
    walk.push(head.id()).unwrap();
    walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)
        .unwrap();
    walk.filter_map(|id| {
        let commit = repo.find_commit(id.unwrap()).unwrap();
        let pod = commit
            .message()
            .ok()?
            .lines()
            .find_map(|l| l.strip_prefix("Agentfs-Pod: "))
            .map(str::to_owned)?;
        Some((commit.committer().name().ok()?.to_owned(), pod))
    })
    .collect()
}

async fn eventually(what: &str, wait: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + wait;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn record_async(layout: &Layout, pod: &'static str, path: &'static str) {
    let layout = layout.clone();
    let out = tokio::task::spawn_blocking(move || record(&layout, pod, path))
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "record as {pod}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_supervisor_runs_and_recovers_a_real_history_service() {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout {
        base: dir.path().join("base"),
        node_state: dir.path().join("node"),
        mount_dir: dir.path().join("mounts"),
        profiles: dir.path().join("profiles"),
    };
    fs::create_dir_all(&layout.profiles).unwrap();
    // A remote pushed at each start and then not for an hour: only the
    // final push at unstage can send the last commit.
    let remote = dir.path().join("remote.git");
    git2::Repository::init_bare(&remote).unwrap();
    // A commit window long enough to kill the service between a record
    // and its commit.
    fs::write(
        layout.profiles.join("test.config"),
        format!(
            "[agentfs]\n\tcommitWindow = 1500ms\n\tpushInterval = 1h\n[remote \"origin\"]\n\turl = {}\n",
            remote.display()
        ),
    )
    .unwrap();
    fs::write(layout.profiles.join("test.exclude"), "*.tmp\n").unwrap();
    let params = VolumeParams {
        profile: "test".into(),
        volume: "demo".into(),
        branch: None,
        on_delete: OnDelete::Delete,
        workload_mounts: Vec::new(),
    };
    agentfs_csi::volume::create(&layout, "pvc-1", &params, 0, false).unwrap();

    let mut cfg = NodeConfig::new("node-1", layout.clone());
    cfg.plugin = PluginIdentity {
        namespace: "agentfs-csi".into(),
        pod: "agentfs-csi-node-test".into(),
        node: "node-1".into(),
    };
    cfg.stage_wait = Duration::from_secs(30);
    cfg.unstage_grace = Duration::from_secs(20);
    cfg.backoff = Backoff {
        initial: Duration::from_millis(200),
        max: Duration::from_secs(2),
        reset_after: Duration::from_secs(60),
    };
    let sup = Supervisor::new(
        cfg,
        Arc::new(AgentfsLauncher {
            bin: env!("CARGO_BIN_EXE_agentfs").into(),
        }),
        Arc::new(NoMounts),
    );
    sup.stage(StageRequest {
        handle: "pvc-1".into(),
        volume_context: HashMap::new(),
    })
    .await
    .unwrap();
    assert!(layout.socket("pvc-1").exists());
    let first = sup.service_pid("pvc-1").unwrap();

    let backing = layout.backing("pvc-1");
    fs::write(backing.join("a"), "from a").unwrap();
    record_async(&layout, "pod-a", "a").await;
    fs::write(backing.join("b"), "from b").unwrap();
    record_async(&layout, "pod-b", "b").await;
    eventually("two commits", Duration::from_secs(20), || {
        commits(&layout).len() == 2
    })
    .await;
    assert_eq!(
        commits(&layout),
        [
            ("pod-a".to_owned(), "pod-a".to_owned()),
            ("pod-b".to_owned(), "pod-b".to_owned()),
        ]
    );

    // Journaled, not yet committed: then the service dies.
    fs::write(backing.join("c"), "from a again").unwrap();
    record_async(&layout, "pod-a", "c").await;
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(first as i32).unwrap(),
        rustix::process::Signal::KILL,
    )
    .unwrap();
    eventually("the restart", Duration::from_secs(20), || {
        sup.service_pid("pvc-1").is_some_and(|p| p != first)
    })
    .await;
    eventually("the recovered commit", Duration::from_secs(20), || {
        commits(&layout).len() == 3
    })
    .await;
    assert_eq!(
        commits(&layout)[2],
        ("pod-a".to_owned(), "pod-a".to_owned())
    );

    // The restarted service serves fronts.
    fs::write(backing.join("d"), "from b again").unwrap();
    record_async(&layout, "pod-b", "d").await;
    sup.unstage("pvc-1").await.unwrap();
    assert_eq!(commits(&layout).len(), 4, "the final commit");
    assert_eq!(commits(&layout)[3].0, "pod-b");
    assert!(!layout.socket("pvc-1").exists());
    assert!(!layout.staged_record("pvc-1").exists());
    assert!(sup.service_pid("pvc-1").is_none());
    // The final commit was pushed as the service stopped (design §14.2).
    let local = git2::Repository::open_bare(layout.state("pvc-1").join("git"))
        .unwrap()
        .refname_to_id("refs/heads/agentfs/demo")
        .unwrap();
    let pushed = git2::Repository::open_bare(&remote)
        .unwrap()
        .refname_to_id("refs/heads/agentfs/demo")
        .ok();
    assert_eq!(pushed, Some(local), "the final push");
}
