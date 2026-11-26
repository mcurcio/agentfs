//! Untrack at open (docs/trees.md, "Rules and the untracked area"): a
//! tracked file the operator's `info/attributes` now marks `-agentfs` moves
//! to its tree's untracked area and leaves history, once.
#![allow(
    clippy::disallowed_methods,
    reason = "a test reads the store History writes"
)]

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use agentfs_contracts::TreeId;
use agentfs_contracts::clients::FileOwner;
use agentfs_contracts::meta::{Attrs, Perm};
use agentfs_engine::tree_actor::TreeActor;
use trees::{ATTRS, new_actor, reopen, rowless_fork, writer};

use crate::trees;

const ROOT: TreeId = TreeId::Root;

/// Mode 0600, owned by 1000:1000.
const SECRET: Attrs = Attrs {
    perm: Perm::from_mode(0o600),
    owner: FileOwner {
        uid: 1000,
        gid: 1000,
    },
};

fn put(v: &mut TreeActor, tree: TreeId, path: &str, attrs: Attrs, bytes: &[u8]) {
    let h = v.write_open(tree, path, &writer("w"), Some(attrs)).unwrap();
    v.write(h, 0, bytes, false).unwrap();
    v.close(h).unwrap();
}

fn mark_env(dir: &Path) {
    let info = dir.join("history.git/info");
    fs::create_dir_all(&info).unwrap();
    fs::write(info.join("attributes"), "*.env -agentfs\n").unwrap();
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(dir.join("history.git"))
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap()
}

/// The subjects of `tree`'s commits that unlinked something, newest first.
fn unlinks(dir: &Path, tree: TreeId) -> Vec<String> {
    git(dir, &["log", "--format=%s", &tree.to_string()])
        .lines()
        .filter(|subject| subject.starts_with("Unlink"))
        .map(str::to_owned)
        .collect()
}

fn area(dir: &Path, tree: TreeId) -> PathBuf {
    dir.canonicalize()
        .unwrap()
        .join("untracked")
        .join(tree.to_string())
}

/// Whether this process may give files away, so the area keeps owners.
fn can_chown() -> bool {
    // SAFETY: no preconditions; cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// The volume with `a/secret.env` (mode 0600) and `keep.md` tracked, then
/// reopened with `*.env -agentfs`.
fn untracked_at_open(dir: &Path) -> TreeActor {
    let mut v = new_actor(dir);
    let dirs = Attrs {
        perm: Perm::from_mode(0o750),
        ..ATTRS
    };
    v.mkdir(ROOT, "a", dirs, &writer("w")).unwrap();
    put(&mut v, ROOT, "a/secret.env", SECRET, b"TOKEN=1\n");
    put(&mut v, ROOT, "keep.md", ATTRS, b"kept\n");
    drop(v);
    mark_env(dir);
    reopen(dir)
}

#[test]
fn a_tracked_path_marked_never_record_moves_to_the_area_once() {
    let tmp = tempfile::tempdir().unwrap();
    let v = untracked_at_open(tmp.path());
    let placed = area(tmp.path(), ROOT).join("a/secret.env");
    assert_eq!(fs::read(&placed).unwrap(), b"TOKEN=1\n");
    let meta = fs::symlink_metadata(&placed).unwrap();
    assert_eq!(meta.permissions().mode() & 0o7777, 0o600);
    let container = fs::metadata(area(tmp.path(), ROOT).join("a")).unwrap();
    assert_eq!(container.permissions().mode() & 0o7777, 0o750);
    if can_chown() {
        assert_eq!((meta.uid(), meta.gid()), (1000, 1000));
    }
    assert!(v.node(ROOT, "a/secret.env").is_err(), "no longer tracked");
    assert!(v.node(ROOT, "a").is_ok(), "its directory stays tracked");
    assert_eq!(v.read(ROOT, "keep.md").unwrap(), b"kept\n");
    let head = v.head(ROOT).unwrap();
    let listed = git(
        tmp.path(),
        &["ls-tree", "-r", "--name-only", &head.to_string()],
    );
    assert!(!listed.contains("secret.env"), "{listed}");
    assert!(listed.contains("keep.md"), "{listed}");
    assert_eq!(unlinks(tmp.path(), ROOT), ["Unlink by agentfs"]);
    drop(v);

    // A second open finds nothing to move.
    let v = reopen(tmp.path());
    assert_eq!(v.head(ROOT).unwrap(), head);
    assert_eq!(unlinks(tmp.path(), ROOT).len(), 1);
    assert_eq!(fs::read(&placed).unwrap(), b"TOKEN=1\n");
}

#[cfg(feature = "failpoints")]
#[test]
fn a_crash_between_the_placement_and_the_unlink_completes_at_the_next_open() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "secret.env", SECRET, b"TOKEN=1\n");
    drop(v);
    mark_env(tmp.path());
    {
        let _guard = agentfs_failpoints::configure("tree.untrack_after_place=error").unwrap();
        assert!(TreeActor::open(tmp.path(), &Default::default()).is_err());
    }
    let placed = area(tmp.path(), ROOT).join("secret.env");
    assert_eq!(
        fs::read(&placed).unwrap(),
        b"TOKEN=1\n",
        "placed, not unlinked"
    );
    assert!(unlinks(tmp.path(), ROOT).is_empty());
    let v = reopen(tmp.path());
    assert!(v.node(ROOT, "secret.env").is_err());
    assert_eq!(fs::read(&placed).unwrap(), b"TOKEN=1\n");
    let entries: Vec<_> = fs::read_dir(area(tmp.path(), ROOT))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(entries, ["secret.env"], "one copy");
    let staging = tmp.path().join("untracked/.staging");
    assert!(
        !staging.exists() || fs::read_dir(&staging).unwrap().next().is_none(),
        "no temporary left"
    );
    v.head(ROOT).unwrap();
    assert_eq!(unlinks(tmp.path(), ROOT), ["Unlink by agentfs"]);
}

#[test]
fn a_forks_tracked_never_recorded_file_moves_to_the_forks_area() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "secret.env", SECRET, b"TOKEN=1\n");
    let fork = rowless_fork(&mut v);
    put(&mut v, fork, "secret.env", SECRET, b"TOKEN=2\n");
    drop(v);
    mark_env(tmp.path());
    let mut v = reopen(tmp.path());
    assert!(v.node(ROOT, "secret.env").is_err());
    assert!(v.node(fork, "secret.env").is_err());
    assert_eq!(
        fs::read(area(tmp.path(), ROOT).join("secret.env")).unwrap(),
        b"TOKEN=1\n"
    );
    assert_eq!(
        fs::read(area(tmp.path(), fork).join("secret.env")).unwrap(),
        b"TOKEN=2\n"
    );
    v.head(fork).unwrap();
    assert_eq!(unlinks(tmp.path(), fork), ["Unlink by agentfs"]);
    // The queued landings and syncs take with no conflict left behind.
    while v.progress().unwrap() {}
    assert!(v.node(ROOT, "secret.env").is_err());
    assert!(v.node(fork, "secret.env").is_err());
}
