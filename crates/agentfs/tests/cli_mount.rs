//! `agentfs mount` refusals that need no FUSE device (design §1.1, §2.3,
//! §18 test 5): every one happens before anything is mounted, so these run
//! anywhere. Real mounts are tested in `mount.rs`, in CI. Also `agentfs
//! probe` on paths with no agentfs mount (design §17.1).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use agentfs_core::lock::StateLock;
use agentfs_core::volume::StateDir;

struct Layout {
    dir: tempfile::TempDir,
}

impl Layout {
    fn new() -> Self {
        let l = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        for d in ["backing", "mnt"] {
            std::fs::create_dir_all(l.p(d)).unwrap();
        }
        l
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn init(&self, state: &Path) {
        let out = run(state, &["init", "--volume", "demo"]);
        assert!(out.status.success(), "{}", stderr(&out));
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
    assert!(!stderr(out).contains("mounted "), "{}", stderr(out));
}

#[test]
fn state_area_inside_the_backing_tree() {
    let l = Layout::new();
    let state = l.p("backing/.state");
    l.init(&state);
    let out = run(&state, &["mount", s(&l.p("backing")), s(&l.p("mnt"))]);
    refused(&out, "is inside the backing tree");
}

#[test]
fn mount_point_inside_the_backing_tree() {
    let l = Layout::new();
    let state = l.p("state");
    l.init(&state);
    std::fs::create_dir_all(l.p("backing/mnt")).unwrap();
    let out = run(
        &state,
        &["mount", s(&l.p("backing")), s(&l.p("backing/mnt"))],
    );
    refused(&out, "the mount point");
    let out = run(&state, &["mount", s(&l.p("backing")), s(&l.p("backing"))]);
    refused(&out, "the same directory");
}

#[test]
fn credentials_inside_the_backing_tree() {
    let l = Layout::new();
    let state = l.p("state");
    l.init(&state);
    std::fs::write(l.p("backing/token"), "secret").unwrap();
    let git = state.join("git");
    let config = |k: &str, v: &str| {
        let ok = Command::new("git")
            .args(["--git-dir", s(&git), "config", k, v])
            .status()
            .unwrap()
            .success();
        assert!(ok);
    };
    config("remote.origin.url", "file:///nowhere.git");
    config("remote.origin.agentfsCredentials", s(&l.p("backing/token")));
    let out = run(&state, &["mount", s(&l.p("backing")), s(&l.p("mnt"))]);
    refused(&out, "agentfsCredentials file");
}

/// An SSH key or known_hosts file under the backing tree is refused before
/// anything is mounted, whether or not this build supports SSH (without the
/// `ssh` feature the remote is misconfigured, and its files are still
/// checked).
#[test]
fn ssh_files_inside_the_backing_tree() {
    for (key, known_hosts, expected) in [
        ("backing/id", "known_hosts", "agentfsSshKey file"),
        ("id", "backing/known_hosts", "agentfsKnownHosts file"),
    ] {
        let l = Layout::new();
        let state = l.p("state");
        l.init(&state);
        let git = state.join("git");
        let config = |k: &str, v: &str| {
            let ok = Command::new("git")
                .args(["--git-dir", s(&git), "config", k, v])
                .status()
                .unwrap()
                .success();
            assert!(ok);
        };
        config("remote.origin.url", "ssh://git@git.example/r.git");
        config("remote.origin.agentfsSshKey", s(&l.p(key)));
        config("remote.origin.agentfsKnownHosts", s(&l.p(known_hosts)));
        let out = run(&state, &["mount", s(&l.p("backing")), s(&l.p("mnt"))]);
        refused(&out, expected);
    }
}

#[test]
fn root_must_match_the_backing_tree() {
    let l = Layout::new();
    let state = l.p("state");
    l.init(&state);
    let out = run(
        &state,
        &[
            "--root",
            s(&l.p("mnt")),
            "mount",
            s(&l.p("backing")),
            s(&l.p("mnt")),
        ],
    );
    refused(&out, "is not the backing tree");
}

#[test]
fn a_held_state_area_exits_75_before_mounting() {
    let l = Layout::new();
    let state = l.p("state");
    l.init(&state);
    let _lock = StateLock::acquire(&StateDir::new(&state), "test-holder", Duration::ZERO).unwrap();
    let out = run(&state, &["mount", s(&l.p("backing")), s(&l.p("mnt"))]);
    assert_eq!(out.status.code(), Some(75), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("command=test-holder"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn bad_passthrough_values_are_rejected() {
    let l = Layout::new();
    let state = l.p("state");
    l.init(&state);
    let out = run(
        &state,
        &[
            "mount",
            "--passthrough",
            "maybe",
            s(&l.p("backing")),
            s(&l.p("mnt")),
        ],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

#[test]
fn an_include_inside_the_backing_tree() {
    let l = Layout::new();
    let state = l.p("state");
    std::fs::write(l.p("backing/agentfs.conf"), "").unwrap();
    let out = run(
        &state,
        &[
            "init",
            "--volume",
            "demo",
            "--include",
            s(&l.p("backing/agentfs.conf")),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let out = run(&state, &["mount", s(&l.p("backing")), s(&l.p("mnt"))]);
    refused(&out, "include.path");
}

#[test]
fn a_missing_mount_point_is_created_before_the_checks() {
    let l = Layout::new();
    let state = l.p("state");
    l.init(&state);
    // Created, then refused by the layout check: the point is that
    // preflight saw a directory, not "does not exist".
    let out = run(
        &state,
        &["mount", s(&l.p("backing")), s(&l.p("backing/new-mnt"))],
    );
    refused(&out, "the mount point");
    assert!(l.p("backing/new-mnt").is_dir());
    // Only one level: a missing parent is an error.
    let out = run(&state, &["mount", s(&l.p("backing")), s(&l.p("a/b"))]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("creating the mount point"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn probe_without_a_mount() {
    let l = Layout::new();
    let state = l.p("state");
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

#[test]
fn hold_without_a_stale_mount_behaves_like_refuse() {
    let l = Layout::new();
    let state = l.p("state");
    l.init(&state);
    std::fs::create_dir_all(l.p("backing/mnt")).unwrap();
    let out = run(
        &state,
        &[
            "mount",
            "--on-stale-mount",
            "hold",
            s(&l.p("backing")),
            s(&l.p("backing/mnt")),
        ],
    );
    refused(&out, "the mount point");
    let out = run(
        &state,
        &[
            "mount",
            "--on-stale-mount",
            "maybe",
            s(&l.p("backing")),
            s(&l.p("mnt")),
        ],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}
