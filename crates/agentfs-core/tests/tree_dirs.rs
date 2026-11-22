//! Directories, modes, owners, links and times in one tree, on real
//! repositories (docs/trees.md, "What a commit holds" and "The library"):
//! each recorded, applied as POSIX answers, kept in history where history
//! keeps it, and the same after a restart and a compaction.

#[path = "support/trees.rs"]
mod trees;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use agentfs_contracts::clients::FileOwner;
use agentfs_core::tree_actor::{
    Attrs, Error, Kind, Node, Perm, RenameMode, SetTime, Took, TreeActor, TreeId,
};
use trees::{ATTRS, checkout, fanned, new_actor, reopen, rowless_fork, writer};

const ROOT: TreeId = TreeId::Root;

/// `ATTRS` with mode `mode`.
fn mode(mode: u32) -> Attrs {
    Attrs {
        perm: Perm::from_mode(mode),
        ..ATTRS
    }
}

fn put(v: &mut TreeActor, tree: TreeId, path: &str, attrs: Attrs, bytes: &[u8]) -> u64 {
    let h = v.write_open(tree, path, &writer("w"), Some(attrs)).unwrap();
    v.truncate(h, 0).unwrap();
    v.write(h, 0, bytes, false).unwrap();
    v.close(h).unwrap();
    v.node(tree, path).unwrap().number.unwrap()
}

fn mkdir(v: &mut TreeActor, tree: TreeId, path: &str) -> u64 {
    v.mkdir(tree, path, mode(0o755), &writer("w")).unwrap()
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

/// Every blob of `rev`'s tree: its git mode and path.
fn listing(dir: &Path, rev: &str) -> Vec<(String, String)> {
    git(dir, &["ls-tree", "-r", "--full-tree", rev])
        .lines()
        .map(|line| {
            let (meta, path) = line.split_once('\t').unwrap();
            (meta.split(' ').next().unwrap().to_owned(), path.to_owned())
        })
        .collect()
}

/// The git mode of `path` in `rev`, if it is there.
fn mode_of(dir: &Path, rev: &str, path: &str) -> Option<String> {
    listing(dir, rev)
        .into_iter()
        .find(|(_, p)| p == path)
        .map(|(m, _)| m)
}

fn show(dir: &Path, rev: &str, path: &str) -> String {
    git(dir, &["show", &format!("{rev}:{path}")])
}

/// The root's head as built.
fn head(v: &TreeActor) -> String {
    v.head(ROOT).unwrap().to_string()
}

fn names(v: &TreeActor, tree: TreeId, dir: &str) -> Vec<(String, Kind)> {
    v.children(tree, dir, 0, 100)
        .unwrap()
        .into_iter()
        .map(|e| (e.name, e.kind))
        .collect()
}

#[test]
fn nested_creates_and_lookups() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let a = mkdir(&mut v, ROOT, "a");
    let b = mkdir(&mut v, ROOT, "a/b");
    let f = put(&mut v, ROOT, "a/b/f", ATTRS, b"nested\n");
    put(&mut v, ROOT, "a/z", ATTRS, b"z\n");
    assert_eq!(v.read(ROOT, "a/b/f").unwrap(), b"nested\n");
    assert_eq!(v.node(ROOT, "a").unwrap().number, Some(a));
    assert_eq!(v.node(ROOT, "a/b/f").unwrap().meta.kind, Kind::File);
    let root = v.node(ROOT, "").unwrap();
    assert_eq!(
        (root.number, root.meta.kind, root.links),
        (None, Kind::Dir, 3)
    );
    assert_eq!(v.node(ROOT, "a").unwrap().links, 3, "a holds one directory");
    assert_eq!(names(&v, ROOT, ""), [("a".to_owned(), Kind::Dir)]);
    assert_eq!(
        names(&v, ROOT, "a"),
        [("b".to_owned(), Kind::Dir), ("z".to_owned(), Kind::File)]
    );
    assert_eq!(names(&v, ROOT, "a/b"), [("f".to_owned(), Kind::File)]);
    let (path, node) = v.node_at(ROOT, b).unwrap();
    assert_eq!((path, node.meta.kind), ("a/b", Kind::Dir));
    assert_eq!(v.node_at(ROOT, f).unwrap().0, "a/b/f");
    // A listing in batches, as READDIR reads it.
    assert_eq!(v.children(ROOT, "a", 1, 10).unwrap()[0].name, "z");
    let w = writer("w");
    assert!(matches!(
        v.write_open(ROOT, "missing/f", &w, Some(ATTRS)),
        Err(Error::NotFound)
    ));
    assert!(matches!(
        v.write_open(ROOT, "a/z/f", &w, Some(ATTRS)),
        Err(Error::NotADirectory)
    ));
    assert!(matches!(
        v.write_open(ROOT, "a/b", &w, Some(ATTRS)),
        Err(Error::IsADirectory)
    ));
    assert!(matches!(
        v.mkdir(ROOT, "a/b", ATTRS, &w),
        Err(Error::Exists)
    ));
    assert!(matches!(
        v.children(ROOT, "a/z", 0, 1),
        Err(Error::NotADirectory)
    ));
    assert!(matches!(v.mkdir(ROOT, "", ATTRS, &w), Err(Error::Name)));
    let long = "x".repeat(256);
    assert!(matches!(
        v.mkdir(ROOT, &long, ATTRS, &w),
        Err(Error::NameTooLong)
    ));
    assert_eq!(Error::NameTooLong.errno(), libc::ENAMETOOLONG);
    drop(v);
    let v = reopen(tmp.path());
    assert_eq!(v.read(ROOT, "a/b/f").unwrap(), b"nested\n");
    assert_eq!(v.node(ROOT, "a/b").unwrap().number, Some(b));
}

