//! Rules 13, 14 and 15 between a fork and the root, on real repositories
//! (docs/library/core.md, "Directories, metadata and links"): the library's
//! referee decides each of the model's scenarios (crates/agentfs-model/tests/
//! scenarios.rs, `r13_*`, `r14_*`, `r15_*`) as the model does. Each test reads
//! every tree's entries and metadata with the library, and each collision's
//! cause and path from git.

#[path = "support/trees.rs"]
mod trees;

use std::path::Path;
use std::process::Command;

use agentfs_contracts::clients::FileOwner;
use agentfs_core::tree_actor::{Attrs, Kind, Meta, Perm, RenameMode, TreeActor, TreeId};
use trees::{ATTRS, fanned, new_actor, rowless_fork, writer};

const ROOT: TreeId = TreeId::Root;

/// `ATTRS` with mode `mode`.
fn mode(mode: u32) -> Attrs {
    Attrs {
        perm: Perm::from_mode(mode),
        ..ATTRS
    }
}

/// A file's metadata as a create with `ATTRS` and `mode` gives it.
fn file(mode: u32) -> Meta {
    Meta {
        kind: Kind::File,
        mode: Perm::from_mode(mode),
        uid: ATTRS.owner.uid,
        gid: ATTRS.owner.gid,
    }
}

/// A volume whose root holds `a`, built, and a fork of it.
fn volume(dir: &Path) -> (TreeActor, TreeId) {
    let mut v = new_actor(dir);
    put(&mut v, ROOT, "a", b"a\n");
    drain(&mut v);
    let fork = rowless_fork(&mut v);
    (v, fork)
}

/// A save of `path`'s whole bytes, created with `ATTRS` when it is not there.
fn put(v: &mut TreeActor, tree: TreeId, path: &str, bytes: &[u8]) {
    let create = v.node(tree, path).is_err();
    let h = v
        .write_open(tree, path, &writer("w"), create.then_some(ATTRS))
        .unwrap();
    v.truncate(h, 0).unwrap();
    v.write(h, 0, bytes, false).unwrap();
    v.close(h).unwrap();
}

/// Appends `bytes` to `path`.
fn append(v: &mut TreeActor, tree: TreeId, path: &str, bytes: &[u8]) {
    let h = v.write_open(tree, path, &writer("w"), None).unwrap();
    let len = v.file_len(tree, path).unwrap();
    v.write(h, len, bytes, false).unwrap();
    v.close(h).unwrap();
}

fn mkdir(v: &mut TreeActor, tree: TreeId, path: &str, attrs: Attrs) {
    v.mkdir(tree, path, attrs, &writer("w")).unwrap();
}

fn rmdir(v: &mut TreeActor, tree: TreeId, path: &str) {
    v.rmdir(tree, path, &writer("w")).unwrap();
}

fn rename(v: &mut TreeActor, tree: TreeId, from: &str, to: &str) {
    v.rename(tree, from, to, RenameMode::Replace, &writer("w"))
        .unwrap();
}

/// A chmod and chown of `path` to `attrs`.
fn set_meta(v: &mut TreeActor, tree: TreeId, path: &str, attrs: Attrs) {
    v.set_meta(tree, path, attrs, &writer("w")).unwrap();
}

/// A link at `path` to `target`, owned as `ATTRS`.
fn symlink(v: &mut TreeActor, tree: TreeId, path: &str, target: &str) {
    v.symlink(tree, path, target, ATTRS.owner, &writer("w"))
        .unwrap();
}

/// A program's `symlink(target, tmp)` then `rename(tmp, path)`: an atomic
/// replace of `path` by a link (rule 11's atomic save, rule 15).
fn link_over(v: &mut TreeActor, tree: TreeId, path: &str, target: &str) {
    let tmp = format!("{path}.tmp");
    symlink(v, tree, &tmp, target);
    rename(v, tree, &tmp, path);
}

/// Every take every queue lets go.
fn drain(v: &mut TreeActor) {
    while v.progress().unwrap() {}
}

