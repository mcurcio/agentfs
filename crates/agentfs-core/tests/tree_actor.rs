//! The recording slice on real Git repositories and Linux byte operations.
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;

use agentfs_core::tree_actor::{Error, Saved, Volume};
use agentfs_core::tree_git::TreeGit;

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
        let length = 24 + field(8) + field(12);
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

/// How many commits on main name this open.
fn opens_saved(root: &std::path::Path, open: u64) -> usize {
    let out = git(
        root,
        &[
            "log",
            "main",
            "--format=%(trailers:key=Agentfs-Open,valueonly)",
        ],
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|line| *line == open.to_string())
        .count()
}

fn put(v: &mut Volume, tree: &str, path: &str, bytes: &[u8]) -> u64 {
    let h = v.write_open(tree, path, "test-writer", true).unwrap();
    v.truncate(h, 0).unwrap();
    v.write(h, 0, bytes, false).unwrap();
    v.close(h).unwrap();
    h
}

#[test]
fn byte_calls_match_a_plain_linux_file_and_publish_only_at_release() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(&tmp.path().join("state")).unwrap();
    put(&mut v, "main", "a", b"foo");
    let h = v.write_open("main", "a", "writer", false).unwrap();
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
        v.sync().unwrap();
        assert_eq!(v.read("main", "a").unwrap(), b"foo");
    }
    v.close(h).unwrap();
    assert_eq!(
        v.read("main", "a").unwrap(),
        fs::read(tmp.path().join("plain")).unwrap()
    );
    let repo = repo(&tmp.path().join("state"));
    let commit = repo.read_commit(v.head("main").unwrap()).unwrap();
    assert!(
        String::from_utf8(commit.message)
            .unwrap()
            .contains(&format!("Agentfs-Open: {h}\n"))
    );
    assert_eq!(
        fs::read(tmp.path().join("state/trees/main/a")).unwrap(),
        v.read("main", "a").unwrap()
    );
}

#[test]
fn append_finishes_a_partial_line_and_close_replays_on_the_current_head() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    put(&mut v, "main", "a", b"foo");
    let old = v.write_open("main", "a", "old", false).unwrap();
    v.write(old, 0, b"bar", true).unwrap();
    put(&mut v, "main", "a", b"new");
    let before = v.head("main").unwrap();
    v.close(old).unwrap();
    assert_eq!(v.read("main", "a").unwrap(), b"newbar");
    let repo = repo(tmp.path());
    assert_eq!(
        repo.read_commit(v.head("main").unwrap()).unwrap().parents[0],
        before
    );
}

#[test]
fn rename_over_and_unlink_keep_old_handles_orphaned_even_after_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    put(&mut v, "main", "a", b"old");
    let old = v.write_open("main", "a", "old-writer", false).unwrap();
    put(&mut v, "main", "temp", b"replacement");
    v.rename("main", "temp", "a", "editor").unwrap();
    v.write(old, 0, b"BAD", false).unwrap();
    let before = v.head("main").unwrap();
    assert_eq!(v.close(old).unwrap(), Saved::OrphanDropped);
    assert_eq!(v.head("main").unwrap(), before);
    assert_eq!(v.read("main", "a").unwrap(), b"replacement");
    let old = v.write_open("main", "a", "old-writer", false).unwrap();
    v.unlink("main", "a", "editor").unwrap();
    put(&mut v, "main", "a", b"new-object");
    v.write(old, 0, b"BAD", false).unwrap();
    drop(v); // Same recovery entry point as a killed actor; WAL must rebuild objects.
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.read("main", "a").unwrap(), b"new-object");
    drop(v);
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.read("main", "a").unwrap(), b"new-object");
}

#[test]
fn rename_of_an_open_file_follows_the_object() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    put(&mut v, "main", "a", b"foo");
    let h = v.write_open("main", "a", "writer", false).unwrap();
    v.write(h, 0, b"bar", true).unwrap();
    v.rename("main", "a", "b", "writer").unwrap();
    v.close(h).unwrap();
    assert!(matches!(v.read("main", "a"), Err(Error::NotFound)));
    assert_eq!(v.read("main", "b").unwrap(), b"foobar");
}

#[test]
fn restart_closes_abandoned_opens_once_and_repairs_the_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    let h = v.write_open("main", "a", "writer", true).unwrap();
    v.write(h, 0, b"acknowledged", false).unwrap();
    let before = v.head("main").unwrap();
    drop(v);
    let mut v = Volume::open(tmp.path()).unwrap();
    let saved = v.head("main").unwrap();
    assert_ne!(saved, before);
    assert_eq!(v.read("main", "a").unwrap(), b"acknowledged");
    let next = v.write_open("main", "a", "writer", false).unwrap();
    assert!(next > h);
    v.close(next).unwrap();
    drop(v);
    fs::write(tmp.path().join("trees/main/a"), b"damaged-cache").unwrap();
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.head("main").unwrap(), saved);
    assert_eq!(
        fs::read(tmp.path().join("trees/main/a")).unwrap(),
        b"acknowledged"
    );
}

#[test]
fn fork_saves_land_and_parent_changes_sync_across_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    put(&mut v, "main", "a", b"base");
    let fork = v.fork().unwrap();
    put(&mut v, &fork, "a", b"sandbox");
    while v.progress().unwrap() {}
    assert_eq!(v.read("main", "a").unwrap(), b"sandbox");
    put(&mut v, "main", "a", b"parent");
    while v.progress().unwrap() {}
    assert_eq!(v.read(&fork, "a").unwrap(), b"parent");
    assert_eq!(v.read("main", "a").unwrap(), b"parent");
    let fork_head = v.head(&fork).unwrap();
    drop(v);
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.head(&fork).unwrap(), fork_head);
    assert_eq!(v.read(&fork, "a").unwrap(), b"parent");
    assert_eq!(
        fs::read(tmp.path().join("trees").join(&fork).join("a")).unwrap(),
        b"parent"
    );
}

