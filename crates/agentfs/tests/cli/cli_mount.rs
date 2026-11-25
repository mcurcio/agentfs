//! `agentfs mount` refusals that need no FUSE device (docs/trees.md, "Run
//! it"): every one happens before anything is mounted, so these run
//! anywhere; real mounts are `launcher_mount.rs`'s, run as root by
//! ci/fuse-tests.sh. Also `agentfs probe` on paths with no agentfs mount,
//! and the recording commands refusing a tree volume.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use agentfs_contracts::pod::PodContext;
use agentfs_core::volume::StateDir;
use agentfs_engine::tree_actor::Recording;

struct Layout {
    dir: tempfile::TempDir,
}

impl Layout {
    fn new() -> Self {
        let l = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(l.p("mnt")).unwrap();
        l
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn state(&self) -> PathBuf {
        self.p("state")
    }

    fn init(&self, extra: &[&str]) {
        let mut args = vec!["init", "--volume", "demo"];
        args.extend_from_slice(extra);
        let out = run(&self.state(), &args);
        assert!(out.status.success(), "{}", stderr(&out));
    }

    /// Every file under the state area with its bytes.
    fn files(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.insert(path.clone(), std::fs::read(&path).unwrap_or_default());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(&self.state(), &mut out);
        out
    }
}

fn run(state: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
    for (name, _) in std::env::vars_os() {
        let n = name.to_string_lossy();
        if n.starts_with("OTEL_") || n.starts_with("AGENTFS_") {
            cmd.env_remove(name);
        }
    }
    cmd.arg("--state")
        .arg(state)
        .args(args)
        .env("AGENTFS_REPLICA", "test-0")
        .output()
        .unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// Each refusal is checked with `-- true` after the mount point: were the
/// refusal lost, the mount would serve and exit at once, never hang the test.
fn refused(out: &Output, needle: &str) {
    assert_eq!(out.status.code(), Some(1), "{}", stderr(out));
    assert!(stderr(out).contains(needle), "{}", stderr(out));
    assert!(!stderr(out).contains("mounted volume"), "{}", stderr(out));
}

/// `mount` recovers a volume and never creates one: nothing is written.
#[test]
fn mount_never_creates() {
    let l = Layout::new();
    let out = run(&l.state(), &["mount", s(&l.p("mnt")), "--", "true"]);
    refused(&out, "agentfs init --volume");
    assert!(!l.state().exists(), "the state area was made");
}

/// Another process holding the state area: exit 75, nothing mounted.
#[test]
fn a_held_state_area_exits_75() {
    let l = Layout::new();
    l.init(&[]);
    let held = Recording::open(&l.state(), &PodContext::default()).unwrap();
    let out = run(&l.state(), &["mount", s(&l.p("mnt")), "--", "true"]);
    assert_eq!(out.status.code(), Some(75), "{}", stderr(&out));
    assert!(stderr(&out).contains("busy"), "{}", stderr(&out));
    drop(held);
}

/// A recording state area is refused, pointing at the runbook that moves
/// it aside, and left as it was.
#[test]
fn a_recording_state_area_is_refused() {
    let l = Layout::new();
    agentfs_core::volume::init_with(&StateDir::new(l.state()), "demo", None, None).unwrap();
    let before = l.files();
    let out = run(&l.state(), &["mount", s(&l.p("mnt")), "--", "true"]);
    refused(&out, "docs/runbooks/upgrading-the-sidecar.md");
    assert_eq!(l.files(), before, "the recording state area changed");
}

/// An include the workload could reach through a view: exit 1 before the
/// volume is staged (staging recovers it, and so writes its journal).
#[test]
fn an_include_under_a_view_is_refused_before_staging() {
    let l = Layout::new();
    let include = l.p("outside.conf");
    std::fs::write(&include, "").unwrap();
    l.init(&["--include", s(&include)]);
    // The include, moved under the mount point after init wrote it.
    let inside = l.p("mnt/operator.conf");
    let config = std::fs::read_to_string(l.state().join("config")).unwrap();
    std::fs::write(
        l.state().join("config"),
        config.replace(s(&include), s(&inside)),
    )
    .unwrap();
    let before = l.files();
    let out = run(&l.state(), &["mount", s(&l.p("mnt")), "--", "true"]);
    refused(&out, "is under the mount point");
    assert_eq!(l.files(), before, "the volume was staged");
}

/// An include named by an included file is checked as a top-level one: one
/// under a view is refused, and the refusal comes before the mount point is
/// created.
#[test]
fn a_nested_include_under_a_view_is_refused_before_anything_is_created() {
    let l = Layout::new();
    let top = l.p("top.conf");
    let at = l.p("fresh");
    std::fs::write(
        &top,
        format!("[include]\n\tpath = {}/operator.conf\n", s(&at)),
    )
    .unwrap();
    l.init(&["--include", s(&top)]);
    let before = l.files();
    let out = run(&l.state(), &["mount", s(&at), "--", "true"]);
    refused(&out, "is under the mount point");
    assert!(
        !at.exists(),
        "the mount point was created before the refusal"
    );
    assert_eq!(l.files(), before, "the volume was staged");
}

/// A missing include named by an included file is warned of.
#[test]
fn a_missing_nested_include_is_warned_of() {
    let l = Layout::new();
    let top = l.p("top.conf");
    std::fs::write(&top, "[include]\n\tpath = nested-missing\n").unwrap();
    l.init(&["--include", s(&top)]);
    // A mount point with files stops the mount after the settings are read,
    // before anything is mounted.
    std::fs::write(l.p("mnt/left.txt"), "x").unwrap();
    let out = run(&l.state(), &["mount", s(&l.p("mnt")), "--", "true"]);
    refused(&out, "is not empty");
    assert!(
        stderr(&out).contains(&format!(
            "warning: operator settings: include {} is missing",
            l.p("nested-missing").display()
        )),
        "{}",
        stderr(&out)
    );
}

/// A mount point inside the state area is refused before anything is
/// created there, for the root's view and a fork's.
#[test]
fn a_mount_point_inside_the_state_area_is_refused() {
    let l = Layout::new();
    l.init(&[]);
    let before = l.files();
    let inside = l.state().join("inside");
    for args in [
        vec!["mount", s(&inside), "--", "true"],
        vec![
            "mount",
            s(&l.p("mnt")),
            "--fork-mount",
            s(&inside),
            "--",
            "true",
        ],
    ] {
        let out = run(&l.state(), &args);
        refused(&out, "is inside the state area");
        assert!(!inside.exists(), "{args:?}: the mount point was created");
        assert_eq!(l.files(), before, "{args:?}: the state area changed");
    }
}

/// A mount point holding files is refused: the mount would hide them.
#[test]
fn a_mount_point_with_files_is_refused() {
    let l = Layout::new();
    l.init(&[]);
    std::fs::write(l.p("mnt/left.txt"), "x").unwrap();
    let out = run(&l.state(), &["mount", s(&l.p("mnt")), "--", "true"]);
    refused(&out, "is not empty");
}

#[test]
fn probe_without_a_mount() {
    let l = Layout::new();
    let state = l.state();
    for args in [
        &["probe", s(&l.p("mnt"))][..],
        &["probe", "--allow-stale", s(&l.p("mnt"))],
    ] {
        let out = run(&state, args);
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "not mounted\n");
    }
    // A path that does not exist is not mounted either; no state area is
    // read or created.
    let out = run(&state, &["probe", s(&l.p("missing"))]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "not mounted\n");
    assert!(!state.exists());
    // The root filesystem is mounted, but not by agentfs.
    let out = run(&state, &["probe", "/"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("not an agentfs mount ("),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// With no stale mount, `hold` mounts as `refuse` does: here, no volume.
#[test]
fn hold_without_a_stale_mount_behaves_like_refuse() {
    let l = Layout::new();
    for mode in ["hold", "refuse"] {
        let out = run(
            &l.state(),
            &["mount", "--on-stale-mount", mode, s(&l.p("mnt"))],
        );
        refused(&out, "agentfs init --volume");
    }
}

#[test]
fn seal_mountpoint_needs_root() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let l = Layout::new();
    l.init(&[]);
    let out = run(&l.state(), &["mount", "--seal-mountpoint", s(&l.p("mnt"))]);
    refused(
        &out,
        "--seal-mountpoint (AGENTFS_SEAL_MOUNTPOINT) needs root",
    );
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(l.p("mnt")).unwrap().permissions().mode();
    assert_ne!(mode & 0o777, 0, "the mount point was sealed");
}

/// A recording command on a tree volume exits 1 naming what is not built,
/// before it reads or writes anything. Every recording command is driven:
/// one that stopped refusing would run the recording engine on a tree area.
#[test]
fn recording_commands_refuse_a_tree_volume() {
    let l = Layout::new();
    l.init(&[]);
    let before = l.files();
    let (b, m) = (l.p("b"), l.p("m"));
    let commands: &[(&str, &[&str])] = &[
        ("status", &["status"]),
        ("flush", &["flush"]),
        ("record", &["--root", s(&b), "record", "a"]),
        ("serve-history", &["serve-history"]),
        (
            "front",
            &["front", "--history-service", "127.0.0.1:1", s(&b), s(&m)],
        ),
        ("serve-git", &["serve-git"]),
        ("push", &["push"]),
        ("checkpoint", &["checkpoint"]),
        ("run", &["run"]),
        ("restore", &["restore", "a", "HEAD"]),
        ("revert", &["revert", "HEAD"]),
        ("patch", &["patch", "status"]),
        ("lfs", &["lfs", "fetch", "--all"]),
        ("fork", &["fork", "finish"]),
        ("start", &["start", "accept", "--tip", "0000"]),
    ];
    for (name, args) in commands {
        let out = run(&l.state(), args);
        assert_tree_refusal(&out, name, &l.state(), args);
    }
    assert_eq!(l.files(), before, "a recording command changed the volume");
    assert!(
        !b.exists() && !m.exists(),
        "a recording command wrote outside the state area"
    );
}

/// `fork create --source-state` names a tree volume as its source: refused
/// as `--state` is, before the new fork's state area is touched.
#[test]
fn fork_create_refuses_a_tree_source() {
    let l = Layout::new();
    l.init(&[]);
    let before = l.files();
    let fork = l.p("fork");
    let out = run(
        &fork,
        &[
            "--root",
            s(&l.p("b")),
            "fork",
            "create",
            "--source-state",
            s(&l.state()),
        ],
    );
    assert_tree_refusal(&out, "fork", &l.state(), &["fork", "create"]);
    assert!(!fork.exists(), "the fork's state area was written");
    assert_eq!(l.files(), before, "fork create changed the source volume");
}

/// The guard fails closed: a tree area whose identity file is corrupt is
/// still refused, on the format's marker alone.
#[test]
fn a_tree_area_with_a_corrupt_identity_is_refused() {
    let l = Layout::new();
    l.init(&[]);
    std::fs::write(l.state().join("volume"), b"not json").unwrap();
    let out = run(&l.state(), &["status"]);
    assert_tree_refusal(&out, "status", &l.state(), &["status"]);
}

fn assert_tree_refusal(out: &Output, name: &str, state: &Path, args: &[&str]) {
    let err = stderr(out);
    assert_eq!(out.status.code(), Some(1), "{args:?}: {err}");
    assert!(
        err.contains(&format!(
            "`agentfs {name}` runs on the recording engine only; {} is a tree volume",
            state.display()
        )),
        "{args:?}: {err}"
    );
    assert!(err.contains("not built yet"), "{args:?}: {err}");
}
