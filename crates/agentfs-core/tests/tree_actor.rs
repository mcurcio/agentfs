//! The recording slice on real Git repositories and Linux byte operations.
#![allow(
    clippy::disallowed_methods,
    reason = "a test reads the store History writes"
)]
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;

use agentfs_contracts::name::WriterLabel;
use agentfs_contracts::pod::PodContext;
use agentfs_core::tree_actor::{Error, Saved, Took, TreeActor, TreeId};
use trees::{new_actor, reopen, writer};

const ROOT: TreeId = TreeId::Root;
use agentfs_core::tree_git::TreeGit;

#[path = "support/trees.rs"]
mod trees;

fn repo(root: &std::path::Path) -> TreeGit {
    TreeGit::open(root.join("history.git")).unwrap()
}

fn entry(repo: &TreeGit, tree: git2::Oid, name: &str) -> git2::Oid {
    repo.list_tree(tree)
        .unwrap()
        .into_iter()
        .find(|e| e.name == name.as_bytes())
        .unwrap()
        .oid
}

/// Each frame of a journal: where it starts and how long it is. Frozen format.
fn frames(journal: &[u8]) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    let mut at = 0;
    while at < journal.len() {
        let field = |offset: usize| {
            u32::from_le_bytes(journal[at + offset..at + offset + 4].try_into().unwrap()) as usize
        };
        let length = 44 + field(28) + field(32);
        result.push((at, length));
        at += length;
    }
    result
}

fn git(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .arg("--git-dir")
        .arg(root.join("history.git"))
        .args(args)
        .output()
        .unwrap()
}

/// How many commits on the root name this open.
fn opens_saved(root: &std::path::Path, open: u64) -> usize {
    let out = git(
        root,
        &[
            "log",
            &ROOT.to_string(),
            "--format=%(trailers:key=Agentfs-Open,valueonly)",
        ],
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|line| *line == open.to_string())
        .count()
}

fn put(v: &mut TreeActor, tree: TreeId, path: &str, bytes: &[u8]) -> u64 {
    let h = v
        .write_open(tree, path, &writer("test-writer"), true)
        .unwrap();
    v.truncate(h, 0).unwrap();
    v.write(h, 0, bytes, false).unwrap();
    v.close(h).unwrap();
    h
}

#[test]
fn byte_calls_match_a_plain_linux_file_and_publish_only_at_release() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(&tmp.path().join("state"));
    put(&mut v, ROOT, "a", b"foo");
    let h = v.write_open(ROOT, "a", &writer("writer"), false).unwrap();
    let mut plain = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(tmp.path().join("plain"))
        .unwrap();
    plain.write_all(b"foo").unwrap();
    let mut seed = 17u64;
    for i in 0..200 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let offset = (seed >> 32) % 75;
        let data = vec![(seed >> 16) as u8; (seed as usize % 8) + 1];
        match i % 4 {
            0 => {
                v.truncate(h, offset).unwrap();
                plain.set_len(offset).unwrap();
            }
            1 => {
                v.write(h, 0, &data, true).unwrap();
                plain.seek(SeekFrom::End(0)).unwrap();
                plain.write_all(&data).unwrap();
            }
            _ => {
                v.write(h, offset, &data, false).unwrap();
                plain.write_all_at(&data, offset).unwrap();
            }
        }
        assert_eq!(
            v.read_open(h).unwrap(),
            fs::read(tmp.path().join("plain")).unwrap()
        );
        // FLUSH and fsync make records durable; they publish nothing.
        v.durability().make_all_durable().unwrap();
        assert_eq!(v.read(ROOT, "a").unwrap(), b"foo");
    }
    v.close(h).unwrap();
    assert_eq!(
        v.read(ROOT, "a").unwrap(),
        fs::read(tmp.path().join("plain")).unwrap()
    );
    let repo = repo(&tmp.path().join("state"));
    let commit = repo.read_commit(v.head(ROOT).unwrap()).unwrap();
    assert!(
        String::from_utf8(commit.message)
            .unwrap()
            .contains(&format!("Agentfs-Open: {h}\n"))
    );
    assert_eq!(
        fs::read(tmp.path().join("state/trees/root/a")).unwrap(),
        v.read(ROOT, "a").unwrap()
    );
}

#[test]
fn append_finishes_a_partial_line_and_close_replays_on_the_current_head() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"foo");
    let old = v.write_open(ROOT, "a", &writer("old"), false).unwrap();
    v.write(old, 0, b"bar", true).unwrap();
    put(&mut v, ROOT, "a", b"new");
    let before = v.head(ROOT).unwrap();
    v.close(old).unwrap();
    assert_eq!(v.read(ROOT, "a").unwrap(), b"newbar");
    let repo = repo(tmp.path());
    assert_eq!(
        repo.read_commit(v.head(ROOT).unwrap()).unwrap().parents[0],
        before
    );
}

#[test]
fn rename_over_and_unlink_keep_old_handles_orphaned_even_after_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"old");
    let old = v
        .write_open(ROOT, "a", &writer("old-writer"), false)
        .unwrap();
    put(&mut v, ROOT, "temp", b"replacement");
    v.rename(ROOT, "temp", "a", &writer("editor")).unwrap();
    v.write(old, 0, b"BAD", false).unwrap();
    let before = v.head(ROOT).unwrap();
    assert_eq!(v.close(old).unwrap(), Saved::OrphanDropped);
    assert_eq!(v.head(ROOT).unwrap(), before);
    assert_eq!(v.read(ROOT, "a").unwrap(), b"replacement");
    let old = v
        .write_open(ROOT, "a", &writer("old-writer"), false)
        .unwrap();
    v.unlink(ROOT, "a", &writer("editor")).unwrap();
    put(&mut v, ROOT, "a", b"new-object");
    v.write(old, 0, b"BAD", false).unwrap();
    drop(v); // Same recovery entry point as a killed actor; WAL must rebuild objects.
    let v = reopen(tmp.path());
    assert_eq!(v.read(ROOT, "a").unwrap(), b"new-object");
    drop(v);
    let v = reopen(tmp.path());
    assert_eq!(v.read(ROOT, "a").unwrap(), b"new-object");
}

#[test]
fn rename_of_an_open_file_follows_the_object() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"foo");
    let h = v.write_open(ROOT, "a", &writer("writer"), false).unwrap();
    v.write(h, 0, b"bar", true).unwrap();
    v.rename(ROOT, "a", "b", &writer("writer")).unwrap();
    v.close(h).unwrap();
    assert!(matches!(v.read(ROOT, "a"), Err(Error::NotFound)));
    assert_eq!(v.read(ROOT, "b").unwrap(), b"foobar");
}

#[test]
fn restart_closes_abandoned_opens_once_and_repairs_the_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let h = v.write_open(ROOT, "a", &writer("writer"), true).unwrap();
    v.write(h, 0, b"acknowledged", false).unwrap();
    let before = v.head(ROOT).unwrap();
    drop(v);
    let mut v = reopen(tmp.path());
    let saved = v.head(ROOT).unwrap();
    assert_ne!(saved, before);
    assert_eq!(v.read(ROOT, "a").unwrap(), b"acknowledged");
    let next = v.write_open(ROOT, "a", &writer("writer"), false).unwrap();
    assert!(next > h);
    v.close(next).unwrap();
    drop(v);
    fs::write(tmp.path().join("trees/root/a"), b"damaged-cache").unwrap();
    let v = reopen(tmp.path());
    assert_eq!(v.head(ROOT).unwrap(), saved);
    assert_eq!(
        fs::read(tmp.path().join("trees/root/a")).unwrap(),
        b"acknowledged"
    );
}

#[test]
fn fork_saves_land_and_parent_changes_sync_across_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"base");
    let fork = v.fork().unwrap();
    put(&mut v, fork, "a", b"sandbox");
    while v.progress().unwrap() {}
    assert_eq!(v.read(ROOT, "a").unwrap(), b"sandbox");
    put(&mut v, ROOT, "a", b"parent");
    while v.progress().unwrap() {}
    assert_eq!(v.read(fork, "a").unwrap(), b"parent");
    assert_eq!(v.read(ROOT, "a").unwrap(), b"parent");
    let fork_head = v.head(fork).unwrap();
    drop(v);
    let v = reopen(tmp.path());
    assert_eq!(v.head(fork).unwrap(), fork_head);
    assert_eq!(v.read(fork, "a").unwrap(), b"parent");
    assert_eq!(
        fs::read(tmp.path().join("trees").join(fork.to_string()).join("a")).unwrap(),
        b"parent"
    );
}