#[test]
fn sync_delivers_prefix_before_open_file_and_holds_later_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    put(&mut v, "main", "a", b"a0\n");
    put(&mut v, "main", "b", b"b0\n");
    put(&mut v, "main", "c", b"c0\n");
    let fork = v.fork().unwrap();
    let held = v.write_open(&fork, "a", "holder", false).unwrap();
    put(&mut v, "main", "b", b"b1\n");
    while v.progress().unwrap() {}
    assert_eq!(v.read(&fork, "b").unwrap(), b"b1\n");
    put(&mut v, "main", "a", b"a1\n");
    put(&mut v, "main", "c", b"c1\n");
    while v.progress().unwrap() {}
    assert_eq!(v.read(&fork, "a").unwrap(), b"a0\n");
    assert_eq!(v.read(&fork, "c").unwrap(), b"c0\n");
    assert!(v.metrics().waits > 0);
    v.close(held).unwrap();
    while v.progress().unwrap() {}
    assert_eq!(v.read(&fork, "a").unwrap(), b"a1\n");
    assert_eq!(v.read(&fork, "c").unwrap(), b"c1\n");
}

#[test]
fn queued_same_source_keeps_order_and_collision_remains_in_history() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    put(&mut v, "main", "a", b"base\n");
    put(&mut v, "main", "b", b"base-b\n");
    let fork = v.fork().unwrap();
    let held = v.write_open("main", "a", "parent", false).unwrap();
    put(&mut v, &fork, "a", b"child\n");
    let losing = v.head(&fork).unwrap();
    put(&mut v, &fork, "b", b"child-b\n");
    while v.progress().unwrap() {}
    assert_eq!(v.read("main", "a").unwrap(), b"base\n");
    assert_eq!(v.read("main", "b").unwrap(), b"base-b\n");
    assert!(v.metrics().waits > 0);
    v.truncate(held, 0).unwrap();
    v.write(held, 0, b"parent\n", false).unwrap();
    v.close(held).unwrap();
    while v.progress().unwrap() {}
    assert_eq!(v.read("main", "a").unwrap(), b"parent\n");
    assert_eq!(v.read("main", "b").unwrap(), b"child-b\n");
    assert_eq!(v.read(&fork, "a").unwrap(), b"parent\n");
    let repo = repo(tmp.path());
    assert!(repo.is_ancestor(losing, v.head("main").unwrap()).unwrap());
    let collisions = repo.list_refs("refs/collisions/main/").unwrap();
    assert!(collisions.iter().any(|(_, oid)| *oid == losing));
}

#[test]
fn create_rename_and_unlink_versions_sync_and_land() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    let fork = v.fork().unwrap();
    put(&mut v, "main", "a", b"content\n");
    while v.progress().unwrap() {}
    assert_eq!(v.read(&fork, "a").unwrap(), b"content\n");
    v.rename("main", "a", "b", "parent").unwrap();
    while v.progress().unwrap() {}
    assert!(matches!(v.read(&fork, "a"), Err(Error::NotFound)));
    assert_eq!(v.read(&fork, "b").unwrap(), b"content\n");
    v.unlink("main", "b", "parent").unwrap();
    while v.progress().unwrap() {}
    assert!(matches!(v.read(&fork, "b"), Err(Error::NotFound)));
    put(&mut v, &fork, "child", b"child\n");
    while v.progress().unwrap() {}
    v.rename(&fork, "child", "moved", "child").unwrap();
    while v.progress().unwrap() {}
    assert_eq!(v.read("main", "moved").unwrap(), b"child\n");
    v.unlink(&fork, "moved", "child").unwrap();
    while v.progress().unwrap() {}
    assert!(matches!(v.read("main", "moved"), Err(Error::NotFound)));
}

#[test]
fn rename_after_rejected_create_keeps_rejected_content_out_of_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    let fork = v.fork().unwrap();
    let parent = v.write_open("main", "a", "parent", true).unwrap();
    put(&mut v, &fork, "a", b"child\n");
    let held = v.write_open(&fork, "a", "child", false).unwrap();
    while v.progress().unwrap() {}
    v.write(parent, 0, b"parent\n", false).unwrap();
    v.close(parent).unwrap();
    while v.progress().unwrap() {}
    assert_eq!(v.read("main", "a").unwrap(), b"parent\n");
    assert_eq!(v.read(&fork, "a").unwrap(), b"child\n");
    v.rename(&fork, "a", "b", "child").unwrap();
    v.close(held).unwrap();
    while v.progress().unwrap() {}
    assert_eq!(v.read("main", "a").unwrap(), b"parent\n");
    assert_eq!(v.read("main", "b").unwrap(), b"");
}

#[test]
fn single_actor_and_invalid_names_fail_before_recording() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    let journal = fs::metadata(tmp.path().join("calls.wal")).unwrap().len();
    assert!(matches!(Volume::open(tmp.path()), Err(Error::Busy)));
    for path in ["../a", "a/b", ".git", ".GiT", "git~1", "", "/tmp"] {
        assert!(matches!(
            v.write_open("main", path, "writer", true),
            Err(Error::Name)
        ));
    }
    assert!(matches!(
        v.write_open("main", "a", "w\nAgentfs-Writer: fake", true),
        Err(Error::Writer)
    ));
    assert_eq!(
        fs::metadata(tmp.path().join("calls.wal")).unwrap().len(),
        journal
    );
}