/// Every entry of `tree` by path, a directory's with a `/`, read with the
/// library.
fn entries(v: &TreeActor, tree: TreeId) -> Vec<String> {
    fn walk(v: &TreeActor, tree: TreeId, dir: &str, out: &mut Vec<String>) {
        for entry in v.children(tree, dir, 0, usize::MAX).unwrap() {
            let path = if dir.is_empty() {
                entry.name
            } else {
                format!("{dir}/{}", entry.name)
            };
            if entry.kind == Kind::Dir {
                out.push(format!("{path}/"));
                walk(v, tree, &path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(v, tree, "", &mut out);
    out.sort();
    out
}

/// The metadata of the entry at `path` in `tree`, read with the library.
fn meta(v: &TreeActor, tree: TreeId, path: &str) -> Meta {
    v.node(tree, path).unwrap().meta
}

/// `git` in the state area's repository; its output.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .env("GIT_DIR", dir.join("history.git"))
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

/// The root's head as built.
fn root_head(v: &TreeActor) -> String {
    v.settle().unwrap();
    v.head(ROOT).unwrap().to_string()
}

/// Every rule 13 and 14 collision in the root's history, read from git: each
/// `Agentfs-Collisions` entry with a cause, as (cause, path).
fn entry_collisions(dir: &Path, head: &str) -> Vec<(String, String)> {
    let log = git(dir, &["log", "--format=%B", head]);
    let mut out = Vec::new();
    for json in log
        .lines()
        .filter_map(|line| line.strip_prefix("Agentfs-Collisions: "))
    {
        let list: Vec<serde_json::Value> = serde_json::from_str(json).unwrap();
        for c in list {
            if let Some(cause) = c.get("cause") {
                assert_eq!(c.as_object().unwrap().len(), 2, "{c}");
                out.push((
                    cause.as_str().unwrap().to_owned(),
                    c["path"].as_str().unwrap().to_owned(),
                ));
            }
        }
    }
    out
}

/// Whether any `Agentfs-Collisions` line is in the root's history.
fn any_collision(dir: &Path, head: &str) -> bool {
    git(dir, &["log", "--format=%B", head]).contains("Agentfs-Collisions: ")
}

/// The commits the root's collision refs keep: each losing change.
fn lost(dir: &Path) -> Vec<String> {
    git(
        dir,
        &[
            "for-each-ref",
            "--format=%(objectname)",
            "refs/collisions/root/",
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

/// The JSON of `lines/<path>` at `rev`: `[identity, line map, meta]`.
fn line_record(dir: &Path, rev: &str, path: &str) -> serde_json::Value {
    let text = git(dir, &["show", &format!("{rev}:lines/{}", fanned(path))]);
    serde_json::from_str(&text).unwrap()
}

/// Rule 13: a fork's chmod lands over the value it saw; history records the
/// value in the file's line record.
#[test]
fn a_chmod_lands_over_the_value_it_saw() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut v, fork) = volume(tmp.path());
    set_meta(&mut v, fork, "a", mode(0o755));
    drain(&mut v);
    for t in [ROOT, fork] {
        assert_eq!(meta(&v, t, "a"), file(0o755), "{t}");
    }
    let head = root_head(&v);
    assert_eq!(
        line_record(tmp.path(), &head, "a")[2],
        serde_json::json!({"kind": "file", "mode": "0755", "uid": 1000, "gid": 1000})
    );
    assert!(!any_collision(tmp.path(), &head));
}

/// Rule 13: two concurrent chmods of one file: the upstream holds a value the
/// fork's writer never saw, so the upstream's wins and the fork's change loses
/// whole, recorded as `metadata`.
#[test]
fn concurrent_chmods_the_upstream_wins_and_the_change_loses_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut v, fork) = volume(tmp.path());
    set_meta(&mut v, fork, "a", mode(0o700));
    let change = v.head(fork).unwrap().to_string();
    set_meta(&mut v, ROOT, "a", mode(0o755));
    drain(&mut v);
    for t in [ROOT, fork] {
        assert_eq!(meta(&v, t, "a"), file(0o755), "{t}");
    }
    let head = root_head(&v);
    assert_eq!(
        entry_collisions(tmp.path(), &head),
        [("metadata".to_owned(), "a".to_owned())]
    );
    assert_eq!(lost(tmp.path()), [change], "the change keeps its ref");
}

/// Rule 13: two trees writing the same new value agree: no collision.
#[test]
fn the_same_new_value_agrees() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut v, fork) = volume(tmp.path());
    let same = Attrs {
        perm: Perm::from_mode(0o600),
        owner: FileOwner { uid: 7, gid: 7 },
    };
    set_meta(&mut v, fork, "a", same);
    set_meta(&mut v, ROOT, "a", same);
    drain(&mut v);
    for t in [ROOT, fork] {
        assert_eq!(meta(&v, t, "a"), Meta::new(Kind::File, same), "{t}");
    }
    assert!(!any_collision(tmp.path(), &root_head(&v)));
}

/// Rule 13: metadata follows the file's identity, so a rename and a chmod of
/// one file both apply.
#[test]
fn a_rename_and_a_chmod_both_apply() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut v, fork) = volume(tmp.path());
    rename(&mut v, fork, "a", "x");
    set_meta(&mut v, ROOT, "a", mode(0o700));
    drain(&mut v);
    for t in [ROOT, fork] {
        assert_eq!(entries(&v, t), ["x"], "{t}");
        assert_eq!(meta(&v, t, "x"), file(0o700), "{t}");
    }
    assert!(!any_collision(tmp.path(), &root_head(&v)));
}