// Finding 45 (the owner, 2026-11-07, his 4, option a): a landing on the parent's
// current head is a fast-forward. The sandbox's commit is the head, it names who wrote
// it, the journal's record moved the pointer, and a restart finds the same.
#[test]
fn a_landing_on_the_parents_head_is_a_fast_forward_across_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"base\n");
    let fork = v.fork().unwrap();
    let h = v
        .write_open(fork, "a", &writer("sandbox-writer"), true)
        .unwrap();
    v.write(h, 0, b"sandbox\n", true).unwrap();
    v.close(h).unwrap();
    let saved = v.head(fork).unwrap();
    assert_eq!(v.take_next(ROOT).unwrap(), Took::Taken);
    assert_eq!(v.head(ROOT).unwrap(), saved, "the save itself is the head");
    assert_eq!(v.read(ROOT, "a").unwrap(), b"base\nsandbox\n");
    let last =
        String::from_utf8(git(tmp.path(), &["log", "-1", "--format=%B", &ROOT.to_string()]).stdout);
    let last = last.unwrap();
    assert!(last.contains("Agentfs-Writer: sandbox-writer"), "{last}");
    assert!(last.contains(&format!("Agentfs-Tree: {fork}")), "{last}");
    assert!(last.contains("Agentfs-Cause: Direct"), "{last}");
    drop(v);
    let mut v = reopen(tmp.path());
    assert_eq!(v.head(ROOT).unwrap(), saved);
    assert_eq!(v.read(ROOT, "a").unwrap(), b"base\nsandbox\n");
    // The next sync has nothing to bring, and a save on the root still reaches the sandbox.
    while v.progress().unwrap() {}
    assert_eq!(v.head(fork).unwrap(), saved);
    let h = v
        .write_open(ROOT, "a", &writer("main-writer"), true)
        .unwrap();
    v.write(h, 0, b"main\n", true).unwrap();
    v.close(h).unwrap();
    while v.progress().unwrap() {}
    assert_eq!(v.read(fork, "a").unwrap(), b"base\nsandbox\nmain\n");
    assert!(
        repo(tmp.path())
            .is_ancestor(saved, v.head(ROOT).unwrap())
            .unwrap()
    );
}

#[test]
fn sync_delivers_prefix_before_open_file_and_holds_later_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"a0\n");
    put(&mut v, ROOT, "b", b"b0\n");
    put(&mut v, ROOT, "c", b"c0\n");
    let fork = v.fork().unwrap();
    let held = v.write_open(fork, "a", &writer("holder"), false).unwrap();
    put(&mut v, ROOT, "b", b"b1\n");
    while v.progress().unwrap() {}
    assert_eq!(v.read(fork, "b").unwrap(), b"b1\n");
    put(&mut v, ROOT, "a", b"a1\n");
    put(&mut v, ROOT, "c", b"c1\n");
    while v.progress().unwrap() {}
    assert_eq!(v.read(fork, "a").unwrap(), b"a0\n");
    assert_eq!(v.read(fork, "c").unwrap(), b"c0\n");
    assert!(v.metrics().waits > 0);
    v.close(held).unwrap();
    while v.progress().unwrap() {}
    assert_eq!(v.read(fork, "a").unwrap(), b"a1\n");
    assert_eq!(v.read(fork, "c").unwrap(), b"c1\n");
}

#[test]
fn queued_same_source_keeps_order_and_collision_remains_in_history() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"base\n");
    put(&mut v, ROOT, "b", b"base-b\n");
    let fork = v.fork().unwrap();
    let held = v.write_open(ROOT, "a", &writer("parent"), false).unwrap();
    put(&mut v, fork, "a", b"child\n");
    let losing = v.head(fork).unwrap();
    put(&mut v, fork, "b", b"child-b\n");
    while v.progress().unwrap() {}
    assert_eq!(v.read(ROOT, "a").unwrap(), b"base\n");
    assert_eq!(v.read(ROOT, "b").unwrap(), b"base-b\n");
    assert!(v.metrics().waits > 0);
    v.truncate(held, 0).unwrap();
    v.write(held, 0, b"parent\n", false).unwrap();
    v.close(held).unwrap();
    while v.progress().unwrap() {}
    assert_eq!(v.read(ROOT, "a").unwrap(), b"parent\n");
    assert_eq!(v.read(ROOT, "b").unwrap(), b"child-b\n");
    assert_eq!(v.read(fork, "a").unwrap(), b"parent\n");
    let repo = repo(tmp.path());
    assert!(repo.is_ancestor(losing, v.head(ROOT).unwrap()).unwrap());
    let collisions = repo.list_refs("refs/collisions/root/").unwrap();
    assert!(collisions.iter().any(|(_, oid)| *oid == losing));
}

#[test]
fn create_rename_and_unlink_versions_sync_and_land() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let fork = v.fork().unwrap();
    put(&mut v, ROOT, "a", b"content\n");
    while v.progress().unwrap() {}
    assert_eq!(v.read(fork, "a").unwrap(), b"content\n");
    v.rename(ROOT, "a", "b", &writer("parent")).unwrap();
    while v.progress().unwrap() {}
    assert!(matches!(v.read(fork, "a"), Err(Error::NotFound)));
    assert_eq!(v.read(fork, "b").unwrap(), b"content\n");
    v.unlink(ROOT, "b", &writer("parent")).unwrap();
    while v.progress().unwrap() {}
    assert!(matches!(v.read(fork, "b"), Err(Error::NotFound)));
    put(&mut v, fork, "child", b"child\n");
    while v.progress().unwrap() {}
    v.rename(fork, "child", "moved", &writer("child")).unwrap();
    while v.progress().unwrap() {}
    assert_eq!(v.read(ROOT, "moved").unwrap(), b"child\n");
    v.unlink(fork, "moved", &writer("child")).unwrap();
    while v.progress().unwrap() {}
    assert!(matches!(v.read(ROOT, "moved"), Err(Error::NotFound)));
}

#[test]
fn rename_after_rejected_create_keeps_rejected_content_out_of_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let fork = v.fork().unwrap();
    let parent = v.write_open(ROOT, "a", &writer("parent"), true).unwrap();
    put(&mut v, fork, "a", b"child\n");
    let held = v.write_open(fork, "a", &writer("child"), false).unwrap();
    while v.progress().unwrap() {}
    v.write(parent, 0, b"parent\n", false).unwrap();
    v.close(parent).unwrap();
    while v.progress().unwrap() {}
    assert_eq!(v.read(ROOT, "a").unwrap(), b"parent\n");
    assert_eq!(v.read(fork, "a").unwrap(), b"child\n");
    v.rename(fork, "a", "b", &writer("child")).unwrap();
    v.close(held).unwrap();
    while v.progress().unwrap() {}
    assert_eq!(v.read(ROOT, "a").unwrap(), b"parent\n");
    assert_eq!(v.read(ROOT, "b").unwrap(), b"");
}

#[test]
fn single_actor_and_invalid_names_fail_before_recording() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let journal = fs::metadata(tmp.path().join("calls.wal")).unwrap().len();
    assert!(matches!(
        TreeActor::open(tmp.path(), &PodContext::default()),
        Err(Error::Busy)
    ));
    for path in ["../a", "a/b", ".git", ".GiT", "git~1", "", "/tmp"] {
        assert!(matches!(
            v.write_open(ROOT, path, &writer("writer"), true),
            Err(Error::Name)
        ));
    }
    // A writer that would forge a trailer is no writer: refused before any call.
    assert!(WriterLabel::parse("w\nAgentfs-Writer: fake").is_err());
    assert_eq!(
        fs::metadata(tmp.path().join("calls.wal")).unwrap().len(),
        journal
    );
}

#[test]
fn incomplete_tail_is_discarded_but_a_bad_frame_with_a_later_one_is_not() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"saved");
    // Durable, then a later frame: its mark says the bad one was on disk.
    v.durability().make_all_durable().unwrap();
    put(&mut v, ROOT, "b", b"later");
    drop(v);
    let path = tmp.path().join("calls.wal");
    let valid = fs::read(&path).unwrap();
    let mut corrupt_header = valid.clone();
    corrupt_header[frames(&valid)[1].0 + 8] ^= 0x80;
    fs::write(&path, corrupt_header).unwrap();
    assert!(matches!(
        TreeActor::open(tmp.path(), &PodContext::default()),
        Err(Error::CorruptJournal(_))
    ));
    fs::write(&path, &valid).unwrap();
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"AFJ")
        .unwrap();
    let v = reopen(tmp.path());
    assert_eq!(v.read(ROOT, "a").unwrap(), b"saved");
    drop(v);
    assert_eq!(fs::read(tmp.path().join("calls.wal.torn")).unwrap(), b"AFJ");
}

#[test]
fn killed_actor_replays_acknowledged_calls() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    let tmp = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_worker", "--nocapture"])
        .env("AGENTFS_TREE_ACTOR_CRASH_TEST", tmp.path())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(
            output.read_line(&mut line).unwrap(),
            0,
            "worker exited before acknowledgement"
        );
        if line.trim() == "ACK" {
            break;
        }
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let v = reopen(tmp.path());
    assert_eq!(v.read(ROOT, "a").unwrap(), b"returned-write");
    let head = v.head(ROOT).unwrap();
    drop(v);
    assert_eq!(reopen(tmp.path()).head(ROOT).unwrap(), head);
}

#[test]
fn crash_worker() {
    let Some(root) = std::env::var_os("AGENTFS_TREE_ACTOR_CRASH_TEST") else {
        return;
    };
    let mut v = new_actor(std::path::Path::new(&root));
    let h = v
        .write_open(ROOT, "a", &writer("killed-client"), true)
        .unwrap();
    v.write(h, 0, b"returned-write", false).unwrap();
    println!("ACK");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn incoming_object_survives_kill_and_replays_later_open() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    let tmp = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "incoming_crash_worker", "--nocapture"])
        .env("AGENTFS_INCOMING_CRASH_TEST", tmp.path())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(
            output.read_line(&mut line).unwrap(),
            0,
            "worker exited before acknowledgement"
        );
        if line.trim() == "ACK" {
            break;
        }
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let mut volume = reopen(tmp.path());
    while volume.progress().unwrap() {}
    assert_eq!(
        volume.read(ROOT, "incoming").unwrap(),
        b"from child; after landing"
    );
    drop(volume);
    let volume = reopen(tmp.path());
    assert_eq!(
        volume.read(ROOT, "incoming").unwrap(),
        b"from child; after landing"
    );
}