#[test]
fn incomplete_tail_is_discarded_but_a_bad_frame_with_a_later_one_is_not() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    put(&mut v, "main", "a", b"saved");
    drop(v);
    let path = tmp.path().join("calls.wal");
    let valid = fs::read(&path).unwrap();
    let mut corrupt_header = valid.clone();
    corrupt_header[frames(&valid)[1].0 + 8] ^= 0x80;
    fs::write(&path, corrupt_header).unwrap();
    assert!(matches!(
        Volume::open(tmp.path()),
        Err(Error::CorruptJournal(_))
    ));
    fs::write(&path, &valid).unwrap();
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"AFJ")
        .unwrap();
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.read("main", "a").unwrap(), b"saved");
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
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.read("main", "a").unwrap(), b"returned-write");
    let head = v.head("main").unwrap();
    drop(v);
    assert_eq!(
        Volume::open(tmp.path()).unwrap().head("main").unwrap(),
        head
    );
}

#[test]
fn crash_worker() {
    let Some(root) = std::env::var_os("AGENTFS_TREE_ACTOR_CRASH_TEST") else {
        return;
    };
    let mut v = Volume::open(std::path::Path::new(&root)).unwrap();
    let h = v.write_open("main", "a", "killed-client", true).unwrap();
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
    let mut volume = Volume::open(tmp.path()).unwrap();
    while volume.progress().unwrap() {}
    assert_eq!(
        volume.read("main", "incoming").unwrap(),
        b"from child; after landing"
    );
    drop(volume);
    let volume = Volume::open(tmp.path()).unwrap();
    assert_eq!(
        volume.read("main", "incoming").unwrap(),
        b"from child; after landing"
    );
}

#[test]
fn incoming_crash_worker() {
    let Some(root) = std::env::var_os("AGENTFS_INCOMING_CRASH_TEST") else {
        return;
    };
    let mut volume = Volume::open(std::path::Path::new(&root)).unwrap();
    let fork = volume.fork().unwrap();
    put(&mut volume, &fork, "incoming", b"from child");
    while volume.progress().unwrap() {}
    assert_eq!(volume.read("main", "incoming").unwrap(), b"from child");
    let handle = volume
        .write_open("main", "incoming", "writer", false)
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
    let mut v = Volume::open(tmp.path()).unwrap();
    let a = v.write_open("main", "a", "one", true).unwrap();
    let b = v.write_open("main", "a", "two", false).unwrap();
    let size = 100 * 1024 * 1024;
    v.truncate(a, size).unwrap();
    v.write(b, 0, b"x", true).unwrap();
    v.close(a).unwrap();
    v.close(b).unwrap();
    let bytes = v.read("main", "a").unwrap();
    assert_eq!(bytes.len(), size as usize + 1);
    assert_eq!(bytes.last(), Some(&b'x'));
}

#[test]
fn save_and_checkout_failures_keep_calls_and_recover_without_duplicate_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    put(&mut v, "main", "a", b"base");
    let h = v.write_open("main", "a", "writer", false).unwrap();
    v.write(h, 0, b"journaled", true).unwrap();
    // A stale ref lock stands in for an interrupted Git ref update.
    fs::write(
        tmp.path().join("history.git/refs/heads/main.lock"),
        b"stale",
    )
    .unwrap();
    assert!(matches!(v.close(h).unwrap(), Saved::Commit(_)));
    assert!(!v.is_stopped());
    assert_eq!(v.read("main", "a").unwrap(), b"basejournaled");
    let recorded = v.head("main").unwrap();
    assert_ne!(
        repo(tmp.path()).head("refs/heads/main").unwrap(),
        Some(recorded)
    );
    drop(v);
    let mut v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.head("main").unwrap(), recorded);
    assert_eq!(v.read("main", "a").unwrap(), b"basejournaled");
    let h = v.write_open("main", "a", "writer", false).unwrap();
    v.write(h, 0, b"once", true).unwrap();
    fs::remove_dir_all(tmp.path().join("trees/main")).unwrap();
    fs::write(tmp.path().join("trees/main"), b"checkout-blocked").unwrap();
    assert!(matches!(v.close(h).unwrap(), Saved::Commit(_))); // Checkout failed.
    let repo = repo(tmp.path());
    let committed = repo.head("refs/heads/main").unwrap().unwrap();
    assert_eq!(committed, v.head("main").unwrap());
    drop(v);
    fs::remove_file(tmp.path().join("trees/main")).unwrap();
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.head("main").unwrap(), committed);
    assert_eq!(v.read("main", "a").unwrap(), b"basejournaledonce");
    assert_eq!(
        fs::read(tmp.path().join("trees/main/a")).unwrap(),
        b"basejournaledonce"
    );
    assert_eq!(opens_saved(tmp.path(), h), 1);
}

#[test]
fn anonymous_object_first_link_and_recovery_preserve_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let object;
    {
        let mut v = Volume::open(tmp.path()).unwrap();
        let h = v.anonymous_open("main", "writer").unwrap();
        object = v.open_object(h).unwrap();
        v.write(h, 0, b"saved on recovery", false).unwrap();
        assert!(v.files("main").unwrap().is_empty());
        v.first_link("main", object, "named", "writer").unwrap();
        assert_eq!(v.read("main", "named").unwrap(), b"");
        assert_eq!(v.files("main").unwrap()[0].1, object);
        // Drop with the handle still open; restart replays the journal and closes it.
    }
    let mut v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.read("main", "named").unwrap(), b"saved on recovery");
    assert_eq!(v.files("main").unwrap()[0].1, object);
    assert!(matches!(
        v.first_link("main", object, "other", "writer"),
        Err(Error::NotFound)
    ));
    let h = v.anonymous_open("main", "writer").unwrap();
    v.write(h, 0, b"unlinked", false).unwrap();
    assert_eq!(v.close(h).unwrap(), Saved::Unchanged);
    assert_eq!(v.files("main").unwrap().len(), 1);
}

