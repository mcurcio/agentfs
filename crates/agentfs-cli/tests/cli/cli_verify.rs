//! `agentfs verify` from the command line (docs/trees.md, "When history
//! cannot check a save"; its exit status: `launcher/verify.rs`): nothing to
//! check on a fresh volume; a flag made through the library checked by the
//! binary, once per release, the release the binary's own hash; exit 2 while
//! a flag stands, 75 while the volume is held. No FUSE.
//!
//! The flag needs the `failpoints` feature: `cargo test -p agentfs-cli
//! --features failpoints --test cli cli_verify::`.
#![allow(
    clippy::disallowed_methods,
    reason = "a test reads the notes History writes"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The binary, its environment cleared of agentfs's and OTLP's variables.
fn agentfs(binary: &Path, state: &Path) -> Command {
    let mut cmd = Command::new(binary);
    for (name, _) in std::env::vars_os() {
        let n = name.to_string_lossy();
        if n.starts_with("OTEL_") || n.starts_with("AGENTFS_") {
            cmd.env_remove(name);
        }
    }
    cmd.arg("--state").arg(state);
    cmd
}

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agentfs"))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn init(state: &Path) {
    let out = agentfs(&binary(), state)
        .args(["init", "--volume", "t"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
}

fn verify(binary: &Path, state: &Path) -> Output {
    agentfs(binary, state).arg("verify").output().unwrap()
}

/// The verdict lines of the flagged commit's note.
#[cfg(feature = "failpoints")]
fn verdicts(state: &Path, commit: git2::Oid) -> Vec<String> {
    agentfs_engine::tree_git::TreeGit::open(state.join("history.git"))
        .unwrap()
        .note(agentfs_engine::tree_actor::VERIFY_REF, commit)
        .unwrap()
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_fresh_volume_has_nothing_to_check() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    init(&state);
    let out = verify(&binary(), &state);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(text(&out.stdout), "");
}

#[test]
fn verify_takes_the_state_flag_only() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    init(&state);
    let out = agentfs(&binary(), &state)
        .args(["--wait-lock", "1s", "verify"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
}

/// A12: the binary checks a flag the library made. The first run (its
/// replay skewed) writes mismatch-confirmed and exits 2; a second run of the
/// same binary writes nothing; a copy of the binary with one byte appended,
/// another release by its hash, writes a second verdict, verified, and
/// exits 0. While the library holds the volume, it exits 75.
#[cfg(feature = "failpoints")]
#[test]
fn verify_checks_a_flag_once_per_release() {
    use agentfs_contracts::TreeId;
    use agentfs_contracts::meta::{Attrs, Perm};
    use agentfs_contracts::pod::{PodContext, WriterAt};
    use agentfs_engine::tree_actor::TreeActor;
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    init(&state);
    let head = {
        let _skew = agentfs_failpoints::configure("tree.build_replay_skew=error@*").unwrap();
        let mut v = TreeActor::open(&state, &PodContext::default()).unwrap();
        let writer = WriterAt {
            writer: agentfs_contracts::name::WriterLabel::parse("w").unwrap(),
            at: PodContext::default(),
        };
        let attrs = Attrs {
            perm: Perm::from_mode(0o644),
            owner: agentfs_contracts::clients::FileOwner { uid: 0, gid: 0 },
        };
        let h = v
            .write_open(TreeId::Root, "a b", &writer, Some(attrs))
            .unwrap();
        v.write(h, 0, b"first ", false).unwrap();
        v.write(h, 6, b"second", false).unwrap();
        v.close(h).unwrap();
        v.settle().unwrap();
        v.head(TreeId::Root).unwrap()
    };
    let skewed = agentfs(&binary(), &state)
        .arg("verify")
        .env(
            agentfs_failpoints::ENV_VAR,
            "tree.build_replay_skew=error@*",
        )
        .output()
        .unwrap();
    assert_eq!(skewed.status.code(), Some(2), "{skewed:?}");
    let line = text(&skewed.stdout);
    let fields: Vec<&str> = line.trim_end().splitn(5, ' ').collect();
    let commit = head.to_string();
    assert_eq!(
        fields[1..4],
        ["root", commit.as_str(), "mismatch-confirmed"]
    );
    assert_eq!(fields[4], "\"a b\"");
    assert_eq!(verdicts(&state, head).len(), 1);
    let again = verify(&binary(), &state);
    assert_eq!(again.status.code(), Some(2), "{again:?}");
    assert!(
        text(&again.stdout).contains(" same-release \"a b\""),
        "{again:?}"
    );
    assert_eq!(
        verdicts(&state, head).len(),
        1,
        "the same release writes nothing"
    );
    // Another release: this binary with one byte appended, written by a
    // child so no descriptor of it is open here at the exec.
    let other = tmp.path().join("agentfs-other");
    let copied = Command::new("sh")
        .args([
            "-c",
            "cp \"$1\" \"$2\" && printf x >> \"$2\" && chmod 755 \"$2\"",
            "sh",
        ])
        .arg(binary())
        .arg(&other)
        .status()
        .unwrap();
    assert!(copied.success());
    let held = TreeActor::open(&state, &PodContext::default()).unwrap();
    assert_eq!(verify(&other, &state).status.code(), Some(75));
    drop(held);
    let later = verify(&other, &state);
    assert_eq!(later.status.code(), Some(0), "{later:?}");
    assert!(
        text(&later.stdout).contains(" verified \"a b\""),
        "{later:?}"
    );
    let written = verdicts(&state, head);
    assert_eq!(written.len(), 2, "{written:?}");
    assert!(written[1].starts_with("Agentfs-Verdict: verified by agentfs "));
    assert_ne!(written[0], written[1]);
}