#[test]
fn incoming_crash_worker() {
    let Some(root) = std::env::var_os("AGENTFS_INCOMING_CRASH_TEST") else {
        return;
    };
    let mut volume = new_actor(std::path::Path::new(&root));
    let fork = volume.fork().unwrap();
    put(&mut volume, fork, "incoming", b"from child");
    while volume.progress().unwrap() {}
    assert_eq!(volume.read(ROOT, "incoming").unwrap(), b"from child");
    let handle = volume
        .write_open(ROOT, "incoming", &writer("writer"), false)
        .unwrap();
    volume.write(handle, 0, b"; after landing", true).unwrap();
    println!("ACK");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn concurrent_writers_are_not_reserved_against_an_arbitrary_size_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let a = v.write_open(ROOT, "a", &writer("one"), true).unwrap();
    let b = v.write_open(ROOT, "a", &writer("two"), false).unwrap();
    let size = 100 * 1024 * 1024;
    v.truncate(a, size).unwrap();
    v.write(b, 0, b"x", true).unwrap();
    v.close(a).unwrap();
    v.close(b).unwrap();
    let bytes = v.read(ROOT, "a").unwrap();
    assert_eq!(bytes.len(), size as usize + 1);
    assert_eq!(bytes.last(), Some(&b'x'));
}

#[test]
fn save_and_checkout_failures_keep_calls_and_recover_without_duplicate_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"base");
    v.settle().unwrap();
    let h = v.write_open(ROOT, "a", &writer("writer"), false).unwrap();
    v.write(h, 0, b"journaled", true).unwrap();
    // A stale ref lock stands in for an interrupted Git ref update.
    fs::write(
        tmp.path().join("history.git/refs/heads/root.lock"),
        b"stale",
    )
    .unwrap();
    assert!(matches!(v.close(h).unwrap(), Saved::Commit(_)));
    assert!(!v.is_stopped());
    assert_eq!(v.read(ROOT, "a").unwrap(), b"basejournaled");
    let recorded = v.head(ROOT).unwrap();
    assert_ne!(
        repo(tmp.path()).head("refs/heads/root").unwrap(),
        Some(recorded)
    );
    drop(v);
    let mut v = reopen(tmp.path());
    assert_eq!(v.head(ROOT).unwrap(), recorded);
    assert_eq!(v.read(ROOT, "a").unwrap(), b"basejournaled");
    let h = v.write_open(ROOT, "a", &writer("writer"), false).unwrap();
    v.write(h, 0, b"once", true).unwrap();
    fs::remove_dir_all(tmp.path().join("trees/root")).unwrap();
    fs::write(tmp.path().join("trees/root"), b"checkout-blocked").unwrap();
    assert!(matches!(v.close(h).unwrap(), Saved::Commit(_)));
    v.settle().unwrap(); // Its checkout failed.
    let repo = repo(tmp.path());
    let committed = repo.head("refs/heads/root").unwrap().unwrap();
    assert_eq!(committed, v.head(ROOT).unwrap());
    drop(v);
    fs::remove_file(tmp.path().join("trees/root")).unwrap();
    let v = reopen(tmp.path());
    assert_eq!(v.head(ROOT).unwrap(), committed);
    assert_eq!(v.read(ROOT, "a").unwrap(), b"basejournaledonce");
    assert_eq!(
        fs::read(tmp.path().join("trees/root/a")).unwrap(),
        b"basejournaledonce"
    );
    assert_eq!(opens_saved(tmp.path(), h), 1);
}

#[test]
fn anonymous_object_first_link_and_recovery_preserve_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let object;
    {
        let mut v = new_actor(tmp.path());
        let h = v.anonymous_open(ROOT, &writer("writer")).unwrap();
        object = v.open_object(h).unwrap();
        v.write(h, 0, b"saved on recovery", false).unwrap();
        assert!(v.files(ROOT).unwrap().is_empty());
        v.first_link(ROOT, object, "named", &writer("writer"))
            .unwrap();
        assert_eq!(v.read(ROOT, "named").unwrap(), b"");
        assert_eq!(v.files(ROOT).unwrap()[0].1, object);
        // Drop with the handle still open; restart replays the journal and closes it.
    }
    let mut v = reopen(tmp.path());
    assert_eq!(v.read(ROOT, "named").unwrap(), b"saved on recovery");
    assert_eq!(v.files(ROOT).unwrap()[0].1, object);
    assert!(matches!(
        v.first_link(ROOT, object, "other", &writer("writer")),
        Err(Error::NotFound)
    ));
    let h = v.anonymous_open(ROOT, &writer("writer")).unwrap();
    v.write(h, 0, b"unlinked", false).unwrap();
    assert_eq!(v.close(h).unwrap(), Saved::Unchanged);
    assert_eq!(v.files(ROOT).unwrap().len(), 1);
}

#[test]
fn saved_line_map_and_bytes_share_a_commit_without_a_reserved_name() {
    let tmp = tempfile::tempdir().unwrap();
    let mut volume = new_actor(tmp.path());
    put(&mut volume, ROOT, "metadata.json", b"first\npartial");
    let handle = volume
        .write_open(ROOT, "metadata.json", &writer("writer"), false)
        .unwrap();
    volume.write(handle, 0, b" tail\nnext\n", true).unwrap();
    volume.close(handle).unwrap();
    let repo = repo(tmp.path());
    let root = repo.read_commit(volume.head(ROOT).unwrap()).unwrap().tree;
    let files = entry(&repo, root, "files");
    let blob = repo
        .read_blob(entry(&repo, files, "metadata.json"))
        .unwrap();
    assert_eq!(blob, b"first\npartial tail\nnext\n");
    let metadata = repo.read_blob(entry(&repo, root, "metadata.json")).unwrap();
    let maps: std::collections::BTreeMap<
        String,
        (u64, u64, agentfs_core::tree_reconcile::LineMap),
    > = serde_json::from_slice(&metadata).unwrap();
    maps["metadata.json"].2.validate(&blob).unwrap();
    assert_eq!(
        fs::read(tmp.path().join("trees/root/metadata.json")).unwrap(),
        blob
    );
}

#[test]
fn atomic_replacement_keeps_merge_identity_but_changes_object_across_recovery() {
    fn metadata(
        root: &std::path::Path,
        head: git2::Oid,
    ) -> (u64, u64, agentfs_core::tree_reconcile::LineMap) {
        let repo = repo(root);
        let tree = repo.read_commit(head).unwrap().tree;
        let blob = repo.read_blob(entry(&repo, tree, "metadata.json")).unwrap();
        let mut maps: std::collections::BTreeMap<
            String,
            (u64, u64, agentfs_core::tree_reconcile::LineMap),
        > = serde_json::from_slice(&blob).unwrap();
        maps.remove("target").unwrap()
    }
    let tmp = tempfile::tempdir().unwrap();
    let mut volume = new_actor(tmp.path());
    put(&mut volume, ROOT, "target", b"first\nsecond\n");
    let old = volume
        .write_open(ROOT, "target", &writer("old"), false)
        .unwrap();
    let before = metadata(tmp.path(), volume.head(ROOT).unwrap());
    put(&mut volume, ROOT, "temp", b"first\nchanged\n");
    volume
        .rename(ROOT, "temp", "target", &writer("editor"))
        .unwrap();
    let after = metadata(tmp.path(), volume.head(ROOT).unwrap());
    assert_ne!(before.0, after.0);
    assert_eq!(before.1, after.1);
    assert_eq!(before.2.lines[0].identity, after.2.lines[0].identity);
    assert_eq!(before.2.lines[1].identity, after.2.lines[1].identity);
    assert_ne!(before.2.lines[1].version, after.2.lines[1].version);
    volume.write(old, 0, b"stale", false).unwrap();
    assert_eq!(volume.close(old).unwrap(), Saved::OrphanDropped);
    drop(volume);
    let volume = reopen(tmp.path());
    assert_eq!(volume.read(ROOT, "target").unwrap(), b"first\nchanged\n");
    assert_eq!(metadata(tmp.path(), volume.head(ROOT).unwrap()), after);
}

#[test]
fn sync_fast_forwards_after_atomic_replacement_with_different_objects() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "target", b"old\n");
    let fork = v.fork().unwrap();
    put(&mut v, fork, "temp", b"new\n");
    v.rename(fork, "temp", "target", &writer("editor")).unwrap();
    let fork_object = v
        .files(fork)
        .unwrap()
        .into_iter()
        .find(|(p, _, _)| p == "target")
        .unwrap()
        .1;
    let parent_object = v
        .files(ROOT)
        .unwrap()
        .into_iter()
        .find(|(p, _, _)| p == "target")
        .unwrap()
        .1;
    assert_ne!(fork_object, parent_object);
    let held = v
        .write_open(fork, "target", &writer("writer"), false)
        .unwrap();
    while v.progress().unwrap() {}
    assert_eq!(v.read(ROOT, "target").unwrap(), b"new\n");
    assert_eq!(v.head(fork).unwrap(), v.head(ROOT).unwrap());
    assert_eq!(
        v.files(fork)
            .unwrap()
            .into_iter()
            .find(|(p, _, _)| p == "target")
            .unwrap()
            .1,
        fork_object
    );
    v.write(held, 0, b"later\n", true).unwrap();
    drop(v);
    let mut v = reopen(tmp.path());
    while v.progress().unwrap() {}
    assert_eq!(v.read(fork, "target").unwrap(), b"new\nlater\n");
    assert_eq!(v.read(ROOT, "target").unwrap(), b"new\nlater\n");
}