/// Rule 13 with rule 11's atomic save: the file keeps its identity and takes
/// the new file's metadata, here its executable bit, which git shows.
#[test]
fn an_atomic_save_carries_the_new_files_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut v, fork) = volume(tmp.path());
    let identity = line_record(tmp.path(), &root_head(&v), "a")[0].clone();
    put(&mut v, fork, "a.tmp", b"#!/bin/sh\n");
    set_meta(&mut v, fork, "a.tmp", mode(0o755));
    rename(&mut v, fork, "a.tmp", "a");
    drain(&mut v);
    for t in [ROOT, fork] {
        assert_eq!(entries(&v, t), ["a"], "{t}");
        assert_eq!(meta(&v, t, "a"), file(0o755), "{t}");
        assert_eq!(v.read(t, "a").unwrap(), b"#!/bin/sh\n");
    }
    let head = root_head(&v);
    assert_eq!(line_record(tmp.path(), &head, "a")[0], identity);
    let listing = git(
        tmp.path(),
        &["ls-tree", &head, &format!("files/{}", fanned("a"))],
    );
    assert!(listing.starts_with("100755 "), "{listing}");
    assert!(!any_collision(tmp.path(), &head));
}

/// Rule 14: a directory is its path, so two trees making one directory agree.
#[test]
fn two_trees_make_one_directory_and_agree() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut v, fork) = volume(tmp.path());
    mkdir(&mut v, fork, "cache", mode(0o755));
    mkdir(&mut v, ROOT, "cache", mode(0o755));
    put(&mut v, ROOT, "cache/x", b"x\n");
    drain(&mut v);
    for t in [ROOT, fork] {
        assert_eq!(entries(&v, t), ["a", "cache/", "cache/x"], "{t}");
    }
    assert!(!any_collision(tmp.path(), &root_head(&v)));
}

/// Rule 14: an empty directory is kept, as its own record in history.
#[test]
fn an_empty_directory_is_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut v, fork) = volume(tmp.path());
    let attrs = Attrs {
        perm: Perm::from_mode(0o2775),
        owner: FileOwner { uid: 0, gid: 100 },
    };
    mkdir(&mut v, fork, "empty", attrs);
    drain(&mut v);
    for t in [ROOT, fork] {
        assert_eq!(entries(&v, t), ["a", "empty/"], "{t}");
        assert_eq!(meta(&v, t, "empty"), Meta::new(Kind::Dir, attrs), "{t}");
    }
    let hex = git2::Oid::hash_object(git2::ObjectType::Blob, b"empty")
        .unwrap()
        .to_string();
    let record = git(
        tmp.path(),
        &[
            "show",
            &format!("{}:dirs/{}/{}/{hex}", root_head(&v), &hex[..2], &hex[2..4]),
        ],
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&record).unwrap(),
        serde_json::json!({"path": "empty", "kind": "dir", "mode": "2775", "uid": 0, "gid": 100})
    );
}