#[test]
fn saved_line_map_and_bytes_share_a_commit_without_a_reserved_name() {
    let tmp = tempfile::tempdir().unwrap();
    let mut volume = Volume::open(tmp.path()).unwrap();
    put(&mut volume, "main", "metadata.json", b"first\npartial");
    let handle = volume
        .write_open("main", "metadata.json", "writer", false)
        .unwrap();
    volume.write(handle, 0, b" tail\nnext\n", true).unwrap();
    volume.close(handle).unwrap();
    let repo = repo(tmp.path());
    let root = repo.read_commit(volume.head("main").unwrap()).unwrap().tree;
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
        fs::read(tmp.path().join("trees/main/metadata.json")).unwrap(),
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
    let mut volume = Volume::open(tmp.path()).unwrap();
    put(&mut volume, "main", "target", b"first\nsecond\n");
    let old = volume.write_open("main", "target", "old", false).unwrap();
    let before = metadata(tmp.path(), volume.head("main").unwrap());
    put(&mut volume, "main", "temp", b"first\nchanged\n");
    volume.rename("main", "temp", "target", "editor").unwrap();
    let after = metadata(tmp.path(), volume.head("main").unwrap());
    assert_ne!(before.0, after.0);
    assert_eq!(before.1, after.1);
    assert_eq!(before.2.lines[0].identity, after.2.lines[0].identity);
    assert_eq!(before.2.lines[1].identity, after.2.lines[1].identity);
    assert_ne!(before.2.lines[1].version, after.2.lines[1].version);
    volume.write(old, 0, b"stale", false).unwrap();
    assert_eq!(volume.close(old).unwrap(), Saved::OrphanDropped);
    drop(volume);
    let volume = Volume::open(tmp.path()).unwrap();
    assert_eq!(volume.read("main", "target").unwrap(), b"first\nchanged\n");
    assert_eq!(metadata(tmp.path(), volume.head("main").unwrap()), after);
}

#[test]
fn sync_fast_forwards_after_atomic_replacement_with_different_objects() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    put(&mut v, "main", "target", b"old\n");
    let fork = v.fork().unwrap();
    put(&mut v, &fork, "temp", b"new\n");
    v.rename(&fork, "temp", "target", "editor").unwrap();
    let fork_object = v
        .files(&fork)
        .unwrap()
        .into_iter()
        .find(|(p, _, _)| p == "target")
        .unwrap()
        .1;
    let parent_object = v
        .files("main")
        .unwrap()
        .into_iter()
        .find(|(p, _, _)| p == "target")
        .unwrap()
        .1;
    assert_ne!(fork_object, parent_object);
    let held = v.write_open(&fork, "target", "writer", false).unwrap();
    while v.progress().unwrap() {}
    assert_eq!(v.read("main", "target").unwrap(), b"new\n");
    assert_eq!(v.head(&fork).unwrap(), v.head("main").unwrap());
    assert_eq!(
        v.files(&fork)
            .unwrap()
            .into_iter()
            .find(|(p, _, _)| p == "target")
            .unwrap()
            .1,
        fork_object
    );
    v.write(held, 0, b"later\n", true).unwrap();
    drop(v);
    let mut v = Volume::open(tmp.path()).unwrap();
    while v.progress().unwrap() {}
    assert_eq!(v.read(&fork, "target").unwrap(), b"new\nlater\n");
    assert_eq!(v.read("main", "target").unwrap(), b"new\nlater\n");
}

#[test]
fn branch_moves_persist_ordered_queue_entries_across_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mut volume = Volume::open(tmp.path()).unwrap();
    put(&mut volume, "main", "shared", b"base\n");
    let fork = volume.fork().unwrap();
    let held = volume
        .write_open("main", "shared", "holder", false)
        .unwrap();
    put(&mut volume, &fork, "shared", b"child\n");
    while volume.progress().unwrap() {}
    let child_head = volume.head(&fork).unwrap();
    let repo = repo(tmp.path());
    let parent_queue = repo.list_refs("refs/queue/main/").unwrap();
    assert!(!parent_queue.is_empty());
    assert_eq!(parent_queue.last().unwrap().1, child_head);
    assert!(volume.metrics().waits > 0);
    assert!(volume.metrics().queued >= parent_queue.len() as u64);
    assert_eq!(volume.read("main", "shared").unwrap(), b"base\n");
    let _ = held;
    drop(volume);
    let mut volume = Volume::open(tmp.path()).unwrap();
    while volume.progress().unwrap() {}
    assert_eq!(volume.read("main", "shared").unwrap(), b"child\n");
    assert!(repo.list_refs("refs/queue/main/").unwrap().is_empty());
    assert!(
        repo.is_ancestor(child_head, volume.head("main").unwrap())
            .unwrap()
    );
}

#[test]
fn name_history_keeps_rename_and_unlink_versions_in_the_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let mut volume = Volume::open(tmp.path()).unwrap();
    put(&mut volume, "main", "a", b"content\n");
    volume.rename("main", "a", "b", "writer").unwrap();
    volume.unlink("main", "b", "writer").unwrap();
    let repo = repo(tmp.path());
    let root = repo.read_commit(volume.head("main").unwrap()).unwrap().tree;
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
    let volume = Volume::open(tmp.path()).unwrap();
    assert!(matches!(volume.read("main", "b"), Err(Error::NotFound)));
    let root = repo.read_commit(volume.head("main").unwrap()).unwrap().tree;
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
    let mut v = Volume::open(tmp.path()).unwrap();
    let fork = v.fork().unwrap();
    put(&mut v, "main", "a", b"main\n");
    put(&mut v, &fork, "b", b"fork\n");
    let refs = all_refs(tmp.path());
    assert!(refs.iter().any(|(name, _)| name.starts_with("refs/queue/")));
    drop(v);
    let gitdir = tmp.path().join("history.git");
    scribble(&gitdir.join("refs"));
    fs::write(gitdir.join("HEAD"), b"garbage\n").unwrap();
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(all_refs(tmp.path()), refs);
    assert_eq!(v.read("main", "a").unwrap(), b"main\n");
    assert_eq!(v.read(&fork, "b").unwrap(), b"fork\n");
    let fsck = git(tmp.path(), &["fsck", "--full", "--strict", "--no-progress"]);
    assert!(fsck.status.success(), "{fsck:?}");
}