#[test]
fn renaming_a_directory_moves_everything_under_it() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let w = writer("w");
    let a = mkdir(&mut v, ROOT, "a");
    let b = mkdir(&mut v, ROOT, "a/b");
    let x = put(&mut v, ROOT, "a/x", ATTRS, b"x\n");
    let y = put(&mut v, ROOT, "a/b/y", ATTRS, b"y\n");
    v.settle().unwrap();
    let before = head(&v);
    let identity = |rev: &str, path: &str| -> u64 {
        let entry: serde_json::Value =
            serde_json::from_str(&show(tmp.path(), rev, &format!("lines/{}", fanned(path))))
                .unwrap();
        entry[0].as_u64().unwrap()
    };
    let y_id = identity(&before, "a/b/y");
    // An open under the directory follows its object.
    let open = v.write_open(ROOT, "a/x", &w, None).unwrap();
    v.rename(ROOT, "a", "c", RenameMode::Replace, &w).unwrap();
    v.write(open, 0, b"X\n", false).unwrap();
    v.close(open).unwrap();
    assert!(matches!(v.node(ROOT, "a"), Err(Error::NotFound)));
    assert_eq!(v.node(ROOT, "c").unwrap().number, Some(a));
    assert_eq!(v.node(ROOT, "c/b").unwrap().number, Some(b));
    assert_eq!(v.node(ROOT, "c/x").unwrap().number, Some(x));
    assert_eq!(v.node(ROOT, "c/b/y").unwrap().number, Some(y));
    assert_eq!(v.read(ROOT, "c/x").unwrap(), b"X\n");
    assert_eq!(v.read(ROOT, "c/b/y").unwrap(), b"y\n");
    v.settle().unwrap();
    let after = head(&v);
    assert_eq!(identity(&after, "c/b/y"), y_id, "identities are kept");
    let paths: Vec<String> = listing(tmp.path(), &after)
        .into_iter()
        .map(|(_, p)| p)
        .collect();
    assert!(paths.contains(&format!("files/{}", fanned("c/b/y"))));
    assert!(
        !paths
            .iter()
            .any(|p| p.ends_with("/a/b/y") || p.ends_with("/a/x")),
        "{paths:?}"
    );
    // Each moved file has a new name version: it is no longer fresh.
    let names = |rev: &str| -> serde_json::Value {
        serde_json::from_str(&show(
            tmp.path(),
            rev,
            &format!("names/{}", fanned(&y_id.to_string())),
        ))
        .unwrap()
    };
    let history = names(&after);
    let head_version = &history["versions"][history["head"].as_u64().unwrap().to_string()];
    assert_eq!(head_version["path"], "c/b/y");
    assert!(head_version["over"].is_u64(), "{history}");
    // An atomic save over it keeps the file.s identity there.
    put(&mut v, ROOT, "tmp", ATTRS, b"new\n");
    v.rename(ROOT, "tmp", "c/b/y", RenameMode::Replace, &w)
        .unwrap();
    v.settle().unwrap();
    assert_eq!(identity(&head(&v), "c/b/y"), y_id);
    drop(v);
    let v = reopen(tmp.path());
    assert_eq!(v.node(ROOT, "c/b").unwrap().number, Some(b));
    assert_eq!(v.read(ROOT, "c/b/y").unwrap(), b"new\n");
}

