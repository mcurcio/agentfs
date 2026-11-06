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
fn state_from_the_first_recording_increment_keeps_its_history_and_pending_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    put(&mut v, "main", "a", b"saved");
    let saved_head = v.head("main").unwrap();
    let h = v.write_open("main", "a", "writer", false).unwrap();
    v.write(h, 0, b"-pending", true).unwrap();
    drop(v);
    fs::write(tmp.path().join("format"), b"agentfs-prototype-1\n").unwrap();
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.read("main", "a").unwrap(), b"saved-pending");
    let recovered_head = v.head("main").unwrap();
    let repo = repo(tmp.path());
    assert_eq!(
        repo.read_commit(recovered_head).unwrap().parents[0],
        saved_head
    );
    drop(v);
    assert_eq!(
        Volume::open(tmp.path()).unwrap().head("main").unwrap(),
        recovered_head
    );
}

#[test]
fn single_actor_and_invalid_names_fail_before_recording() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
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
    assert_eq!(fs::metadata(tmp.path().join("calls.wal")).unwrap().len(), 0);
}

#[test]
fn incomplete_tail_is_discarded_but_a_complete_corrupt_record_is_not() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    put(&mut v, "main", "a", b"saved");
    drop(v);
    let path = tmp.path().join("calls.wal");
    let valid = fs::read(&path).unwrap();
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"AFP")
        .unwrap();
    drop(Volume::open(tmp.path()).unwrap());
    assert_eq!(fs::read(&path).unwrap(), valid);
    let mut corrupt_header = valid.clone();
    corrupt_header[4] ^= 0x80;
    fs::write(&path, corrupt_header).unwrap();
    assert!(matches!(
        Volume::open(tmp.path()),
        Err(Error::CorruptJournal(_))
    ));
    let mut corrupt = valid;
    let last = corrupt.len() - 1;
    corrupt[last] ^= 1;
    fs::write(&path, corrupt).unwrap();
    assert!(matches!(
        Volume::open(tmp.path()),
        Err(Error::CorruptJournal(_))
    ));
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
        tmp.path().join("history.git/reftable/tables.list.lock"),
        b"stale",
    )
    .unwrap();
    assert!(v.close(h).is_err());
    assert!(v.is_stopped());
    assert!(matches!(v.read("main", "a"), Err(Error::Stopped)));
    drop(v);
    let mut v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.read("main", "a").unwrap(), b"basejournaled");
    let h = v.write_open("main", "a", "writer", false).unwrap();
    v.write(h, 0, b"once", true).unwrap();
    fs::remove_dir_all(tmp.path().join("trees/main")).unwrap();
    fs::write(tmp.path().join("trees/main"), b"checkout-blocked").unwrap();
    assert!(v.close(h).is_err()); // Ref moved, checkout failed.
    let repo = repo(tmp.path());
    let committed = repo.head("refs/heads/main").unwrap().unwrap();
    drop(v);
    fs::remove_file(tmp.path().join("trees/main")).unwrap();
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.head("main").unwrap(), committed);
    assert_eq!(v.read("main", "a").unwrap(), b"basejournaledonce");
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

#[cfg(feature = "failpoints")]
#[test]
fn branch_and_queue_recover_together_after_ref_transaction() {
    let tmp = tempfile::tempdir().unwrap();
    let mut volume = Volume::open(tmp.path()).unwrap();
    let fork = volume.fork().unwrap();
    let h = volume.write_open(&fork, "a", "writer", true).unwrap();
    volume.write(h, 0, b"saved", false).unwrap();
    let repo = repo(tmp.path());
    let before = repo.list_refs("refs/queue/main/").unwrap().len();
    let guard = agentfs_core::failpoints::configure("tree.after_refs_durable=error").unwrap();
    assert!(volume.close(h).is_err());
    drop(guard);
    let committed = repo.head(&format!("refs/heads/{fork}")).unwrap().unwrap();
    let queued = repo.list_refs("refs/queue/main/").unwrap();
    assert_eq!(queued.len(), before + 1);
    assert_eq!(queued.last().unwrap().1, committed);
    drop(volume);
    let mut volume = Volume::open(tmp.path()).unwrap();
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
fn recovery_enqueues_once_after_failure_before_ref_transaction() {
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
