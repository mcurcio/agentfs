//! The recording slice on real Git repositories and Linux byte operations.
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;

use agentfs_core::tree_actor::{Error, Saved, Volume};

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
    let repo = git2::Repository::open_bare(tmp.path().join("state/history.git")).unwrap();
    let commit = repo.find_commit(v.head("main").unwrap()).unwrap();
    assert!(
        commit
            .message()
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
    let repo = git2::Repository::open_bare(tmp.path().join("history.git")).unwrap();
    assert_eq!(
        repo.find_commit(v.head("main").unwrap())
            .unwrap()
            .parent_id(0)
            .unwrap(),
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
fn forks_are_real_branches_and_isolate_saves() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    put(&mut v, "main", "a", b"base");
    let fork = v.fork().unwrap();
    put(&mut v, &fork, "a", b"sandbox");
    put(&mut v, "main", "a", b"parent");
    assert_eq!(v.read(&fork, "a").unwrap(), b"sandbox");
    assert_eq!(v.read("main", "a").unwrap(), b"parent");
    let fork_head = v.head(&fork).unwrap();
    drop(v);
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.head(&fork).unwrap(), fork_head);
    assert_eq!(v.read(&fork, "a").unwrap(), b"sandbox");
    assert_eq!(
        fs::read(tmp.path().join("trees").join(&fork).join("a")).unwrap(),
        b"sandbox"
    );
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
    let repo = git2::Repository::open_bare(tmp.path().join("history.git")).unwrap();
    assert_eq!(
        repo.find_commit(recovered_head)
            .unwrap()
            .parent_id(0)
            .unwrap(),
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
fn concurrent_appends_cannot_exceed_the_limit_only_at_release() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = Volume::open(tmp.path()).unwrap();
    let a = v.write_open("main", "a", "one", true).unwrap();
    let b = v.write_open("main", "a", "two", false).unwrap();
    v.truncate(a, agentfs_core::tree_actor::MAX_FILE_SIZE as u64)
        .unwrap();
    assert!(matches!(v.write(b, 0, b"x", true), Err(Error::TooLarge)));
    v.close(a).unwrap();
    v.close(b).unwrap();
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
    let repo = git2::Repository::open_bare(tmp.path().join("history.git")).unwrap();
    let committed = repo.refname_to_id("refs/heads/main").unwrap();
    drop(v);
    fs::remove_file(tmp.path().join("trees/main")).unwrap();
    let v = Volume::open(tmp.path()).unwrap();
    assert_eq!(v.head("main").unwrap(), committed);
    assert_eq!(v.read("main", "a").unwrap(), b"basejournaledonce");
}