#[test]
fn rmdir_refuses_a_directory_with_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let w = writer("w");
    mkdir(&mut v, ROOT, "a");
    mkdir(&mut v, ROOT, "a/sub");
    put(&mut v, ROOT, "a/f", ATTRS, b"f");
    let refused = v.rmdir(ROOT, "a", &w).unwrap_err();
    assert!(matches!(refused, Error::NotEmpty));
    assert_eq!(refused.errno(), libc::ENOTEMPTY);
    assert!(matches!(
        v.rmdir(ROOT, "a/f", &w),
        Err(Error::NotADirectory)
    ));
    assert!(matches!(
        v.unlink(ROOT, "a/sub", &w),
        Err(Error::IsADirectory)
    ));
    assert!(matches!(v.rmdir(ROOT, "nothing", &w), Err(Error::NotFound)));
    v.unlink(ROOT, "a/f", &w).unwrap();
    v.rmdir(ROOT, "a/sub", &w).unwrap();
    v.rmdir(ROOT, "a", &w).unwrap();
    assert!(matches!(v.node(ROOT, "a"), Err(Error::NotFound)));
    assert_eq!(v.node(ROOT, "").unwrap().links, 2);
    v.settle().unwrap();
    assert!(
        !listing(tmp.path(), &head(&v))
            .iter()
            .any(|(_, p)| p.starts_with("dirs/") && p != "dirs/root")
    );
}

#[test]
fn rename_errors_follow_posix() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let w = writer("w");
    for dir in ["d", "d/inner", "full", "empty"] {
        mkdir(&mut v, ROOT, dir);
    }
    put(&mut v, ROOT, "full/f", ATTRS, b"f");
    put(&mut v, ROOT, "file", ATTRS, b"x");
    put(&mut v, ROOT, "other", ATTRS, b"y");
    let mut rename = |from: &str, to: &str, mode: RenameMode| v.rename(ROOT, from, to, mode, &w);
    let errno = |r: Result<(), Error>| r.map_err(|e| e.errno());
    assert_eq!(
        errno(rename("d", "file", RenameMode::Replace)),
        Err(libc::ENOTDIR)
    );
    assert_eq!(
        errno(rename("file", "d", RenameMode::Replace)),
        Err(libc::EISDIR)
    );
    assert_eq!(
        errno(rename("d", "full", RenameMode::Replace)),
        Err(libc::ENOTEMPTY)
    );
    assert_eq!(
        errno(rename("d", "d/inner/x", RenameMode::Replace)),
        Err(libc::EINVAL)
    );
    assert_eq!(
        errno(rename("file", "other", RenameMode::NoReplace)),
        Err(libc::EEXIST)
    );
    assert_eq!(
        errno(rename("nothing", "x", RenameMode::Replace)),
        Err(libc::ENOENT)
    );
    assert_eq!(
        errno(rename("file", "nothing/x", RenameMode::Replace)),
        Err(libc::ENOENT)
    );
    assert_eq!(errno(rename("d", "d", RenameMode::Replace)), Ok(()));
    let d = v.node(ROOT, "d").unwrap().number;
    v.rename(ROOT, "d", "empty", RenameMode::Replace, &w)
        .unwrap();
    assert_eq!(
        v.node(ROOT, "empty").unwrap().number,
        d,
        "onto an empty directory"
    );
    assert!(v.node(ROOT, "empty/inner").is_ok());
    assert!(matches!(v.node(ROOT, "d"), Err(Error::NotFound)));
    v.rename(ROOT, "file", "empty/inner/file", RenameMode::NoReplace, &w)
        .unwrap();
    assert_eq!(v.read(ROOT, "empty/inner/file").unwrap(), b"x");
}

