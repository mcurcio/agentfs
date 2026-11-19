//! `agentfs mount` refusals that need no FUSE device (docs/trees.md, "Run
//! it"): every one happens before anything is mounted, so these run
//! anywhere; real mounts are `launcher_mount.rs`'s, run as root by
//! ci/fuse-tests.sh. Also `agentfs probe` on paths with no agentfs mount,
//! and the recording commands refusing a tree volume.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use agentfs_contracts::pod::PodContext;
use agentfs_core::tree_actor::Recording;
use agentfs_core::volume::StateDir;

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

fn refused(out: &Output, needle: &str) {
    assert_eq!(out.status.code(), Some(1), "{}", stderr(out));
    assert!(stderr(out).contains(needle), "{}", stderr(out));
    assert!(!stderr(out).contains("mounted volume"), "{}", stderr(out));
}

/// `mount` recovers a volume and never creates one: nothing is written.
#[test]
fn mount_never_creates() {
    let l = Layout::new();
    let out = run(&l.state(), &["mount", s(&l.p("mnt"))]);
    refused(&out, "agentfs init --volume");
    assert!(!l.state().exists(), "the state area was made");
}

/// Another process holding the state area: exit 75, nothing mounted.
#[test]
fn a_held_state_area_exits_75() {
    let l = Layout::new();
    l.init(&[]);
    let held = Recording::open(&l.state(), &PodContext::default()).unwrap();
    let out = run(&l.state(), &["mount", s(&l.p("mnt"))]);
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
    let out = run(&l.state(), &["mount", s(&l.p("mnt"))]);
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
    let out = run(&l.state(), &["mount", s(&l.p("mnt"))]);
    refused(&out, "is under the mount point");
    assert_eq!(l.files(), before, "the volume was staged");
}

/// A mount point holding files is refused: the mount would hide them.
#[test]
fn a_mount_point_with_files_is_refused() {
    let l = Layout::new();
    l.init(&[]);
    std::fs::write(l.p("mnt/left.txt"), "x").unwrap();
    let out = run(&l.state(), &["mount", s(&l.p("mnt"))]);
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
/// before it reads or writes anything.
#[test]
fn recording_commands_refuse_a_tree_volume() {
    let l = Layout::new();
    l.init(&[]);
    let before = l.files();
    for args in [
        &["push"][..],
        &["status"],
        &["--root", "b", "record", "a"],
        &["run"],
    ] {
        let out = run(&l.state(), args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(
            err.contains(&format!(
                "`agentfs {}` runs on the recording engine only",
                args.iter().find(|a| !a.starts_with('-') && **a != "b").unwrap()
            )),
            "{args:?}: {err}"
        );
        assert!(err.contains("is a tree volume"), "{args:?}: {err}");
        assert!(err.contains("not built yet"), "{args:?}: {err}");
    }
    assert_eq!(l.files(), before, "a recording command changed the volume");
}