#[test]
fn branch_moves_persist_ordered_queue_entries_across_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut volume = new_actor(tmp.path());
    put(&mut volume, ROOT, "shared", b"base\n");
    let fork = volume.fork().unwrap();
    let held = volume
        .write_open(ROOT, "shared", &writer("holder"), false)
        .unwrap();
    put(&mut volume, fork, "shared", b"child\n");
    while volume.progress().unwrap() {}
    let child_head = volume.head(fork).unwrap();
    let repo = repo(tmp.path());
    let parent_queue = repo.list_refs("refs/queue/root/").unwrap();
    assert!(!parent_queue.is_empty());
    assert_eq!(parent_queue.last().unwrap().1, child_head);
    assert!(volume.metrics().waits > 0);
    assert!(volume.metrics().queued >= parent_queue.len() as u64);
    assert_eq!(volume.read(ROOT, "shared").unwrap(), b"base\n");
    let _ = held;
    drop(volume);
    let mut volume = reopen(tmp.path());
    while volume.progress().unwrap() {}
    assert_eq!(volume.read(ROOT, "shared").unwrap(), b"child\n");
    assert!(repo.list_refs("refs/queue/root/").unwrap().is_empty());
    assert!(
        repo.is_ancestor(child_head, volume.head(ROOT).unwrap())
            .unwrap()
    );
}

#[test]
fn name_history_keeps_rename_and_unlink_versions_in_the_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let mut volume = new_actor(tmp.path());
    put(&mut volume, ROOT, "a", b"content\n");
    volume.rename(ROOT, "a", "b", &writer("writer")).unwrap();
    volume.unlink(ROOT, "b", &writer("writer")).unwrap();
    let repo = repo(tmp.path());
    let root = repo.read_commit(volume.head(ROOT).unwrap()).unwrap().tree;
    let data = repo
        .read_blob(entry(&repo, root, "namespace.json"))
        .unwrap();
    let histories: serde_json::Value = serde_json::from_slice(&data).unwrap();
    let history = histories.as_object().unwrap().values().next().unwrap();
    let versions = history["versions"].as_object().unwrap();
    assert_eq!(versions.len(), 3);
    let paths: Vec<_> = versions.values().map(|v| &v["path"]).collect();
    assert!(paths.contains(&&serde_json::json!("a")));
    assert!(paths.contains(&&serde_json::json!("b")));
    assert!(paths.contains(&&serde_json::Value::Null));
    drop(volume);
    let volume = reopen(tmp.path());
    assert!(matches!(volume.read(ROOT, "b"), Err(Error::NotFound)));
    let root = repo.read_commit(volume.head(ROOT).unwrap()).unwrap().tree;
    assert_eq!(
        repo.read_blob(entry(&repo, root, "namespace.json"))
            .unwrap(),
        data
    );
}

// The journal is the commit point; Git's refs are a copy (git-model §12).

fn all_refs(root: &std::path::Path) -> Vec<(String, git2::Oid)> {
    repo(root).list_refs("refs/").unwrap()
}

#[test]
fn a_corrupt_copy_is_rebuilt_from_the_journal() {
    fn scribble(dir: &std::path::Path) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                scribble(&path);
            } else {
                fs::write(path, b"garbage\n").unwrap();
            }
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let fork = v.fork().unwrap();
    put(&mut v, ROOT, "a", b"main\n");
    put(&mut v, fork, "b", b"fork\n");
    v.settle().unwrap();
    let refs = all_refs(tmp.path());
    assert!(refs.iter().any(|(name, _)| name.starts_with("refs/queue/")));
    drop(v);
    let gitdir = tmp.path().join("history.git");
    scribble(&gitdir.join("refs"));
    fs::write(gitdir.join("HEAD"), b"garbage\n").unwrap();
    let v = reopen(tmp.path());
    assert_eq!(all_refs(tmp.path()), refs);
    assert_eq!(v.read(ROOT, "a").unwrap(), b"main\n");
    assert_eq!(v.read(fork, "b").unwrap(), b"fork\n");
    let fsck = git(tmp.path(), &["fsck", "--full", "--strict", "--no-progress"]);
    assert!(fsck.status.success(), "{fsck:?}");
}

#[test]
fn removed_refs_are_rebuilt_from_the_journal() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"main\n");
    v.settle().unwrap();
    let refs = all_refs(tmp.path());
    drop(v);
    fs::remove_dir_all(tmp.path().join("history.git/refs")).unwrap();
    let v = reopen(tmp.path());
    assert_eq!(all_refs(tmp.path()), refs);
    assert_eq!(v.read(ROOT, "a").unwrap(), b"main\n");
}

#[test]
fn an_emptied_config_is_rewritten() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let fork = v.fork().unwrap();
    drop(v);
    fs::write(tmp.path().join("history.git/config"), b"").unwrap();
    fs::write(tmp.path().join("history.git/HEAD"), b"").unwrap();
    let v = reopen(tmp.path());
    assert_eq!(v.trees(), [ROOT, TreeId::Fork(1)]);
    let get =
        |key: &str| String::from_utf8(git(tmp.path(), &["config", "--get", key]).stdout).unwrap();
    assert_eq!(get("core.fsync"), "objects,derived-metadata\n");
    assert_eq!(get("core.fsyncMethod"), "fsync\n");
    assert_eq!(get(&format!("branch.{fork}.merge")), "refs/heads/root\n");
}

#[test]
fn recovery_reads_pointers_from_the_journal() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"first");
    let first = v.head(ROOT).unwrap();
    put(&mut v, ROOT, "a", b"second");
    let second = v.head(ROOT).unwrap();
    drop(v);
    assert!(
        git(
            tmp.path(),
            &["update-ref", "refs/heads/root", &first.to_string()]
        )
        .status
        .success()
    );
    let v = reopen(tmp.path());
    assert_eq!(v.head(ROOT).unwrap(), second);
    assert_eq!(v.read(ROOT, "a").unwrap(), b"second");
    assert_eq!(
        repo(tmp.path()).head("refs/heads/root").unwrap(),
        Some(second)
    );
}

#[test]
fn a_ref_copy_failure_is_not_a_call_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"base");
    let before = v.head(ROOT).unwrap();
    let h = v.write_open(ROOT, "a", &writer("writer"), false).unwrap();
    v.write(h, 0, b"!", true).unwrap();
    let lock = tmp.path().join("history.git/refs/heads/root.lock");
    fs::write(&lock, b"stale").unwrap();
    assert!(matches!(v.close(h).unwrap(), Saved::Commit(_)));
    assert!(!v.is_stopped());
    let recorded = v.head(ROOT).unwrap();
    assert_ne!(recorded, before);
    assert_eq!(v.read(ROOT, "a").unwrap(), b"base!");
    let copy = repo(tmp.path());
    assert_eq!(copy.head("refs/heads/root").unwrap(), Some(before));
    // The next batch rebuilds the copy whole and redoes the checkout.
    put(&mut v, ROOT, "b", b"next");
    v.settle().unwrap();
    assert_eq!(
        copy.head("refs/heads/root").unwrap(),
        Some(v.head(ROOT).unwrap())
    );
    assert!(copy.is_ancestor(recorded, v.head(ROOT).unwrap()).unwrap());
    assert!(!lock.exists());
    assert_eq!(fs::read(tmp.path().join("trees/root/a")).unwrap(), b"base!");
    assert_eq!(fs::read(tmp.path().join("trees/root/b")).unwrap(), b"next");
}

#[test]
fn a_bad_final_frame_is_a_torn_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let h = v.write_open(ROOT, "a", &writer("writer"), true).unwrap();
    v.write(h, 0, b"returned", false).unwrap();
    v.write(h, 8, b"-torn", false).unwrap();
    drop(v);
    let path = tmp.path().join("calls.wal");
    let mut journal = fs::read(&path).unwrap();
    let last = journal.len() - 1;
    journal[last] ^= 1;
    let (offset, length) = *frames(&journal).last().unwrap();
    fs::write(&path, &journal).unwrap();
    let v = reopen(tmp.path());
    // The last record is dropped; every one before it stands, closed once.
    assert_eq!(v.read(ROOT, "a").unwrap(), b"returned");
    assert_eq!(opens_saved(tmp.path(), h), 1);
    assert_eq!(
        fs::read(tmp.path().join("calls.wal.torn")).unwrap(),
        journal[offset..offset + length]
    );
}

#[test]
fn a_bad_frame_before_a_good_one_is_corruption() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let h = v.write_open(ROOT, "a", &writer("writer"), true).unwrap();
    v.write(h, 0, b"one", false).unwrap();
    // Durable before the next frame is written: that frame's mark says so.
    v.durability().make_all_durable().unwrap();
    v.write(h, 3, b"two", false).unwrap();
    drop(v);
    let path = tmp.path().join("calls.wal");
    let journal = fs::read(&path).unwrap();
    let all = frames(&journal);
    let (offset, length) = all[all.len() - 2];
    // A flip in the body, then one in the header: both before a good frame.
    for at in [offset + length - 1, offset + 9] {
        let mut bad = journal.clone();
        bad[at] ^= 1;
        fs::write(&path, &bad).unwrap();
        assert!(matches!(
            TreeActor::open(tmp.path(), &PodContext::default()),
            Err(Error::CorruptJournal(found)) if found == offset
        ));
        assert_eq!(fs::read(&path).unwrap(), bad);
    }
    fs::write(&path, &journal).unwrap();
    assert_eq!(reopen(tmp.path()).read(ROOT, "a").unwrap(), b"onetwo");
}