#[test]
fn an_executable_file_is_100755_in_history() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    mkdir(&mut v, ROOT, "a");
    mkdir(&mut v, ROOT, "a/b");
    put(
        &mut v,
        ROOT,
        "a/b/run.sh",
        mode(0o755),
        b"#!/bin/sh\necho hi\n",
    );
    put(&mut v, ROOT, "a/b/data", mode(0o644), b"plain\n");
    v.settle().unwrap();
    let rev = head(&v);
    let run = format!("files/{}", fanned("a/b/run.sh"));
    assert_eq!(mode_of(tmp.path(), &rev, &run).as_deref(), Some("100755"));
    let data = format!("files/{}", fanned("a/b/data"));
    assert_eq!(mode_of(tmp.path(), &rev, &data).as_deref(), Some("100644"));
    // A chmod is a step: its commit has the new mode.
    v.set_meta(ROOT, "a/b/run.sh", mode(0o644), &writer("w"))
        .unwrap();
    v.settle().unwrap();
    let rev = head(&v);
    assert_eq!(mode_of(tmp.path(), &rev, &run).as_deref(), Some("100644"));
    assert_eq!(
        git(tmp.path(), &["log", "-1", "--format=%s", &rev]).trim(),
        "Meta by w"
    );
    // The checkout's file has the mode too.
    let checked_out = checkout(tmp.path(), "root", "a/b/data");
    assert_eq!(
        std::fs::metadata(checked_out).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[test]
fn the_line_record_carries_meta() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let attrs = Attrs {
        perm: Perm::from_mode(0o750),
        owner: FileOwner {
            uid: 1234,
            gid: 5678,
        },
    };
    put(&mut v, ROOT, "f", attrs, b"one\n");
    v.settle().unwrap();
    let entry: serde_json::Value = serde_json::from_str(&show(
        tmp.path(),
        &head(&v),
        &format!("lines/{}", fanned("f")),
    ))
    .unwrap();
    assert_eq!(entry.as_array().unwrap().len(), 3);
    assert_eq!(
        entry[2],
        serde_json::json!({"kind": "file", "mode": "0750", "uid": 1234, "gid": 5678})
    );
    let node = v.node(ROOT, "f").unwrap();
    assert_eq!(
        (node.meta.uid, node.meta.gid, node.meta.mode.bits()),
        (1234, 5678, 0o750)
    );
    drop(v);
    let v = reopen(tmp.path());
    assert_eq!(v.node(ROOT, "f").unwrap().meta, node.meta);
}

#[test]
fn a_directory_record_is_kept_for_an_empty_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    mkdir(&mut v, ROOT, "a");
    v.mkdir(ROOT, "a/b", mode(0o700), &writer("w")).unwrap();
    v.settle().unwrap();
    let rev = head(&v);
    let (aa_bb, _) = fanned("a/b")
        .rsplit_once("/a/b")
        .map(|(f, r)| (f.to_owned(), r.to_owned()))
        .unwrap();
    // The leaf is the path's whole blob id: `printf a/b | git hash-object --stdin`.
    let key = git2::Oid::hash_object(git2::ObjectType::Blob, b"a/b").unwrap();
    let record = format!("dirs/{aa_bb}/{key}");
    assert_eq!(
        mode_of(tmp.path(), &rev, &record).as_deref(),
        Some("100644")
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&show(tmp.path(), &rev, &record)).unwrap(),
        serde_json::json!({"path": "a/b", "kind": "dir", "mode": "0700", "uid": 1000, "gid": 1000})
    );
    assert_eq!(
        git(tmp.path(), &["log", "-1", "--format=%s", &rev]).trim(),
        "Mkdir by w"
    );
    assert!(git(tmp.path(), &["cat-file", "-p", &rev]).contains("Agentfs-Format: envelope-2"));
    drop(v);
    let v = reopen(tmp.path());
    assert_eq!(v.node(ROOT, "a/b").unwrap().meta.mode.bits(), 0o700);
    assert!(v.children(ROOT, "a/b", 0, 10).unwrap().is_empty());
}