#[test]
fn removed_refs_are_rebuilt_from_the_journal() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    put(&mut v, "main", "a", b"main\n");
    let refs = all_refs(tmp.path());
    drop(v);
    fs::remove_dir_all(tmp.path().join("history.git/refs")).unwrap();
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(all_refs(tmp.path()), refs);
    assert_eq!(v.read("main", "a").unwrap(), b"main\n");
}

#[test]
fn an_emptied_config_is_rewritten() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    let fork = v.fork().unwrap();
    drop(v);
    fs::write(tmp.path().join("history.git/config"), b"").unwrap();
    fs::write(tmp.path().join("history.git/HEAD"), b"").unwrap();
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.tree_names(), vec!["fork/1".to_owned(), "main".to_owned()]);
    let get =
        |key: &str| String::from_utf8(git(tmp.path(), &["config", "--get", key]).stdout).unwrap();
    assert_eq!(get("core.fsync"), "objects,derived-metadata\n");
    assert_eq!(get("core.fsyncMethod"), "fsync\n");
    assert_eq!(get(&format!("branch.{fork}.merge")), "refs/heads/main\n");
}

#[test]
fn recovery_reads_pointers_from_the_journal() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    put(&mut v, "main", "a", b"first");
    let first = v.head("main").unwrap();
    put(&mut v, "main", "a", b"second");
    let second = v.head("main").unwrap();
    drop(v);
    assert!(
        git(
            tmp.path(),
            &["update-ref", "refs/heads/main", &first.to_string()]
        )
        .status
        .success()
    );
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.head("main").unwrap(), second);
    assert_eq!(v.read("main", "a").unwrap(), b"second");
    assert_eq!(
        repo(tmp.path()).head("refs/heads/main").unwrap(),
        Some(second)
    );
}

#[test]
fn a_ref_copy_failure_is_not_a_call_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    put(&mut v, "main", "a", b"base");
    let before = v.head("main").unwrap();
    let h = v.write_open("main", "a", "writer", false).unwrap();
    v.write(h, 0, b"!", true).unwrap();
    let lock = tmp.path().join("history.git/refs/heads/main.lock");
    fs::write(&lock, b"stale").unwrap();
    assert!(matches!(v.close(h).unwrap(), Saved::Commit(_)));
    assert!(!v.is_stopped());
    let recorded = v.head("main").unwrap();
    assert_ne!(recorded, before);
    assert_eq!(v.read("main", "a").unwrap(), b"base!");
    let copy = repo(tmp.path());
    assert_eq!(copy.head("refs/heads/main").unwrap(), Some(before));
    // The next step rebuilds the copy whole and redoes the checkout.
    put(&mut v, "main", "b", b"next");
    assert_eq!(
        copy.head("refs/heads/main").unwrap(),
        Some(v.head("main").unwrap())
    );
    assert!(copy.is_ancestor(recorded, v.head("main").unwrap()).unwrap());
    assert!(!lock.exists());
    assert_eq!(fs::read(tmp.path().join("trees/main/a")).unwrap(), b"base!");
    assert_eq!(fs::read(tmp.path().join("trees/main/b")).unwrap(), b"next");
}

#[test]
fn a_bad_final_frame_is_a_torn_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    let h = v.write_open("main", "a", "writer", true).unwrap();
    v.write(h, 0, b"returned", false).unwrap();
    v.write(h, 8, b"-torn", false).unwrap();
    drop(v);
    let path = tmp.path().join("calls.wal");
    let mut journal = fs::read(&path).unwrap();
    let last = journal.len() - 1;
    journal[last] ^= 1;
    let (offset, length) = *frames(&journal).last().unwrap();
    fs::write(&path, &journal).unwrap();
    let v = Volume::open(tmp.path()).unwrap();
    // The last record is dropped; every one before it stands, closed once.
    assert_eq!(v.read("main", "a").unwrap(), b"returned");
    assert_eq!(opens_saved(tmp.path(), h), 1);
    assert_eq!(
        fs::read(tmp.path().join("calls.wal.torn")).unwrap(),
        journal[offset..offset + length]
    );
}

#[test]
fn a_bad_frame_before_a_good_one_is_corruption() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    let h = v.write_open("main", "a", "writer", true).unwrap();
    v.write(h, 0, b"one", false).unwrap();
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
            Volume::open(tmp.path()),
            Err(Error::CorruptJournal(found)) if found == offset
        ));
        assert_eq!(fs::read(&path).unwrap(), bad);
    }
    fs::write(&path, &journal).unwrap();
    assert_eq!(
        Volume::open(tmp.path()).unwrap().read("main", "a").unwrap(),
        b"onetwo"
    );
}