#[test]
fn a_frame_in_a_workloads_bytes_is_not_a_frame() {
    // A whole valid frame of some other journal, with a high number.
    fn forged(salt: u32) -> Vec<u8> {
        let head = br#"{"at_ns":0,"entry":{"AnonymousOpen":{"tree":ROOT,"writer":"w"}}}"#;
        let mut frame = b"AFJ3".to_vec();
        frame.extend_from_slice(&salt.to_le_bytes());
        frame.extend_from_slice(&999_999_u64.to_le_bytes());
        // A mark past every record: were it this journal's, a hole would be corruption.
        frame.extend_from_slice(&999_999_u64.to_le_bytes());
        frame.extend_from_slice(&0_u32.to_le_bytes());
        frame.extend_from_slice(&(head.len() as u32).to_le_bytes());
        frame.extend_from_slice(&0_u32.to_le_bytes());
        frame.extend_from_slice(&crc32fast::hash(head).to_le_bytes());
        let header_crc = crc32fast::hash(&frame);
        frame.extend_from_slice(&header_crc.to_le_bytes());
        frame.extend_from_slice(head);
        frame
    }
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let path = tmp.path().join("calls.wal");
    let salt = u32::from_le_bytes(fs::read(&path).unwrap()[4..8].try_into().unwrap());
    let h = v.write_open(ROOT, "a", &writer("writer"), true).unwrap();
    v.write(h, 0, b"returned", false).unwrap();
    let mut bytes = b"workload ".to_vec();
    bytes.extend_from_slice(&forged(salt ^ 1));
    bytes.extend_from_slice(b" bytes");
    v.write(h, 8, &bytes, false).unwrap();
    drop(v);
    // Whole, the frame reads as what it is: the workload's bytes.
    let saved = {
        let copy = tempfile::tempdir().unwrap();
        copy_dir(tmp.path(), copy.path());
        let v = reopen(copy.path());
        v.read(ROOT, "a").unwrap().to_vec()
    };
    assert_eq!(saved, [b"returned".as_slice(), &bytes].concat());
    // Torn, the forged frame cannot make its tail look like corruption.
    let journal = fs::read(&path).unwrap();
    fs::write(&path, &journal[..journal.len() - 1]).unwrap();
    let v = reopen(tmp.path());
    assert_eq!(v.read(ROOT, "a").unwrap(), b"returned");
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}

#[test]
fn a_lost_object_at_a_tip_refuses_to_serve() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"will be lost");
    let head = v.head(ROOT).unwrap();
    drop(v);
    // The builder's objects are in packs: lose them all.
    for entry in fs::read_dir(tmp.path().join("history.git/objects/pack")).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    assert!(matches!(
        TreeActor::open(tmp.path(), &PodContext::default()),
        Err(Error::MissingObject { name, .. }) if name.starts_with("refs/")
    ));
    let _ = head;
}

#[test]
fn a_missing_journal_refuses_and_keeps_history() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"history");
    let head = v.head(ROOT).unwrap();
    drop(v);
    fs::remove_file(tmp.path().join("calls.wal")).unwrap();
    assert!(matches!(
        TreeActor::open(tmp.path(), &PodContext::default()),
        Err(Error::CorruptJournal(0))
    ));
    fs::write(tmp.path().join("calls.wal"), b"").unwrap();
    assert!(matches!(
        TreeActor::open(tmp.path(), &PodContext::default()),
        Err(Error::CorruptJournal(0))
    ));
    assert_eq!(
        repo(tmp.path()).head("refs/heads/root").unwrap(),
        Some(head)
    );
    assert!(tmp.path().join("format").exists());
    fs::remove_file(tmp.path().join("format")).unwrap();
    assert!(matches!(
        TreeActor::open(tmp.path(), &PodContext::default()),
        Err(Error::Format)
    ));
    assert_eq!(
        repo(tmp.path()).head("refs/heads/root").unwrap(),
        Some(head)
    );
}

#[test]
fn an_earlier_format_does_not_open() {
    let tmp = tempfile::tempdir().unwrap();
    drop(new_actor(tmp.path()));
    fs::write(tmp.path().join("format"), b"agentfs-trees-1\n").unwrap();
    assert!(matches!(
        TreeActor::open(tmp.path(), &PodContext::default()),
        Err(Error::Format)
    ));
}

#[test]
fn open_numbers_never_repeat_across_compaction() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let first = put(&mut v, ROOT, "a", b"one");
    drop(v);
    // Every open compacts; the journal then holds only a checkpoint.
    drop(reopen(tmp.path()));
    // Nothing but the checkpoint can say what the next number is now.
    assert_eq!(
        frames(&fs::read(tmp.path().join("calls.wal")).unwrap()).len(),
        1
    );
    let mut v = reopen(tmp.path());
    let second = v.write_open(ROOT, "a", &writer("writer"), false).unwrap();
    assert!(second > first);
    v.write(second, 0, b"two", true).unwrap();
    v.close(second).unwrap();
    drop(v);
    let mut v = reopen(tmp.path());
    let third = v.write_open(ROOT, "a", &writer("writer"), false).unwrap();
    assert!(third > second);
    assert_eq!(opens_saved(tmp.path(), second), 1);
}

/// Takes the journal past the compaction threshold through real calls, leaving
/// `FILLED` in the file so its save stays cheap. A compaction waits for the
/// first call after the builder caught up: an open (closed, saving nothing) is
/// that call.
const FILLED: &[u8] = b"xxxxxx";
fn fill_journal(v: &mut TreeActor, path: &str) {
    let h = v.write_open(ROOT, path, &writer("filler"), true).unwrap();
    let chunk = vec![b'x'; 33 << 20];
    for _ in 0..2 {
        v.write(h, 0, &chunk, true).unwrap();
    }
    v.truncate(h, FILLED.len() as u64).unwrap();
    v.close(h).unwrap();
    v.settle().unwrap();
    // A compaction failing after its rename stops the actor: this open fails.
    if let Ok(h) = v.write_open(ROOT, path, &writer("filler"), false) {
        v.close(h).unwrap();
    }
}

#[test]
fn a_log_held_open_does_not_pin_other_saves() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let log = v.write_open(ROOT, "log", &writer("logger"), true).unwrap();
    v.write(log, 0, b"before\n", true).unwrap();
    fill_journal(&mut v, "big");
    // The step compacted: the other file's calls are gone, the log's stayed.
    let length = fs::metadata(tmp.path().join("calls.wal")).unwrap().len();
    assert!(length < 1 << 20, "journal holds {length} bytes");
    v.write(log, 0, b"after\n", true).unwrap();
    drop(v);
    let v = reopen(tmp.path());
    assert_eq!(v.read(ROOT, "log").unwrap(), b"before\nafter\n");
    assert_eq!(v.read(ROOT, "big").unwrap(), FILLED);
    assert_eq!(opens_saved(tmp.path(), log), 1);
}

// Run a worker test in a child process with this failpoint spec, and wait for
// the failpoint to kill it. The child prints the open it was closing.
#[cfg(feature = "failpoints")]
fn run_killed_worker(worker: &str, root: &std::path::Path, spec: &str) -> u64 {
    use std::process::{Command, Stdio};
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", worker, "--nocapture"])
        .env("AGENTFS_TREE_STEP_WORKER", root)
        .env(agentfs_core::failpoints::ENV_VAR, spec)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        output.status.signal(),
        Some(libc::SIGKILL),
        "{spec} did not kill the worker: {output:?}"
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("OPEN "))
        .unwrap()
        .parse()
        .unwrap()
}

#[cfg(feature = "failpoints")]
#[test]
fn step_worker() {
    let Some(root) = std::env::var_os("AGENTFS_TREE_STEP_WORKER") else {
        return;
    };
    let mut v = new_actor(std::path::Path::new(&root));
    let h = v.write_open(ROOT, "a", &writer("writer"), true).unwrap();
    // The create's batch, alone: the write's and the close's is the second.
    v.settle().unwrap();
    v.write(h, 0, b"returned", false).unwrap();
    println!("OPEN {h}");
    std::io::stdout().flush().unwrap();
    v.close(h).unwrap();
    v.settle().unwrap();
    unreachable!("the failpoint kills this process");
}

#[cfg(feature = "failpoints")]
#[test]
fn compact_worker() {
    let Some(root) = std::env::var_os("AGENTFS_TREE_STEP_WORKER") else {
        return;
    };
    let mut v = new_actor(std::path::Path::new(&root));
    let log = v.write_open(ROOT, "log", &writer("logger"), true).unwrap();
    v.write(log, 0, b"returned\n", true).unwrap();
    println!("OPEN {log}");
    std::io::stdout().flush().unwrap();
    fill_journal(&mut v, "big");
    unreachable!("the failpoint kills this process");
}