/// Rule 14: a directory's rename is one change moving everything under it,
/// each file keeping its identity (rule 11), so the upstream's concurrent edit
/// of a file in it applies too.
#[test]
fn renaming_a_directory_moves_everything_under_it_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"a\n");
    mkdir(&mut v, ROOT, "q", mode(0o755));
    mkdir(&mut v, ROOT, "q/s", mode(0o700));
    put(&mut v, ROOT, "q/x", b"");
    put(&mut v, ROOT, "q/s/y", b"y\n");
    drain(&mut v);
    let fork = rowless_fork(&mut v);
    let before = v.head(fork).unwrap();
    rename(&mut v, fork, "q", "e");
    let one = git(
        tmp.path(),
        &["rev-parse", &format!("{}^", v.head(fork).unwrap())],
    );
    assert_eq!(one.trim(), before.to_string(), "one commit on the fork");
    append(&mut v, ROOT, "q/x", b"edit\n");
    drain(&mut v);
    for t in [ROOT, fork] {
        assert_eq!(entries(&v, t), ["a", "e/", "e/s/", "e/s/y", "e/x"], "{t}");
        assert_eq!(meta(&v, t, "e/s").mode, Perm::from_mode(0o700), "{t}");
        assert_eq!(v.read(t, "e/x").unwrap(), b"edit\n", "{t}");
    }
    assert!(!any_collision(tmp.path(), &root_head(&v)));
}

/// A root with `a` and an empty directory `q`, built, and a fork of it.
fn with_q(dir: &Path) -> (TreeActor, TreeId) {
    let mut v = new_actor(dir);
    put(&mut v, ROOT, "a", b"a\n");
    mkdir(&mut v, ROOT, "q", mode(0o755));
    drain(&mut v);
    let fork = rowless_fork(&mut v);
    (v, fork)
}

/// Rule 14: a fork's rmdir loses whole when the upstream has an entry under
/// the directory its writer never saw, recorded as `directory-not-empty`.
#[test]
fn removing_a_directory_the_upstream_filled_loses_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut v, fork) = with_q(tmp.path());
    rmdir(&mut v, fork, "q");
    let change = v.head(fork).unwrap().to_string();
    put(&mut v, ROOT, "q/x", b"x\n");
    drain(&mut v);
    for t in [ROOT, fork] {
        assert_eq!(entries(&v, t), ["a", "q/", "q/x"], "{t}");
    }
    assert_eq!(
        entry_collisions(tmp.path(), &root_head(&v)),
        [("directory-not-empty".to_owned(), "q/".to_owned())]
    );
    assert_eq!(lost(tmp.path()), [change]);
}

/// Rule 14: a fork's move of a directory loses whole when the upstream has an
/// entry under it the fork's writer never saw.
#[test]
fn moving_a_directory_the_upstream_filled_loses_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut v, fork) = with_q(tmp.path());
    rename(&mut v, fork, "q", "e");
    put(&mut v, ROOT, "q/x", b"x\n");
    drain(&mut v);
    for t in [ROOT, fork] {
        assert_eq!(entries(&v, t), ["a", "q/", "q/x"], "{t}");
    }
    assert_eq!(
        entry_collisions(tmp.path(), &root_head(&v)),
        [("directory-not-empty".to_owned(), "q/".to_owned())]
    );
}

/// Rule 14: a fork's move of a directory onto a path loses whole when the
/// upstream has an entry there its writer never saw.
#[test]
fn moving_onto_a_path_the_upstream_filled_loses_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"a\n");
    mkdir(&mut v, ROOT, "q", mode(0o755));
    put(&mut v, ROOT, "q/x", b"x\n");
    drain(&mut v);
    let fork = rowless_fork(&mut v);
    rename(&mut v, fork, "q", "e");
    mkdir(&mut v, ROOT, "e", mode(0o755));
    put(&mut v, ROOT, "e/y", b"y\n");
    drain(&mut v);
    for t in [ROOT, fork] {
        assert_eq!(entries(&v, t), ["a", "e/", "e/y", "q/", "q/x"], "{t}");
    }
    assert_eq!(
        entry_collisions(tmp.path(), &root_head(&v)),
        [("directory-not-empty".to_owned(), "e/".to_owned())]
    );
}

/// Rule 14: a fork's create into a directory the upstream removed loses whole,
/// as the fork did not make that directory itself; recorded as
/// `directory-gone`.
#[test]
fn a_create_into_a_directory_the_upstream_removed_loses_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut v, fork) = with_q(tmp.path());
    put(&mut v, fork, "q/x", b"");
    let change = v.head(fork).unwrap().to_string();
    rmdir(&mut v, ROOT, "q");
    drain(&mut v);
    for t in [ROOT, fork] {
        assert_eq!(entries(&v, t), ["a"], "{t}");
    }
    let head = root_head(&v);
    assert_eq!(
        entry_collisions(tmp.path(), &head),
        [("directory-gone".to_owned(), "q/".to_owned())]
    );
    assert_eq!(lost(tmp.path()), [change]);
}