#[test]
fn a_frame_in_a_workloads_bytes_is_not_a_frame() {
    // A whole valid frame of some other journal, with a high number.
    fn forged(salt: u32) -> Vec<u8> {
        let head =
            br#"{"seq":999999,"at_ns":0,"entry":{"AnonymousOpen":{"tree":"main","writer":"w"}}}"#;
        let mut frame = b"AFJ2".to_vec();
        frame.extend_from_slice(&salt.to_le_bytes());
        frame.extend_from_slice(&(head.len() as u32).to_le_bytes());
        frame.extend_from_slice(&0_u32.to_le_bytes());
        frame.extend_from_slice(&crc32fast::hash(head).to_le_bytes());
        let header_crc = crc32fast::hash(&frame);
        frame.extend_from_slice(&header_crc.to_le_bytes());
        frame.extend_from_slice(head);
        frame
    }
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    let path = tmp.path().join("calls.wal");
    let salt = u32::from_le_bytes(fs::read(&path).unwrap()[4..8].try_into().unwrap());
    let h = v.write_open("main", "a", "writer", true).unwrap();
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
        let v = Volume::open(copy.path()).unwrap();
        v.read("main", "a").unwrap().to_vec()
    };
    assert_eq!(saved, [b"returned".as_slice(), &bytes].concat());
    // Torn, the forged frame cannot make its tail look like corruption.
    let journal = fs::read(&path).unwrap();
    fs::write(&path, &journal[..journal.len() - 1]).unwrap();
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.read("main", "a").unwrap(), b"returned");
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
fn a_lost_blob_at_a_tip_refuses_to_serve() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    put(&mut v, "main", "a", b"will be lost");
    let copy = repo(tmp.path());
    let root = copy.read_commit(v.head("main").unwrap()).unwrap().tree;
    let blob = entry(&copy, entry(&copy, root, "files"), "a");
    drop(v);
    let name = blob.to_string();
    fs::remove_file(
        tmp.path()
            .join("history.git/objects")
            .join(&name[..2])
            .join(&name[2..]),
    )
    .unwrap();
    assert!(matches!(
        Volume::open(tmp.path()),
        Err(Error::MissingObject { name, oid })
            if oid == blob && name.starts_with("refs/")
    ));
}

#[test]
fn a_missing_journal_refuses_and_keeps_history() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    put(&mut v, "main", "a", b"history");
    let head = v.head("main").unwrap();
    drop(v);
    fs::remove_file(tmp.path().join("calls.wal")).unwrap();
    assert!(matches!(
        Volume::open(tmp.path()),
        Err(Error::CorruptJournal(0))
    ));
    fs::write(tmp.path().join("calls.wal"), b"").unwrap();
    assert!(matches!(
        Volume::open(tmp.path()),
        Err(Error::CorruptJournal(0))
    ));
    assert_eq!(
        repo(tmp.path()).head("refs/heads/main").unwrap(),
        Some(head)
    );
    assert!(tmp.path().join("format").exists());
    fs::remove_file(tmp.path().join("format")).unwrap();
    assert!(matches!(Volume::open(tmp.path()), Err(Error::Format)));
    assert_eq!(
        repo(tmp.path()).head("refs/heads/main").unwrap(),
        Some(head)
    );
}

#[test]
fn an_earlier_format_does_not_open() {
    let tmp = tempfile::tempdir().unwrap();
    drop(Volume::open(tmp.path()).unwrap());
    fs::write(tmp.path().join("format"), b"agentfs-trees-1\n").unwrap();
    assert!(matches!(Volume::open(tmp.path()), Err(Error::Format)));
}

#[test]
fn open_numbers_never_repeat_across_compaction() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    let first = put(&mut v, "main", "a", b"one");
    drop(v);
    // Every open compacts; the journal then holds only a checkpoint.
    let mut v = Volume::open(tmp.path()).unwrap();
    assert_eq!(
        frames(&fs::read(tmp.path().join("calls.wal")).unwrap()).len(),
        1
    );
    let second = v.write_open("main", "a", "writer", false).unwrap();
    assert!(second > first);
    v.write(second, 0, b"two", true).unwrap();
    v.close(second).unwrap();
    drop(v);
    let mut v = Volume::open(tmp.path()).unwrap();
    let third = v.write_open("main", "a", "writer", false).unwrap();
    assert!(third > second);
    assert_eq!(opens_saved(tmp.path(), second), 1);
}

/// Takes the journal past the compaction threshold through real calls, leaving
/// `FILLED` in the file so its save stays cheap.
const FILLED: &[u8] = b"xxxxxx";
fn fill_journal(v: &mut Volume, path: &str) {
    let h = v.write_open("main", path, "filler", true).unwrap();
    let chunk = vec![b'x'; 33 << 20];
    for _ in 0..2 {
        v.write(h, 0, &chunk, true).unwrap();
    }
    v.truncate(h, FILLED.len() as u64).unwrap();
    v.close(h).unwrap();
}

#[test]
fn a_log_held_open_does_not_pin_other_saves() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    let log = v.write_open("main", "log", "logger", true).unwrap();
    v.write(log, 0, b"before\n", true).unwrap();
    fill_journal(&mut v, "big");
    // The step compacted: the other file's calls are gone, the log's stayed.
    let length = fs::metadata(tmp.path().join("calls.wal")).unwrap().len();
    assert!(length < 1 << 20, "journal holds {length} bytes");
    v.write(log, 0, b"after\n", true).unwrap();
    drop(v);
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.read("main", "log").unwrap(), b"before\nafter\n");
    assert_eq!(v.read("main", "big").unwrap(), FILLED);
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
    let mut v = Volume::open(std::path::Path::new(&root)).unwrap();
    let h = v.write_open("main", "a", "writer", true).unwrap();
    v.write(h, 0, b"returned", false).unwrap();
    println!("OPEN {h}");
    std::io::stdout().flush().unwrap();
    v.close(h).unwrap();
    unreachable!("the failpoint kills this process");
}

#[cfg(feature = "failpoints")]
#[test]
fn compact_worker() {
    let Some(root) = std::env::var_os("AGENTFS_TREE_STEP_WORKER") else {
        return;
    };
    let mut v = Volume::open(std::path::Path::new(&root)).unwrap();
    let log = v.write_open("main", "log", "logger", true).unwrap();
    v.write(log, 0, b"returned\n", true).unwrap();
    println!("OPEN {log}");
    std::io::stdout().flush().unwrap();
    fill_journal(&mut v, "big");
    unreachable!("the failpoint kills this process");
}