#[test]
fn the_root_commit_holds_the_root_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let v = new_actor(tmp.path());
    let root = git(
        tmp.path(),
        &["rev-list", "--max-parents=0", "refs/heads/root"],
    );
    let root = root.trim();
    assert_eq!(
        listing(tmp.path(), root),
        [("100644".to_owned(), "dirs/root".to_owned())]
    );
    // SAFETY: neither call has preconditions.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&show(tmp.path(), root, "dirs/root")).unwrap(),
        serde_json::json!({"path": "", "kind": "dir", "mode": "0755", "uid": uid, "gid": gid})
    );
    let node = v.node(ROOT, "").unwrap();
    assert_eq!((node.meta.uid, node.meta.mode.bits()), (uid, 0o755));
}

#[test]
fn a_link_is_120000_with_its_target_and_a_regular_file_in_the_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let owner = FileOwner {
        uid: 1000,
        gid: 1000,
    };
    let w = writer("w");
    let link = v.symlink(ROOT, "l", "/etc/passwd", owner, &w).unwrap();
    let node = v.node(ROOT, "l").unwrap();
    assert_eq!(node.number, Some(link));
    assert_eq!(
        (node.meta.kind, node.meta.mode.bits(), node.len),
        (Kind::Symlink, 0o777, 11)
    );
    assert_eq!(v.readlink(ROOT, "l").unwrap(), b"/etc/passwd");
    put(&mut v, ROOT, "f", ATTRS, b"f");
    assert!(matches!(v.readlink(ROOT, "f"), Err(Error::Invalid)));
    assert!(matches!(
        v.symlink(ROOT, "e", "", owner, &w),
        Err(Error::NotFound)
    ));
    assert!(matches!(
        v.symlink(ROOT, "n", "a\0b", owner, &w),
        Err(Error::Invalid)
    ));
    assert!(matches!(
        v.symlink(ROOT, "long", &"t".repeat(4096), owner, &w),
        Err(Error::NameTooLong)
    ));
    v.settle().unwrap();
    let rev = head(&v);
    let path = format!("files/{}", fanned("l"));
    assert_eq!(mode_of(tmp.path(), &rev, &path).as_deref(), Some("120000"));
    assert_eq!(show(tmp.path(), &rev, &path), "/etc/passwd");
    assert_eq!(
        git(tmp.path(), &["log", "-1", "--format=%s", &rev, "--", &path]).trim(),
        "Symlink by w"
    );
    let checked_out = checkout(tmp.path(), "root", "l");
    let meta = std::fs::symlink_metadata(&checked_out).unwrap();
    assert!(meta.file_type().is_file(), "a regular file, never a link");
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read(&checked_out).unwrap(), b"/etc/passwd");
    drop(v);
    let v = reopen(tmp.path());
    assert_eq!(v.readlink(ROOT, "l").unwrap(), b"/etc/passwd");
    assert_eq!(v.node(ROOT, "l").unwrap().meta.kind, Kind::Symlink);
}

#[test]
fn setting_times_makes_no_commit_and_survives_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "f", ATTRS, b"f");
    mkdir(&mut v, ROOT, "d");
    v.settle().unwrap();
    let before = head(&v);
    let w = writer("w");
    v.set_times(
        ROOT,
        "f",
        Some(SetTime::At(1_000)),
        Some(SetTime::At(2_000)),
        &w,
    )
    .unwrap();
    v.set_times(ROOT, "d", None, Some(SetTime::At(3_000)), &w)
        .unwrap();
    v.set_times(ROOT, "", Some(SetTime::Now), None, &w).unwrap();
    v.settle().unwrap();
    assert_eq!(head(&v), before, "times make no commit");
    let times = v.node(ROOT, "f").unwrap().times;
    assert_eq!((times.atime_ns, times.mtime_ns), (1_000, 2_000));
    assert!(times.ctime_ns > 2_000, "ctime moves to the step's time");
    let d = v.node(ROOT, "d").unwrap().times;
    assert_eq!(d.mtime_ns, 3_000);
    assert!(matches!(
        v.set_times(ROOT, "nothing", None, None, &w),
        Err(Error::NotFound)
    ));
    drop(v);
    let v = reopen(tmp.path());
    assert_eq!(v.node(ROOT, "f").unwrap().times, times);
    assert_eq!(v.node(ROOT, "d").unwrap().times, d);
}