/// Finding 36 (the owner, 2026-11-07, "1. A"): a save that leaves the bytes as they
/// were commits nothing and queues nothing, in the root and in a sandbox, whichever
/// calls made it. The close still ends the open, so a sync waiting on it goes, and a
/// crash with such an open replays to the same result.
#[test]
fn a_save_that_changes_no_byte_commits_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let state = tmp.path().join("state");
    let mut v = new_actor(&state);
    let fork = v.fork().unwrap();
    let h = v.write_open(ROOT, "a", &writer("w"), true).unwrap();
    v.write(h, 0, b"same\n", false).unwrap();
    v.close(h).unwrap();
    while v.progress().unwrap() {}
    assert_eq!(v.read(fork, "a").unwrap(), b"same\n");
    let before = (v.head(ROOT).unwrap(), v.head(fork).unwrap());
    let queues = || {
        let out = git(&state, &["for-each-ref", "refs/queue/"]);
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    };
    assert_eq!(queues(), "");
    for tree in [ROOT, fork] {
        let h = v.write_open(tree, "a", &writer("w"), false).unwrap();
        v.truncate(h, 0).unwrap();
        v.write(h, 0, b"same\n", false).unwrap();
        assert_eq!(v.close(h).unwrap(), Saved::Unchanged, "{tree}: a rewrite");
        assert!(matches!(v.read_open(h), Err(Error::NotFound)), "{tree}");
        let h = v.write_open(tree, "a", &writer("w"), false).unwrap();
        v.write(h, 0, b"sa", false).unwrap();
        assert_eq!(v.close(h).unwrap(), Saved::Unchanged, "{tree}: an offset");
        let h = v.write_open(tree, "a", &writer("w"), false).unwrap();
        v.write(h, 0, b"", true).unwrap();
        assert_eq!(v.close(h).unwrap(), Saved::Unchanged, "{tree}: an append");
    }
    assert_eq!(
        before,
        (v.head(ROOT).unwrap(), v.head(fork).unwrap()),
        "no commit"
    );
    assert_eq!(queues(), "", "no queue entry");

    // The close ends the open: the sync that waits for it goes.
    let held = v.write_open(fork, "a", &writer("w"), false).unwrap();
    v.write(held, 0, b"same\n", false).unwrap();
    let h = v.write_open(ROOT, "a", &writer("w"), false).unwrap();
    v.write(h, 0, b"more\n", true).unwrap();
    v.close(h).unwrap();
    assert_eq!(v.take_next(fork).unwrap(), Took::Waiting);
    assert_eq!(v.close(held).unwrap(), Saved::Unchanged);
    assert_eq!(v.take_next(fork).unwrap(), Took::Taken);
    assert_eq!(v.read(fork, "a").unwrap(), b"same\nmore\n");

    // A crash with such an open: the journal replays it to the same result, once.
    let at = (v.head(ROOT).unwrap(), v.head(fork).unwrap());
    let open = v.write_open(fork, "a", &writer("w"), false).unwrap();
    v.truncate(open, 0).unwrap();
    v.write(open, 0, b"same\nmore\n", false).unwrap();
    drop(v);
    let v = reopen(&state);
    assert_eq!(at, (v.head(ROOT).unwrap(), v.head(fork).unwrap()));
    assert_eq!(opens_saved(&state, open), 0);
    assert_eq!(queues(), "");
    assert!(matches!(v.read_open(open), Err(Error::NotFound)));
    drop(v);
    let v = reopen(&state);
    assert_eq!(at, (v.head(ROOT).unwrap(), v.head(fork).unwrap()));
    assert_eq!(v.read(fork, "a").unwrap(), b"same\nmore\n");
}

#[cfg(feature = "failpoints")]
#[test]
fn a_step_killed_before_its_record_saves_at_restart() {
    let tmp = tempfile::tempdir().unwrap();
    // The create is the first hit; the close is the second.
    let spec = "tree.before_step=kill@2";
    let state = tmp.path().join(spec);
    let open = run_killed_worker("step_worker", &state, spec);
    let v = reopen(&state);
    assert_eq!(v.read(ROOT, "a").unwrap(), b"returned", "{spec}");
    let head = v.head(ROOT).unwrap();
    assert_eq!(opens_saved(&state, open), 1, "{spec}");
    drop(v);
    let v = reopen(&state);
    assert_eq!(v.head(ROOT).unwrap(), head, "{spec}");
    assert_eq!(opens_saved(&state, open), 1, "{spec}");
}

#[cfg(feature = "failpoints")]
#[test]
fn a_step_killed_after_its_record_is_not_saved_again() {
    let tmp = tempfile::tempdir().unwrap();
    // The create's batch is the first; the close's is the second.
    for spec in [
        "tree.build_before_batch=kill@2",
        "tree.build_after_pack=kill@2",
        "tree.build_after_built_log=kill@2",
        "tree.build_after_ref_copy=kill@2",
    ] {
        let state = tmp.path().join(spec);
        let open = run_killed_worker("step_worker", &state, spec);
        let v = reopen(&state);
        assert_eq!(v.read(ROOT, "a").unwrap(), b"returned", "{spec}");
        assert_eq!(opens_saved(&state, open), 1, "{spec}");
        assert_eq!(
            repo(&state).head("refs/heads/root").unwrap(),
            Some(v.head(ROOT).unwrap()),
            "{spec}"
        );
        assert_eq!(
            fs::read(state.join("trees/root/a")).unwrap(),
            b"returned",
            "{spec}"
        );
    }
}

// A fast-forward killed on either side of its commit point: before, the entry is still
// queued and is taken after the restart; after, the journal's record moved the root. Either
// way the sandbox's commit is the root's head, once.
#[cfg(feature = "failpoints")]
#[test]
fn a_fast_forward_killed_at_any_step_lands_once() {
    let tmp = tempfile::tempdir().unwrap();
    // Steps and their batches: the fork, the link, the close, then the landing: the fourth
    // pass over each failpoint.
    for spec in [
        "tree.before_step=kill@4",
        "tree.build_before_batch=kill@4",
        "tree.build_after_pack=kill@4",
        "tree.build_after_built_log=kill@4",
        "tree.build_after_ref_copy=kill@4",
    ] {
        let state = tmp.path().join(spec);
        run_killed_worker("fast_forward_worker", &state, spec);
        let mut v = reopen(&state);
        while v.progress().unwrap() {}
        let fork = v.trees().into_iter().find(|tree| !tree.is_root()).unwrap();
        assert_eq!(v.read(ROOT, "a").unwrap(), b"sandbox", "{spec}");
        assert_eq!(v.read(fork, "a").unwrap(), b"sandbox", "{spec}");
        assert_eq!(v.head(ROOT).unwrap(), v.head(fork).unwrap(), "{spec}");
        let merges = git(
            &state,
            &["rev-list", "--count", "--merges", &ROOT.to_string()],
        );
        assert_eq!(
            String::from_utf8(merges.stdout).unwrap().trim(),
            "0",
            "{spec}"
        );
    }
}

#[cfg(feature = "failpoints")]
#[test]
fn fast_forward_worker() {
    let Some(root) = std::env::var_os("AGENTFS_TREE_STEP_WORKER") else {
        return;
    };
    let mut v = new_actor(std::path::Path::new(&root));
    // One batch per step: the fork, the link, the close, the landing.
    let fork = v.fork().unwrap();
    v.settle().unwrap();
    let h = v.anonymous_open(fork, &writer("w")).unwrap();
    v.write(h, 0, b"sandbox", false).unwrap();
    v.first_link(fork, v.open_object(h).unwrap(), "a", &writer("w"))
        .unwrap();
    v.settle().unwrap();
    println!("OPEN {h}");
    v.close(h).unwrap();
    v.settle().unwrap();
    assert_eq!(v.take_next(ROOT).unwrap(), Took::Taken);
    v.settle().unwrap();
    panic!("a failpoint should have killed this worker");
}

#[cfg(feature = "failpoints")]
#[test]
fn a_kill_at_either_compact_failpoint_loses_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    // The compaction at open is the first hit; the one at the step, the second.
    for spec in [
        "tree.compact_before_rename=kill@2",
        "tree.compact_after_rename=kill@2",
    ] {
        let state = tmp.path().join(spec);
        let log = run_killed_worker("compact_worker", &state, spec);
        let v = reopen(&state);
        assert_eq!(v.read(ROOT, "log").unwrap(), b"returned\n", "{spec}");
        assert_eq!(v.read(ROOT, "big").unwrap(), FILLED, "{spec}");
        assert_eq!(opens_saved(&state, log), 1, "{spec}");
    }
}

#[cfg(feature = "failpoints")]
#[test]
fn a_failure_after_the_rename_stops_the_actor() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"before");
    let guard = agentfs_core::failpoints::configure("tree.compact_after_rename=error").unwrap();
    fill_journal(&mut v, "big");
    drop(guard);
    // The step itself stands: its record is in the old file and the new one.
    assert!(v.is_stopped());
    assert!(matches!(v.read(ROOT, "a"), Err(Error::Stopped)));
    assert!(matches!(
        v.write_open(ROOT, "a", &writer("writer"), false),
        Err(Error::Stopped)
    ));
    assert!(matches!(v.progress(), Err(Error::Stopped)));
    drop(v);
    let v = reopen(tmp.path());
    assert_eq!(v.read(ROOT, "a").unwrap(), b"before");
    assert_eq!(v.read(ROOT, "big").unwrap(), FILLED);
}

#[cfg(feature = "failpoints")]
#[test]
fn a_failure_before_the_rename_is_retried_at_the_next_step() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let guard = agentfs_core::failpoints::configure("tree.compact_before_rename=error").unwrap();
    fill_journal(&mut v, "big");
    drop(guard);
    assert!(!v.is_stopped());
    let path = tmp.path().join("calls.wal");
    assert!(fs::metadata(&path).unwrap().len() > 64 << 20);
    v.settle().unwrap();
    put(&mut v, ROOT, "a", b"next");
    assert!(fs::metadata(&path).unwrap().len() < 1 << 20);
    assert_eq!(v.read(ROOT, "a").unwrap(), b"next");
}