#[cfg(feature = "failpoints")]
#[test]
fn a_step_killed_before_its_record_saves_at_restart() {
    let tmp = tempfile::tempdir().unwrap();
    // The create is the first hit; the close is the second.
    for spec in [
        "tree.before_objects=kill@2",
        "tree.after_objects_durable=kill@2",
    ] {
        let state = tmp.path().join(spec);
        let open = run_killed_worker("step_worker", &state, spec);
        let v = Volume::open(&state).unwrap();
        assert_eq!(v.read("main", "a").unwrap(), b"returned", "{spec}");
        let head = v.head("main").unwrap();
        assert_eq!(opens_saved(&state, open), 1, "{spec}");
        drop(v);
        let v = Volume::open(&state).unwrap();
        assert_eq!(v.head("main").unwrap(), head, "{spec}");
        assert_eq!(opens_saved(&state, open), 1, "{spec}");
    }
}

#[cfg(feature = "failpoints")]
#[test]
fn a_step_killed_after_its_record_is_not_saved_again() {
    let tmp = tempfile::tempdir().unwrap();
    for spec in [
        "tree.after_commit_point=kill@2",
        "tree.after_ref_copy=kill@2",
        "tree.before_checkout=kill@2",
        "tree.after_checkout=kill@2",
    ] {
        let state = tmp.path().join(spec);
        let open = run_killed_worker("step_worker", &state, spec);
        let v = Volume::open(&state).unwrap();
        assert_eq!(v.read("main", "a").unwrap(), b"returned", "{spec}");
        assert_eq!(opens_saved(&state, open), 1, "{spec}");
        assert_eq!(
            repo(&state).head("refs/heads/main").unwrap(),
            Some(v.head("main").unwrap()),
            "{spec}"
        );
        assert_eq!(
            fs::read(state.join("trees/main/a")).unwrap(),
            b"returned",
            "{spec}"
        );
    }
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
        let v = Volume::open(&state).unwrap();
        assert_eq!(v.read("main", "log").unwrap(), b"returned\n", "{spec}");
        assert_eq!(v.read("main", "big").unwrap(), FILLED, "{spec}");
        assert_eq!(opens_saved(&state, log), 1, "{spec}");
    }
}

#[cfg(feature = "failpoints")]
#[test]
fn a_failure_after_the_rename_stops_the_actor() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    put(&mut v, "main", "a", b"before");
    let guard = agentfs_core::failpoints::configure("tree.compact_after_rename=error").unwrap();
    fill_journal(&mut v, "big");
    drop(guard);
    // The step itself stands: its record is in the old file and the new one.
    assert!(v.is_stopped());
    assert!(matches!(v.read("main", "a"), Err(Error::Stopped)));
    assert!(matches!(
        v.write_open("main", "a", "writer", false),
        Err(Error::Stopped)
    ));
    assert!(matches!(v.progress(), Err(Error::Stopped)));
    drop(v);
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.read("main", "a").unwrap(), b"before");
    assert_eq!(v.read("main", "big").unwrap(), FILLED);
}

#[cfg(feature = "failpoints")]
#[test]
fn a_failure_before_the_rename_is_retried_at_the_next_step() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    let guard = agentfs_core::failpoints::configure("tree.compact_before_rename=error").unwrap();
    fill_journal(&mut v, "big");
    drop(guard);
    assert!(!v.is_stopped());
    let path = tmp.path().join("calls.wal");
    assert!(fs::metadata(&path).unwrap().len() > 64 << 20);
    put(&mut v, "main", "a", b"next");
    assert!(fs::metadata(&path).unwrap().len() < 1 << 20);
    assert_eq!(v.read("main", "a").unwrap(), b"next");
}

#[cfg(feature = "failpoints")]
#[test]
fn a_cut_first_init_starts_over() {
    let tmp = tempfile::tempdir().unwrap();
    let guard = agentfs_core::failpoints::configure("tree.init_before_format=error").unwrap();
    assert!(Volume::open(tmp.path()).is_err());
    drop(guard);
    assert!(tmp.path().join("initializing").exists());
    assert!(tmp.path().join("history.git").exists());
    assert!(!tmp.path().join("format").exists());
    let mut v = Volume::open(tmp.path()).unwrap();
    assert!(!tmp.path().join("initializing").exists());
    put(&mut v, "main", "a", b"fresh");
    assert_eq!(v.read("main", "a").unwrap(), b"fresh");
}

#[cfg(feature = "failpoints")]
#[test]
fn a_failed_save_keeps_serving_and_saves_at_the_next_step() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    put(&mut v, "main", "a", b"base");
    let h = v.write_open("main", "a", "writer", false).unwrap();
    v.write(h, 0, b"+returned", true).unwrap();
    let head = v.head("main").unwrap();
    for point in ["tree.before_objects", "tree.after_objects_durable"] {
        let guard = agentfs_core::failpoints::configure(&format!("{point}=error")).unwrap();
        assert!(v.close(h).is_err(), "{point}");
        drop(guard);
        assert!(!v.is_stopped(), "{point}");
        assert_eq!(v.read("main", "a").unwrap(), b"base", "{point}");
        assert_eq!(v.head("main").unwrap(), head, "{point}");
    }
    // Other work goes on, the failed close's handle still counting as open.
    let other = v.write_open("main", "other", "writer", true);
    assert!(other.is_ok());
    assert_eq!(v.read("main", "a").unwrap(), b"base+returned");
    assert_eq!(opens_saved(tmp.path(), h), 1);
    assert!(matches!(v.close(h), Err(Error::NotFound)));

    // A failed save a restart finds saves there, once.
    let h = v.write_open("main", "a", "writer", false).unwrap();
    v.write(h, 0, b"+again", true).unwrap();
    let guard = agentfs_core::failpoints::configure("tree.before_objects=error").unwrap();
    assert!(v.close(h).is_err());
    drop(guard);
    drop(v);
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.read("main", "a").unwrap(), b"base+returned+again");
    assert_eq!(opens_saved(tmp.path(), h), 1);
}