#[test]
fn a_save_moves_mtime_and_a_create_moves_its_directorys() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let w = writer("w");
    mkdir(&mut v, ROOT, "d");
    v.set_times(ROOT, "d", Some(SetTime::At(1)), Some(SetTime::At(1)), &w)
        .unwrap();
    put(&mut v, ROOT, "d/f", ATTRS, b"one");
    let dir = v.node(ROOT, "d").unwrap().times;
    let file = v.node(ROOT, "d/f").unwrap().times;
    assert!(dir.mtime_ns > 1 && dir.ctime_ns == dir.mtime_ns);
    v.set_times(ROOT, "d/f", Some(SetTime::At(1)), Some(SetTime::At(1)), &w)
        .unwrap();
    let h = v.write_open(ROOT, "d/f", &w, None).unwrap();
    v.write(h, 0, b"two", false).unwrap();
    v.close(h).unwrap();
    let saved = v.node(ROOT, "d/f").unwrap().times;
    assert!(saved.mtime_ns >= file.mtime_ns && saved.mtime_ns > 1);
    assert_eq!(saved.ctime_ns, saved.mtime_ns);
    // A close that changes nothing moves nothing.
    let h = v.write_open(ROOT, "d/f", &w, None).unwrap();
    v.close(h).unwrap();
    assert_eq!(v.node(ROOT, "d/f").unwrap().times, saved);
    // The directory's times moved at the create, not at the save.
    assert_eq!(v.node(ROOT, "d").unwrap().times, dir);
}

#[test]
fn a_new_entry_in_a_set_group_id_directory_takes_its_group() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let w = writer("w");
    let shared = Attrs {
        perm: Perm::from_mode(0o2775),
        owner: FileOwner {
            uid: 1000,
            gid: 2000,
        },
    };
    v.mkdir(ROOT, "g", shared, &w).unwrap();
    put(&mut v, ROOT, "g/f", ATTRS, b"f");
    let file: Node = v.node(ROOT, "g/f").unwrap();
    assert_eq!((file.meta.uid, file.meta.gid), (1000, 2000));
    assert_eq!(file.meta.mode.bits(), 0o644);
    v.mkdir(ROOT, "g/sub", mode(0o755), &w).unwrap();
    let sub = v.node(ROOT, "g/sub").unwrap();
    assert_eq!(sub.meta.gid, 2000);
    assert_eq!(
        sub.meta.mode.bits(),
        0o2755,
        "a new directory keeps the bit"
    );
    // Outside it, the caller's group.
    put(&mut v, ROOT, "plain", ATTRS, b"p");
    assert_eq!(v.node(ROOT, "plain").unwrap().meta.gid, 1000);
}

/// Every directory's and file's node, by path.
fn nodes(v: &TreeActor) -> Vec<(String, Node)> {
    let mut all = vec![(String::new(), v.node(ROOT, "").unwrap())];
    let mut at = 0;
    while at < all.len() {
        let (dir, node) = all[at].clone();
        at += 1;
        if node.meta.kind != Kind::Dir {
            continue;
        }
        for entry in v.children(ROOT, &dir, 0, 1000).unwrap() {
            let path = if dir.is_empty() {
                entry.name
            } else {
                format!("{dir}/{}", entry.name)
            };
            let node = v.node(ROOT, &path).unwrap();
            all.push((path, node));
        }
    }
    all.sort_by(|a, b| a.0.cmp(&b.0));
    all
}