/// Rule 14: until the upstream's queue decides, a sync keeps the directory a
/// fork's undecided entry needs, though the upstream removed it; then the
/// decision reaches the fork.
#[test]
fn a_sync_keeps_the_directory_of_an_undecided_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut v, fork) = with_q(tmp.path());
    put(&mut v, fork, "q/x", b"x\n");
    rmdir(&mut v, ROOT, "q");
    assert_eq!(
        v.take_next(fork).unwrap(),
        agentfs_core::tree_actor::Took::Taken
    );
    assert_eq!(entries(&v, fork), ["a", "q/", "q/x"]);
    assert_eq!(entries(&v, ROOT), ["a"]);
    drain(&mut v);
    for t in [ROOT, fork] {
        assert_eq!(entries(&v, t), ["a"], "{t}");
    }
    assert_eq!(
        entry_collisions(tmp.path(), &root_head(&v)),
        [("directory-gone".to_owned(), "q/".to_owned())]
    );
}

/// Rule 14: a decided upstream rmdir reaches the fork.
#[test]
fn an_upstream_rmdir_reaches_the_fork() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut v, fork) = with_q(tmp.path());
    rmdir(&mut v, ROOT, "q");
    drain(&mut v);
    for t in [ROOT, fork] {
        assert_eq!(entries(&v, t), ["a"], "{t}");
    }
    assert!(!any_collision(tmp.path(), &root_head(&v)));
}

/// Rule 15: a link is a file whose bytes are its target and whose kind is a
/// link; it lands like any file, its target its blob under git's link mode.
#[test]
fn a_link_is_a_file_whose_bytes_are_its_target() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut v, fork) = volume(tmp.path());
    symlink(&mut v, fork, "l", "a");
    drain(&mut v);
    for t in [ROOT, fork] {
        assert_eq!(v.readlink(t, "l").unwrap(), b"a", "{t}");
        assert_eq!(
            meta(&v, t, "l"),
            Meta::new(Kind::Symlink, mode(0o777)),
            "{t}"
        );
    }
    let head = root_head(&v);
    let listing = git(
        tmp.path(),
        &["ls-tree", &head, &format!("files/{}", fanned("l"))],
    );
    assert!(listing.starts_with("120000 "), "{listing}");
    assert!(!any_collision(tmp.path(), &head));
}

/// Rule 15: two concurrent retargets of one link (each an atomic replace) are
/// two edits of one file: the upstream's wins and the fork's change loses
/// whole.
#[test]
fn concurrent_retargets_the_upstream_wins() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    symlink(&mut v, ROOT, "l", "a");
    drain(&mut v);
    let fork = rowless_fork(&mut v);
    link_over(&mut v, fork, "l", "b");
    link_over(&mut v, ROOT, "l", "c");
    drain(&mut v);
    assert_eq!(entries(&v, ROOT), entries(&v, fork));
    for t in [ROOT, fork] {
        assert_eq!(v.readlink(t, "l").unwrap(), b"c", "{t}");
        assert_eq!(meta(&v, t, "l").kind, Kind::Symlink, "{t}");
    }
    let head = root_head(&v);
    assert!(any_collision(tmp.path(), &head));
    assert!(!lost(tmp.path()).is_empty(), "the fork's change lost");
    assert!(entry_collisions(tmp.path(), &head).is_empty());
}

/// Rules 15 and 13: replacing a file by a link (an atomic save of a link over
/// it) keeps the file's identity and changes its kind.
#[test]
fn replacing_a_file_by_a_link_changes_its_kind() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut v, fork) = volume(tmp.path());
    let identity = line_record(tmp.path(), &root_head(&v), "a")[0].clone();
    link_over(&mut v, fork, "a", "b");
    drain(&mut v);
    for t in [ROOT, fork] {
        assert_eq!(entries(&v, t), ["a"], "{t}");
        assert_eq!(v.readlink(t, "a").unwrap(), b"b", "{t}");
        assert_eq!(meta(&v, t, "a").kind, Kind::Symlink, "{t}");
    }
    let head = root_head(&v);
    assert_eq!(line_record(tmp.path(), &head, "a")[0], identity);
    assert!(!any_collision(tmp.path(), &head));
}