#[cfg(feature = "failpoints")]
#[test]
fn a_failed_take_stays_queued_for_the_next_event() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    let fork = v.fork().unwrap();
    put(&mut v, &fork, "a", b"sandbox");
    let guard = agentfs_core::failpoints::configure("tree.before_objects=error").unwrap();
    assert!(!v.progress().unwrap());
    drop(guard);
    assert!(!v.is_stopped());
    assert!(matches!(v.read("main", "a"), Err(Error::NotFound)));
    assert!(v.progress().unwrap());
    while v.progress().unwrap() {}
    assert_eq!(v.read("main", "a").unwrap(), b"sandbox");
}

#[cfg(feature = "failpoints")]
#[test]
fn a_failed_write_changes_nothing_and_keeps_serving() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    let h = v.write_open("main", "a", "writer", true).unwrap();
    v.write(h, 0, b"one", false).unwrap();
    let path = tmp.path().join("calls.wal");
    let journal = fs::read(&path).unwrap();
    for point in ["tree.wal_after_header", "tree.wal_after_payload"] {
        let guard = agentfs_core::failpoints::configure(&format!("{point}=error")).unwrap();
        assert!(v.write(h, 3, b"lost", false).is_err(), "{point}");
        drop(guard);
        assert!(!v.is_stopped(), "{point}");
        assert_eq!(v.read_open(h).unwrap(), b"one", "{point}");
        assert_eq!(fs::read(&path).unwrap(), journal, "{point}");
    }
    v.write(h, 3, b"two", false).unwrap();
    v.close(h).unwrap();
    assert_eq!(v.read("main", "a").unwrap(), b"onetwo");
}

#[cfg(feature = "failpoints")]
#[test]
fn a_failed_fsync_stops_the_actor() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    let h = v.write_open("main", "a", "writer", true).unwrap();
    v.write(h, 0, b"one", false).unwrap();
    let guard = agentfs_core::failpoints::configure("tree.wal_before_fsync=error").unwrap();
    assert!(v.write(h, 3, b"unknown", false).is_err());
    drop(guard);
    assert!(v.is_stopped());
    assert!(matches!(v.read("main", "a"), Err(Error::Stopped)));
    drop(v);
    let v = Volume::open(tmp.path()).unwrap();
    assert!(v.read("main", "a").unwrap().starts_with(b"one"));
}

#[cfg(feature = "failpoints")]
#[test]
fn branch_and_queue_recover_together_after_the_commit_point() {
    let tmp = tempfile::tempdir().unwrap();
    let mut volume = Volume::open(tmp.path()).unwrap();
    let fork = volume.fork().unwrap();
    let h = volume.write_open(&fork, "a", "writer", true).unwrap();
    volume.write(h, 0, b"saved", false).unwrap();
    let repo = repo(tmp.path());
    let before = repo.list_refs("refs/queue/main/").unwrap().len();
    let guard = agentfs_core::failpoints::configure("tree.after_commit_point=error").unwrap();
    assert!(matches!(volume.close(h).unwrap(), Saved::Commit(_)));
    drop(guard);
    let committed = volume.head(&fork).unwrap();
    drop(volume);
    let mut volume = Volume::open(tmp.path()).unwrap();
    assert_eq!(
        repo.head(&format!("refs/heads/{fork}")).unwrap(),
        Some(committed)
    );
    let queued = repo.list_refs("refs/queue/main/").unwrap();
    assert_eq!(queued.len(), before + 1);
    assert_eq!(queued.last().unwrap().1, committed);
    while volume.progress().unwrap() {}
    assert_eq!(volume.read(&fork, "a").unwrap(), b"saved");
    assert!(repo.list_refs("refs/queue/main/").unwrap().is_empty());
    assert!(
        repo.is_ancestor(committed, volume.head("main").unwrap())
            .unwrap()
    );
}

#[cfg(feature = "failpoints")]
#[test]
fn recovery_enqueues_once_after_failure_before_the_commit_point() {
    let tmp = tempfile::tempdir().unwrap();
    let mut volume = Volume::open(tmp.path()).unwrap();
    let fork = volume.fork().unwrap();
    let h = volume.write_open(&fork, "a", "writer", true).unwrap();
    volume.write(h, 0, b"saved", false).unwrap();
    let before_head = volume.head(&fork).unwrap();
    let repo = repo(tmp.path());
    let before = repo.list_refs("refs/queue/main/").unwrap();
    let guard = agentfs_core::failpoints::configure("tree.after_objects_durable=error").unwrap();
    assert!(volume.close(h).is_err());
    drop(guard);
    assert_eq!(
        repo.head(&format!("refs/heads/{fork}")).unwrap(),
        Some(before_head)
    );
    assert_eq!(repo.list_refs("refs/queue/main/").unwrap(), before);
    drop(volume);
    let mut volume = Volume::open(tmp.path()).unwrap();
    assert_eq!(volume.read(&fork, "a").unwrap(), b"saved");
    let queued = repo.list_refs("refs/queue/main/").unwrap();
    let saved = queued.last().unwrap().1;
    assert_ne!(saved, before_head);
    assert!(!before.iter().any(|(_, oid)| *oid == saved));
    while volume.progress().unwrap() {}
    let mut cursor = volume.head("main").unwrap();
    let mut takes = 0;
    loop {
        let commit = repo.read_commit(cursor).unwrap();
        if commit.parents.get(1) == Some(&saved) {
            takes += 1;
        }
        let Some(parent) = commit.parents.first() else {
            break;
        };
        cursor = *parent;
    }
    assert_eq!(takes, 1);
}