#[cfg(feature = "failpoints")]
#[test]
fn a_cut_first_init_starts_over() {
    let tmp = tempfile::tempdir().unwrap();
    let guard = agentfs_core::failpoints::configure("tree.init_before_format=error").unwrap();
    assert!(TreeActor::create(tmp.path(), &trees::identity(), &PodContext::default()).is_err());
    drop(guard);
    assert!(tmp.path().join("initializing").exists());
    assert!(tmp.path().join("history.git").exists());
    assert!(!tmp.path().join("format").exists());
    let mut v = new_actor(tmp.path());
    assert!(!tmp.path().join("initializing").exists());
    put(&mut v, ROOT, "a", b"fresh");
    assert_eq!(v.read(ROOT, "a").unwrap(), b"fresh");
}

#[cfg(feature = "failpoints")]
#[test]
fn a_failed_save_keeps_serving_and_saves_at_the_next_step() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"base");
    let h = v.write_open(ROOT, "a", &writer("writer"), false).unwrap();
    v.write(h, 0, b"+returned", true).unwrap();
    let head = v.head(ROOT).unwrap();
    let point = "tree.before_step";
    let guard = agentfs_core::failpoints::configure(&format!("{point}=error")).unwrap();
    assert!(v.close(h).is_err(), "{point}");
    drop(guard);
    assert!(!v.is_stopped(), "{point}");
    assert_eq!(v.read(ROOT, "a").unwrap(), b"base", "{point}");
    assert_eq!(v.head(ROOT).unwrap(), head, "{point}");
    // Other work goes on, the failed close's handle still counting as open.
    let other = v.write_open(ROOT, "other", &writer("writer"), true);
    assert!(other.is_ok());
    assert_eq!(v.read(ROOT, "a").unwrap(), b"base+returned");
    v.settle().unwrap();
    assert_eq!(opens_saved(tmp.path(), h), 1);
    assert!(matches!(v.close(h), Err(Error::NotFound)));

    // A failed save a restart finds saves there, once.
    let h = v.write_open(ROOT, "a", &writer("writer"), false).unwrap();
    v.write(h, 0, b"+again", true).unwrap();
    let guard = agentfs_core::failpoints::configure("tree.before_step=error").unwrap();
    assert!(v.close(h).is_err());
    drop(guard);
    drop(v);
    let v = reopen(tmp.path());
    assert_eq!(v.read(ROOT, "a").unwrap(), b"base+returned+again");
    assert_eq!(opens_saved(tmp.path(), h), 1);
}

/// Finding 51 (R6.1): a fast-forward takes the upstream's line maps, so the fork's lost
/// version is in the history only: the fork's next save of the file lists none of it.
#[test]
fn a_fast_forward_takes_the_upstreams_line_maps() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "c", b"x\n");
    let fork = v.fork().unwrap();
    put(&mut v, fork, "c", b"s\n");
    let lost = v.head(fork).unwrap();
    put(&mut v, ROOT, "c", b"m\n");
    assert_eq!(v.take_next(ROOT).unwrap(), Took::Taken);
    assert_eq!(v.take_next(fork).unwrap(), Took::Taken);
    assert_eq!(v.head(fork).unwrap(), v.head(ROOT).unwrap());
    put(&mut v, fork, "c", b"t\n");
    let map = |commit: String| -> serde_json::Value {
        let out = git(tmp.path(), &["show", &format!("{commit}:metadata.json")]);
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["c"][2].clone()
    };
    let lost_lines: Vec<String> = map(lost.to_string())["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["version"].as_str().unwrap().to_owned())
        .collect();
    let now = map(format!("refs/heads/{fork}"));
    for version in &lost_lines {
        assert!(
            now["ancestors"].get(version).is_none(),
            "the lost version {version} is still in {now}"
        );
    }
}

/// Finding 51 (R6.1): a fast-forward takes the upstream's name histories, so a fork's
/// rename that lost is in the history only, and the fork's next commit does not carry it.
#[test]
fn a_fast_forward_takes_the_upstreams_names() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"x\n");
    let fork = v.fork().unwrap();
    v.rename(fork, "a", "b", &writer("w")).unwrap();
    v.rename(ROOT, "a", "c", &writer("w")).unwrap();
    assert_eq!(v.take_next(ROOT).unwrap(), Took::Taken);
    assert_eq!(v.take_next(fork).unwrap(), Took::Taken);
    assert_eq!(v.head(fork).unwrap(), v.head(ROOT).unwrap());
    // The fork's next commit writes out the names it holds now.
    put(&mut v, fork, "d", b"y\n");
    let names = |reference: &str| -> serde_json::Value {
        let out = git(
            tmp.path(),
            &["show", &format!("{reference}:namespace.json")],
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let (forked, upstream) = (
        names(&format!("refs/heads/{fork}")),
        names("refs/heads/root"),
    );
    for (id, history) in upstream.as_object().unwrap() {
        assert_eq!(forked[id]["versions"], history["versions"], "file {id}");
    }
}

/// R9.10: an `open(O_TRUNC)` that fails answers an error, so no RELEASE ever comes. It
/// leaves no open behind to hold its file's takes.
#[cfg(feature = "failpoints")]
#[test]
fn a_failed_truncate_at_open_leaves_no_open_behind() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"old");
    let fork = v.fork().unwrap();
    put(&mut v, fork, "a", b"sandbox");
    // The open's record is the first append, the truncate's the second.
    let guard = agentfs_core::failpoints::configure("tree.wal_before_header=error@2").unwrap();
    assert!(
        v.write_open_truncating(ROOT, "a", &writer("w"), false)
            .is_err()
    );
    drop(guard);
    assert_eq!(v.take_next(ROOT).unwrap(), Took::Taken);
}

#[cfg(feature = "failpoints")]
#[test]
fn a_failed_take_stays_queued_for_the_next_event() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let fork = v.fork().unwrap();
    put(&mut v, fork, "a", b"sandbox");
    let guard = agentfs_core::failpoints::configure("tree.before_step=error").unwrap();
    assert!(!v.progress().unwrap());
    drop(guard);
    assert!(!v.is_stopped());
    assert!(matches!(v.read(ROOT, "a"), Err(Error::NotFound)));
    assert!(v.progress().unwrap());
    while v.progress().unwrap() {}
    assert_eq!(v.read(ROOT, "a").unwrap(), b"sandbox");
}

#[cfg(feature = "failpoints")]
#[test]
fn a_failed_write_changes_nothing_and_keeps_serving() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let h = v.write_open(ROOT, "a", &writer("writer"), true).unwrap();
    v.write(h, 0, b"one", false).unwrap();
    let path = tmp.path().join("calls.wal");
    let journal = fs::read(&path).unwrap();
    for point in ["tree.wal_before_header", "tree.wal_after_payload"] {
        let guard = agentfs_core::failpoints::configure(&format!("{point}=error")).unwrap();
        assert!(v.write(h, 3, b"lost", false).is_err(), "{point}");
        drop(guard);
        assert!(!v.is_stopped(), "{point}");
        assert_eq!(v.read_open(h).unwrap(), b"one", "{point}");
        assert_eq!(fs::read(&path).unwrap(), journal, "{point}");
    }
    v.write(h, 3, b"two", false).unwrap();
    v.close(h).unwrap();
    assert_eq!(v.read(ROOT, "a").unwrap(), b"onetwo");
}

#[cfg(feature = "failpoints")]
#[test]
fn a_failed_fsync_stops_the_actor() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let h = v.write_open(ROOT, "a", &writer("writer"), true).unwrap();
    v.write(h, 0, b"one", false).unwrap();
    v.write(h, 3, b"unknown", false).unwrap();
    let guard = agentfs_core::failpoints::configure("tree.wal_before_fsync=error").unwrap();
    assert!(v.durability().make_all_durable().is_err());
    drop(guard);
    assert!(v.is_stopped());
    assert!(matches!(
        v.write(h, 10, b"more", false),
        Err(Error::Stopped)
    ));
    assert!(matches!(v.read(ROOT, "a"), Err(Error::Stopped)));
    drop(v);
    let v = reopen(tmp.path());
    assert!(v.read(ROOT, "a").unwrap().starts_with(b"one"));
}

#[cfg(feature = "failpoints")]
#[test]
fn branch_and_queue_recover_together_after_the_commit_point() {
    let tmp = tempfile::tempdir().unwrap();
    let mut volume = new_actor(tmp.path());
    let fork = volume.fork().unwrap();
    let h = volume
        .write_open(fork, "a", &writer("writer"), true)
        .unwrap();
    volume.write(h, 0, b"saved", false).unwrap();
    volume.settle().unwrap();
    let repo = repo(tmp.path());
    let before = repo.list_refs("refs/queue/root/").unwrap().len();
    let guard = agentfs_core::failpoints::configure("tree.build_after_built_log=error").unwrap();
    assert!(matches!(volume.close(h).unwrap(), Saved::Commit(_)));
    volume.settle().unwrap();
    drop(guard);
    let committed = volume.head(fork).unwrap();
    drop(volume);
    let mut volume = reopen(tmp.path());
    assert_eq!(
        repo.head(&format!("refs/heads/{fork}")).unwrap(),
        Some(committed)
    );
    let queued = repo.list_refs("refs/queue/root/").unwrap();
    assert_eq!(queued.len(), before + 1);
    assert_eq!(queued.last().unwrap().1, committed);
    while volume.progress().unwrap() {}
    assert_eq!(volume.read(fork, "a").unwrap(), b"saved");
    assert!(repo.list_refs("refs/queue/root/").unwrap().is_empty());
    assert!(
        repo.is_ancestor(committed, volume.head(ROOT).unwrap())
            .unwrap()
    );
}