#[test]
fn directory_numbers_and_times_survive_compaction_and_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = trees::new_actor_with(tmp.path(), trees::compacting());
    let w = writer("w");
    mkdir(&mut v, ROOT, "a");
    mkdir(&mut v, ROOT, "a/b");
    put(&mut v, ROOT, "a/b/f", mode(0o755), b"f\n");
    v.set_times(ROOT, "a", Some(SetTime::At(5)), Some(SetTime::At(6)), &w)
        .unwrap();
    let seen = nodes(&v);
    // Every step built and compacted: the reopen starts from a checkpoint
    // at the last record.
    let settled = v.settle().unwrap();
    assert_eq!(trees::compacted_through(&v, settled).built, settled);
    drop(v);
    let mut v = trees::reopen_with(tmp.path(), trees::compacting());
    assert_eq!(nodes(&v), seen);
    // Steps after the checkpoint, built and compacted before the next
    // reopen: the checkpoint holds their numbers and times.
    mkdir(&mut v, ROOT, "c");
    v.rename(ROOT, "a/b", "c/b", RenameMode::Replace, &w)
        .unwrap();
    v.set_times(ROOT, "c/b/f", None, Some(SetTime::At(9)), &w)
        .unwrap();
    let seen = nodes(&v);
    let settled = v.settle().unwrap();
    assert_eq!(trees::compacted_through(&v, settled).built, settled);
    drop(v);
    let v = trees::reopen_with(tmp.path(), trees::compacting());
    assert_eq!(nodes(&v), seen);
    drop(v);
    let v = reopen(tmp.path());
    assert_eq!(nodes(&v), seen);
}

#[test]
fn a_fork_sees_the_upstreams_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let a = mkdir(&mut v, ROOT, "a");
    put(&mut v, ROOT, "a/f", mode(0o755), b"f\n");
    let fork = rowless_fork(&mut v);
    assert_eq!(v.node(fork, "a").unwrap().number, Some(a));
    assert_eq!(v.read(fork, "a/f").unwrap(), b"f\n");
    assert_eq!(v.node(fork, "a/f").unwrap().meta.mode.bits(), 0o755);
    drop(v);
    let v = reopen(tmp.path());
    assert_eq!(v.node(fork, "a").unwrap().number, Some(a));
}

/// Takes every entry of `tree`'s queue.
fn take_all(v: &mut TreeActor, tree: TreeId) {
    while v.take_next(tree).unwrap() == Took::Taken {}
}

#[test]
fn a_forks_new_directory_and_file_land_in_the_root() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "seed", ATTRS, b"s\n");
    let fork = rowless_fork(&mut v);
    mkdir(&mut v, fork, "d");
    mkdir(&mut v, fork, "d/e");
    put(&mut v, fork, "d/e/run.sh", mode(0o755), b"#!/bin/sh\n");
    // The root moved meanwhile: the landings merge.
    put(&mut v, ROOT, "seed", ATTRS, b"root\n");
    take_all(&mut v, ROOT);
    assert_eq!(v.node(ROOT, "d/e").unwrap().meta.kind, Kind::Dir);
    assert_eq!(v.read(ROOT, "d/e/run.sh").unwrap(), b"#!/bin/sh\n");
    assert_eq!(v.node(ROOT, "d/e/run.sh").unwrap().meta.mode.bits(), 0o755);
    v.settle().unwrap();
    let rev = head(&v);
    let run = format!("files/{}", fanned("d/e/run.sh"));
    assert_eq!(mode_of(tmp.path(), &rev, &run).as_deref(), Some("100755"));
    drop(v);
    let v = reopen(tmp.path());
    assert_eq!(v.read(ROOT, "d/e/run.sh").unwrap(), b"#!/bin/sh\n");
}

#[test]
fn the_roots_new_directory_reaches_a_fork() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let fork = rowless_fork(&mut v);
    mkdir(&mut v, ROOT, "d");
    put(&mut v, ROOT, "d/f", ATTRS, b"f\n");
    v.mkdir(ROOT, "empty", mode(0o700), &writer("w")).unwrap();
    take_all(&mut v, fork);
    assert_eq!(v.read(fork, "d/f").unwrap(), b"f\n");
    assert_eq!(v.node(fork, "empty").unwrap().meta.mode.bits(), 0o700);
    assert_eq!(
        names(&v, fork, ""),
        [("d".to_owned(), Kind::Dir), ("empty".to_owned(), Kind::Dir)]
    );
    drop(v);
    let v = reopen(tmp.path());
    assert_eq!(v.read(fork, "d/f").unwrap(), b"f\n");
}