#[cfg(feature = "failpoints")]
#[test]
fn recovery_enqueues_once_after_failure_before_the_commit_point() {
    let tmp = tempfile::tempdir().unwrap();
    let mut volume = new_actor(tmp.path());
    let fork = volume.fork().unwrap();
    let h = volume
        .write_open(fork, "a", &writer("writer"), true)
        .unwrap();
    volume.write(h, 0, b"saved", false).unwrap();
    let before_head = volume.head(fork).unwrap();
    let repo = repo(tmp.path());
    let before = repo.list_refs("refs/queue/root/").unwrap();
    let guard = agentfs_core::failpoints::configure("tree.before_step=error").unwrap();
    assert!(volume.close(h).is_err());
    volume.settle().unwrap();
    drop(guard);
    assert_eq!(
        repo.head(&format!("refs/heads/{fork}")).unwrap(),
        Some(before_head)
    );
    assert_eq!(repo.list_refs("refs/queue/root/").unwrap(), before);
    drop(volume);
    let mut volume = reopen(tmp.path());
    assert_eq!(volume.read(fork, "a").unwrap(), b"saved");
    let queued = repo.list_refs("refs/queue/root/").unwrap();
    let saved = queued.last().unwrap().1;
    assert_ne!(saved, before_head);
    assert!(!before.iter().any(|(_, oid)| *oid == saved));
    while volume.progress().unwrap() {}
    // Taken once: as a fast-forward, the save itself on the root's first parents (R6.2),
    // or else a landing whose second parent it is.
    let mut cursor = volume.head(ROOT).unwrap();
    let mut takes = 0;
    loop {
        let commit = repo.read_commit(cursor).unwrap();
        if cursor == saved || commit.parents.get(1) == Some(&saved) {
            takes += 1;
        }
        let Some(parent) = commit.parents.first() else {
            break;
        };
        cursor = *parent;
    }
    assert_eq!(takes, 1);
}

/// Every ref, checkout, config entry and `Agentfs-Tree` trailer names a tree by
/// its identity, `root` or `fork/<n>`: a save, a fork, a landing, a sync and a
/// reopen leave no other name (docs/trees.md).
#[test]
fn history_names_trees_by_identity_only() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"base\n");
    let fork = v.fork().unwrap();
    assert!(!fork.is_root());
    put(&mut v, fork, "b", b"sandbox\n");
    while v.progress().unwrap() {}
    put(&mut v, ROOT, "a", b"next\n");
    while v.progress().unwrap() {}
    assert_eq!(v.read(fork, "a").unwrap(), b"next\n");
    assert_eq!(v.read(ROOT, "b").unwrap(), b"sandbox\n");
    drop(v);
    let v = reopen(tmp.path());
    assert_eq!(v.trees(), [ROOT, fork]);

    let identity = |name: &str| name.parse::<TreeId>().is_ok();
    for (name, _) in all_refs(tmp.path()) {
        let tree = ["refs/heads/", "refs/agentfs/objects/"]
            .iter()
            .find_map(|prefix| name.strip_prefix(prefix));
        assert!(tree.is_some_and(identity), "{name}");
    }
    let head = fs::read_to_string(tmp.path().join("history.git/HEAD")).unwrap();
    assert_eq!(head, "ref: refs/heads/root\n");
    let config = String::from_utf8(
        git(
            tmp.path(),
            &["config", "--get", &format!("branch.{fork}.merge")],
        )
        .stdout,
    )
    .unwrap();
    assert_eq!(config, "refs/heads/root\n");
    let mut checkouts: Vec<String> = fs::read_dir(tmp.path().join("trees"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    checkouts.sort();
    assert_eq!(checkouts, ["fork", "root"]);
    assert!(
        tmp.path()
            .join("trees")
            .join(fork.to_string())
            .join("a")
            .is_file()
    );
    let trailers = String::from_utf8(
        git(
            tmp.path(),
            &[
                "log",
                "--all",
                "--format=%(trailers:key=Agentfs-Tree,valueonly)",
            ],
        )
        .stdout,
    )
    .unwrap();
    let named: std::collections::BTreeSet<&str> =
        trailers.lines().filter(|l| !l.is_empty()).collect();
    let fork_text = fork.to_string();
    assert_eq!(named, [fork_text.as_str(), "root"].into_iter().collect());
}

/// A take goes to the root first, then to the forks by number: `fork/10` after
/// `fork/9`, as no text order would have it.
#[test]
fn takes_visit_the_root_then_forks_by_number() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let forks: Vec<TreeId> = (0..11).map(|_| v.fork().unwrap()).collect();
    assert_eq!(forks, (1..=11).map(TreeId::Fork).collect::<Vec<_>>());
    put(&mut v, forks[0], "f", b"sandbox\n");
    put(&mut v, ROOT, "r", b"root\n");
    v.settle().unwrap();
    let queued = |tree: TreeId| {
        repo(tmp.path())
            .list_refs(&format!("refs/queue/{tree}/"))
            .unwrap()
            .len()
    };
    assert!(queued(ROOT) > 0, "fork/1's landings");
    let mut taken = Vec::new();
    loop {
        let before: Vec<usize> = v.trees().into_iter().map(queued).collect();
        if !v.progress().unwrap() {
            break;
        }
        v.settle().unwrap();
        let after: Vec<usize> = v.trees().into_iter().map(queued).collect();
        let tree = (0..before.len())
            .find(|i| after[*i] < before[*i])
            .map(|i| v.trees()[i])
            .unwrap();
        if taken.last() != Some(&tree) {
            taken.push(tree);
        }
    }
    let mut expected = vec![ROOT];
    expected.extend(forks);
    assert_eq!(taken, expected);
}

/// A pod context with every field.
fn pod(name: &str) -> PodContext {
    let label = |text: String| Some(agentfs_contracts::name::Label::parse(&text).unwrap());
    PodContext {
        namespace: label(format!("ns-{name}")),
        pod: label(format!("pod-{name}")),
        pod_uid: label(format!("uid-{name}")),
        node: label(format!("node-{name}")),
    }
}

fn last_message(root: &std::path::Path, tree: TreeId) -> String {
    String::from_utf8(git(root, &["log", "-1", "--format=%B", &tree.to_string()]).stdout).unwrap()
}

/// Rule 2: a recovery close commits as the writer that opened, where it ran;
/// the restarted service's own pod context is never put in its place.
#[test]
fn a_recovered_open_commits_with_its_writers_pod_context() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let by = agentfs_contracts::pod::WriterAt {
        writer: WriterLabel::parse("w").unwrap(),
        at: pod("a"),
    };
    let h = v.write_open(ROOT, "a", &by, true).unwrap();
    v.write(h, 0, b"abandoned\n", false).unwrap();
    drop(v);
    let v = TreeActor::open(tmp.path(), &pod("b")).unwrap();
    assert_eq!(v.read(ROOT, "a").unwrap(), b"abandoned\n");
    let last = last_message(tmp.path(), ROOT);
    assert!(last.starts_with("Direct by w\n"), "{last}");
    for trailer in [
        "Agentfs-Namespace: ns-a",
        "Agentfs-Pod: pod-a",
        "Agentfs-Pod-Uid: uid-a",
        "Agentfs-Node: node-a",
        "Agentfs-Volume: test",
        "Agentfs-Volume-Id: 00000000-0000-0000-0000-000000000001",
    ] {
        assert!(last.contains(&format!("{trailer}\n")), "{trailer}: {last}");
    }
    assert!(!last.contains("-b\n"), "{last}");
}

/// git-model §3: a landing merge names the writer whose save it lands, with that
/// save's pod context, never `agentfs`.
#[test]
fn a_landing_names_its_landed_writer() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = TreeActor::create(tmp.path(), &trees::identity(), &pod("svc")).unwrap();
    put(&mut v, ROOT, "a", b"base\n");
    let fork = v.fork().unwrap();
    put(&mut v, ROOT, "b", b"root moved\n");
    let by = agentfs_contracts::pod::WriterAt {
        writer: WriterLabel::parse("w").unwrap(),
        at: pod("p"),
    };
    let h = v.write_open(fork, "c", &by, true).unwrap();
    v.write(h, 0, b"forked\n", false).unwrap();
    v.close(h).unwrap();
    assert_eq!(v.take_next(ROOT).unwrap(), Took::Taken);
    v.settle().unwrap();
    let parents = git(tmp.path(), &["log", "-1", "--format=%P", &ROOT.to_string()]).stdout;
    assert_eq!(
        String::from_utf8(parents)
            .unwrap()
            .split_whitespace()
            .count(),
        2,
        "a merge"
    );
    let last = last_message(tmp.path(), ROOT);
    assert!(last.starts_with("Landing by w\n\n"), "{last}");
    for trailer in [
        "Agentfs-Writer: w",
        "Agentfs-Cause: Landing",
        "Agentfs-Namespace: ns-p",
        "Agentfs-Pod: pod-p",
        "Agentfs-Pod-Uid: uid-p",
        "Agentfs-Node: node-p",
    ] {
        assert!(last.contains(&format!("{trailer}\n")), "{trailer}: {last}");
    }
    assert!(!last.contains("svc"), "{last}");
    // A sync, by contrast, is agentfs's own, where the service runs.
    while v.progress().unwrap() {}
    let sync = last_message(tmp.path(), fork);
    if sync.starts_with("Sync") {
        assert!(sync.contains("Agentfs-Writer: agentfs\n"), "{sync}");
        assert!(sync.contains("Agentfs-Pod: pod-svc\n"), "{sync}");
    }
}
