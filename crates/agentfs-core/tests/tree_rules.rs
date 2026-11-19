//! The operator's rules and the untracked areas on the tree engine
//! (docs/trees.md, "Rules and the untracked area"): the guard on every call
//! that makes a name, rules read once at open, and each tree's area,
//! confined, copied into a new fork and removed with it.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::Path;

use agentfs_contracts::clients::FileOwner;
use agentfs_core::tree_actor::{Error, Kind, Meta, Perm, RenameMode, TreeActor, TreeId};
use trees::{ATTRS, new_actor, reopen, rowless_fork, writer};

#[path = "support/trees.rs"]
mod trees;

/// Writes the operator's rule files into a volume's git directory.
fn rules(dir: &Path, exclude: &str, attributes: &str) {
    let info = dir.join("history.git/info");
    fs::create_dir_all(&info).unwrap();
    fs::write(info.join("exclude"), exclude).unwrap();
    fs::write(info.join("attributes"), attributes).unwrap();
}

/// A volume whose rules keep `*.log`, `/sessions/` and `*.db` out.
fn ruled(dir: &Path) -> TreeActor {
    drop(new_actor(dir));
    rules(dir, "*.log\n/sessions/\n", "*.db -agentfs\n");
    reopen(dir)
}

fn untracked(result: Result<impl std::fmt::Debug, Error>) -> (String, String) {
    match result {
        Err(error @ Error::Untracked { .. }) => {
            assert_eq!(error.errno(), libc::EXDEV);
            let Error::Untracked { path, reason } = error else {
                unreachable!()
            };
            (path, reason.to_string())
        }
        other => panic!("expected Untracked, got {other:?}"),
    }
}

#[test]
fn every_call_that_makes_a_name_refuses_one_the_rules_leave_out_and_journals_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = ruled(tmp.path());
    let w = writer("w");
    let h = v
        .write_open(TreeId::Root, "notes.md", &w, Some(ATTRS))
        .unwrap();
    v.close(h).unwrap();
    let anonymous = v.anonymous_open(TreeId::Root, &w, ATTRS).unwrap();
    let object = v.open_object(anonymous).unwrap();
    let before = v.metrics().journal_bytes;

    let (path, reason) = untracked(v.write_open(TreeId::Root, "a.log", &w, Some(ATTRS)));
    assert_eq!(
        (path.as_str(), reason.as_str()),
        ("a.log", "ignored by info/exclude (*.log)")
    );
    let (_, reason) = untracked(v.write_open_truncating(TreeId::Root, "s.db", &w, Some(ATTRS)));
    assert_eq!(reason, "marked -agentfs in info/attributes");
    untracked(v.mkdir(TreeId::Root, "sessions", ATTRS, &w));
    // A `.git` name is no git-safe name: the engine refuses it first.
    assert!(matches!(
        v.mkdir(TreeId::Root, ".git", ATTRS, &w),
        Err(Error::Name)
    ));
    let owner = FileOwner {
        uid: 1000,
        gid: 1000,
    };
    untracked(v.symlink(TreeId::Root, "l.log", "notes.md", owner, &w));
    untracked(v.first_link(TreeId::Root, object, "x.db", &w));
    untracked(v.rename(
        TreeId::Root,
        "notes.md",
        "notes.log",
        RenameMode::Replace,
        &w,
    ));
    untracked(v.rename(TreeId::Root, "notes.md", "n.db", RenameMode::NoReplace, &w));
    assert_eq!(v.metrics().journal_bytes, before, "nothing journaled");
    assert!(v.node(TreeId::Root, "notes.md").is_ok());

    // A directory the rules exclude only when new: `sessions` as a file is fine.
    let h = v
        .write_open(TreeId::Root, "sessions", &w, Some(ATTRS))
        .unwrap();
    v.close(h).unwrap();
    v.unlink(TreeId::Root, "sessions", &w).unwrap();
}

#[test]
fn rules_decide_only_for_new_names() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let w = writer("w");
    let h = v
        .write_open(TreeId::Root, "a.log", &w, Some(ATTRS))
        .unwrap();
    v.close(h).unwrap();
    v.mkdir(TreeId::Root, "logs", ATTRS, &w).unwrap();
    drop(v);
    rules(tmp.path(), "*.log\nlogs/\n", "");
    let mut v = reopen(tmp.path());
    // A tracked path stays recorded: reopened, written, created again.
    let h = v
        .write_open(TreeId::Root, "a.log", &w, Some(ATTRS))
        .unwrap();
    v.write(h, 0, b"still recorded", false).unwrap();
    v.close(h).unwrap();
    assert_eq!(v.read(TreeId::Root, "a.log").unwrap(), b"still recorded");
    assert_eq!(
        v.mkdir(TreeId::Root, "logs", ATTRS, &w)
            .unwrap_err()
            .errno(),
        libc::EEXIST
    );
    // A rename onto the tracked name replaces it.
    let h = v.write_open(TreeId::Root, "b", &w, Some(ATTRS)).unwrap();
    v.close(h).unwrap();
    v.rename(TreeId::Root, "b", "a.log", RenameMode::Replace, &w)
        .unwrap();
}

#[test]
fn a_rule_not_built_refuses_the_open_before_anything_is_written() {
    let tmp = tempfile::tempdir().unwrap();
    drop(new_actor(tmp.path()));
    rules(tmp.path(), "", "# keep\n*.db agentfs.group=x\n");
    let before = snapshot(tmp.path());
    let Err(error) = TreeActor::open(tmp.path(), &Default::default()) else {
        panic!("opened");
    };
    assert!(matches!(error, Error::Rules(_)), "{error:?}");
    assert_eq!(error.errno(), libc::EINVAL);
    assert_eq!(
        error.to_string(),
        "operator rules: info/attributes line 2: agentfs.group=x is not built for tree volumes"
    );
    assert_eq!(snapshot(tmp.path()), before, "the state area is unchanged");
    let Err(error) = agentfs_core::tree_actor::Recording::open(tmp.path(), &Default::default())
    else {
        panic!("opened");
    };
    assert!(matches!(error, Error::Rules(_)), "{error:?}");
}

#[test]
fn empty_or_missing_rule_files_change_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    drop(new_actor(tmp.path()));
    for (exclude, attributes) in [(None, None), (Some(""), Some(""))] {
        let info = tmp.path().join("history.git/info");
        let _ = fs::remove_file(info.join("exclude"));
        let _ = fs::remove_file(info.join("attributes"));
        if let (Some(e), Some(a)) = (exclude, attributes) {
            rules(tmp.path(), e, a);
        }
        let mut v = reopen(tmp.path());
        let w = writer("w");
        for path in ["x.log", "y.db", "sessions-file"] {
            let h = v.write_open(TreeId::Root, path, &w, Some(ATTRS)).unwrap();
            v.close(h).unwrap();
            v.unlink(TreeId::Root, path, &w).unwrap();
        }
        // A `.git` name is still refused: it is no git-safe name.
        assert!(matches!(
            v.mkdir(TreeId::Root, ".git", ATTRS, &w),
            Err(Error::Name)
        ));
    }
}

/// Every file's bytes under `dir`, but the lease file.
fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                let rel = path
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                if rel != "actor.lock" {
                    out.insert(rel, fs::read(&path).unwrap());
                }
            }
        }
    }
    out
}

fn meta(kind: Kind, mode: u32) -> Meta {
    let me = unsafe { (libc::geteuid(), libc::getegid()) };
    Meta {
        kind,
        mode: Perm::from_mode(mode),
        uid: me.0,
        gid: me.1,
    }
}

#[test]
fn an_area_is_confined_beneath_its_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let v = new_actor(tmp.path());
    let area = v.untracked_area(TreeId::Root).unwrap();
    assert_eq!(area.tree(), TreeId::Root);
    let base = area.dir().path().to_path_buf();
    assert_eq!(
        base,
        tmp.path().canonicalize().unwrap().join("untracked/root")
    );
    assert_eq!(fs::metadata(&base).unwrap().mode() & 0o777, 0o700);
    let outside = tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret"), b"x").unwrap();
    symlink(&outside, base.join("escape")).unwrap();
    symlink(outside.join("secret"), base.join("file-link")).unwrap();

    // A link on the way is never followed.
    assert!(area.parent("escape/x").is_err());
    assert!(area.stat("escape/secret").unwrap().is_none());
    assert!(
        area.place("escape/new", &mut &b"no"[..], 2, meta(Kind::File, 0o644))
            .is_err()
    );
    assert!(area.parent("../x").is_err());
    assert!(area.stat("../untracked").is_err() || area.stat("../untracked").unwrap().is_none());
    assert!(area.entries("escape").unwrap().is_empty());
    // A final link is the link itself, never its target.
    let st = area.stat("file-link").unwrap().unwrap();
    assert_eq!(st.st_mode & libc::S_IFMT, libc::S_IFLNK);
    // Placing over a final link replaces the link, never writes its target.
    area.place("file-link", &mut &b"mine"[..], 4, meta(Kind::File, 0o600))
        .unwrap();
    assert_eq!(fs::read(outside.join("secret")).unwrap(), b"x");
    assert_eq!(fs::read(base.join("file-link")).unwrap(), b"mine");
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
}

#[test]
fn containers_take_the_given_modes_and_place_replaces_atomically() {
    let tmp = tempfile::tempdir().unwrap();
    let v = new_actor(tmp.path());
    let area = v.untracked_area(TreeId::Root).unwrap();
    let base = area.dir().path().to_path_buf();
    let modes: BTreeMap<&str, u32> = [("a", 0o750), ("a/b", 0o711)].into();
    area.containers("a/b/c.log", &mut |dir| {
        modes.get(dir).map(|m| meta(Kind::Dir, *m))
    })
    .unwrap();
    assert_eq!(fs::metadata(base.join("a")).unwrap().mode() & 0o7777, 0o750);
    assert_eq!(
        fs::metadata(base.join("a/b")).unwrap().mode() & 0o7777,
        0o711
    );
    assert!(
        !base.join("a/b/c.log").exists(),
        "only the directories above"
    );

    area.place("a/b/c.log", &mut &b"one"[..], 3, meta(Kind::File, 0o640))
        .unwrap();
    let first = fs::metadata(base.join("a/b/c.log")).unwrap();
    assert_eq!(first.mode() & 0o7777, 0o640);
    // A reader that opened the old file keeps it: the new one is renamed in.
    let mut old = fs::File::open(base.join("a/b/c.log")).unwrap();
    area.place("a/b/c.log", &mut &b"two!"[..], 4, meta(Kind::File, 0o600))
        .unwrap();
    let mut text = String::new();
    std::io::Read::read_to_string(&mut old, &mut text).unwrap();
    assert_eq!(text, "one");
    assert_eq!(fs::read(base.join("a/b/c.log")).unwrap(), b"two!");
    assert_ne!(
        fs::metadata(base.join("a/b/c.log")).unwrap().ino(),
        first.ino()
    );
    area.place("a/l", &mut &b"b/c.log"[..], 7, meta(Kind::Symlink, 0o777))
        .unwrap();
    assert_eq!(
        fs::read_link(base.join("a/l")).unwrap(),
        Path::new("b/c.log")
    );
    // A short source places nothing.
    assert!(
        area.place("a/short", &mut &b"ab"[..], 5, meta(Kind::File, 0o644))
            .is_err()
    );
    assert!(!base.join("a/short").exists());
    let names: Vec<String> = area
        .entries("a")
        .unwrap()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(names, ["b", "l"]);
    assert!(
        fs::read_dir(tmp.path().join("untracked/.staging"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn a_fork_starts_with_a_copy_of_its_upstreams_area_and_loses_it_when_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let root = v.untracked_area(TreeId::Root).unwrap();
    let base = root.dir().path().to_path_buf();
    fs::create_dir(base.join("sessions")).unwrap();
    fs::write(base.join("sessions/s.json"), b"before").unwrap();
    fs::set_permissions(
        base.join("sessions/s.json"),
        fs::Permissions::from_mode(0o604),
    )
    .unwrap();
    symlink("sessions/s.json", base.join("latest")).unwrap();
    let fork = rowless_fork(&mut v);
    let copy = v.untracked_area(fork).unwrap();
    let forked = copy.dir().path().to_path_buf();
    assert_eq!(
        forked,
        tmp.path()
            .canonicalize()
            .unwrap()
            .join(format!("untracked/{fork}"))
    );
    assert_eq!(fs::read(forked.join("sessions/s.json")).unwrap(), b"before");
    assert_eq!(
        fs::metadata(forked.join("sessions/s.json")).unwrap().mode() & 0o7777,
        0o604
    );
    assert_eq!(
        fs::read_link(forked.join("latest")).unwrap(),
        Path::new("sessions/s.json")
    );
    // Later writes on either side stay on that side.
    fs::write(base.join("sessions/s.json"), b"upstream later").unwrap();
    fs::write(base.join("new"), b"x").unwrap();
    fs::write(forked.join("mine"), b"fork").unwrap();
    assert_eq!(fs::read(forked.join("sessions/s.json")).unwrap(), b"before");
    assert!(!forked.join("new").exists());
    assert!(!base.join("mine").exists());

    // Dropped: its area goes with the drop.
    v.settle().unwrap();
    #[allow(
        clippy::disallowed_methods,
        reason = "the engine's drop, below the store"
    )]
    v.drop_fork(fork, |_| Ok(Vec::new())).unwrap();
    v.settle().unwrap();
    assert!(!forked.exists(), "a dropped fork's area goes");
    assert!(base.join("new").exists(), "the upstream's stays");
}

#[test]
fn an_upstream_with_no_area_gives_a_fork_an_empty_one() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let fork = rowless_fork(&mut v);
    assert!(!tmp.path().join("untracked/root").exists());
    let area = v.untracked_area(fork).unwrap();
    assert!(area.entries("").unwrap().is_empty());
}

/// A crash mid-copy leaves a copy under `.staging/` and, past its publish,
/// an area no fork on record owns: the next open removes both, and the
/// fork's number made again starts from a fresh copy.
#[test]
fn a_copy_a_crash_cut_short_leaves_no_fork_and_is_removed_at_open() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let root = v.untracked_area(TreeId::Root).unwrap();
    fs::write(root.dir().path().join("db"), b"upstream").unwrap();
    let kept = rowless_fork(&mut v);
    drop(v);
    let untracked = tmp.path().join("untracked");
    // As a kill leaves them: a partial copy, and a published one whose fork
    // step never became durable.
    fs::create_dir_all(untracked.join(".staging/copy-fork-90/half")).unwrap();
    fs::create_dir_all(untracked.join("fork/91")).unwrap();
    fs::write(untracked.join("fork/91/stale"), b"x").unwrap();
    fs::create_dir_all(untracked.join("fork/not-a-number")).unwrap();
    let mut v = reopen(tmp.path());
    assert!(!untracked.join(".staging").exists());
    assert!(!untracked.join("fork/91").exists());
    assert!(
        untracked.join("fork/not-a-number").exists(),
        "never agentfs's"
    );
    assert!(
        untracked.join(format!("{kept}/db")).exists(),
        "a live fork's stays"
    );
    assert_eq!(v.trees(), [TreeId::Root, kept]);
    let next = rowless_fork(&mut v);
    let area = v.untracked_area(next).unwrap();
    let names: Vec<String> = area
        .entries("")
        .unwrap()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(names, ["db"]);
}

/// The copy holds the upstream's untracked changes: a change that holds the
/// area waits until the copy ends, so the copy is the area at one instant.
#[test]
fn a_copy_waits_for_changes_in_progress_and_holds_new_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let v = std::sync::Arc::new(std::sync::Mutex::new(new_actor(tmp.path())));
    let root = v.lock().unwrap().untracked_area(TreeId::Root).unwrap();
    let base = root.dir().path().to_path_buf();
    fs::write(base.join("wal"), b"").unwrap();
    let writing = root.writing();
    let (tx, rx) = std::sync::mpsc::channel();
    let forking = {
        let v = std::sync::Arc::clone(&v);
        std::thread::spawn(move || {
            let mut v = v.lock().unwrap();
            tx.send(()).unwrap();
            rowless_fork(&mut v)
        })
    };
    rx.recv().unwrap();
    // The fork waits on this change: what it writes is in the copy.
    let mut wal = fs::OpenOptions::new()
        .append(true)
        .open(base.join("wal"))
        .unwrap();
    wal.write_all(b"frame").unwrap();
    drop(writing);
    let fork = forking.join().unwrap();
    let area = v.lock().unwrap().untracked_area(fork).unwrap();
    assert_eq!(fs::read(area.dir().path().join("wal")).unwrap(), b"frame");
}

/// Each fork's copy is the upstream's area at one instant: a writer that
/// writes `n` to `a` then to `b`, each change holding the area, is never seen
/// with `a` and `b` more than one write apart, however the copy interleaves.
#[test]
fn every_copy_is_the_area_at_one_instant() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let root = std::sync::Arc::new(v.untracked_area(TreeId::Root).unwrap());
    let base = root.dir().path().to_path_buf();
    fs::write(base.join("a"), b"0").unwrap();
    fs::write(base.join("b"), b"0").unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (root, stop, base) = (
            std::sync::Arc::clone(&root),
            std::sync::Arc::clone(&stop),
            base.clone(),
        );
        std::thread::spawn(move || {
            let mut n = 0_u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                n += 1;
                for name in ["a", "b"] {
                    let _writing = root.writing();
                    fs::write(base.join(name), n.to_string()).unwrap();
                }
            }
        })
    };
    let mut forks = Vec::new();
    for _ in 0..20 {
        forks.push(rowless_fork(&mut v));
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
    for fork in forks {
        let area = v.untracked_area(fork).unwrap();
        let read = |name: &str| -> u64 {
            fs::read_to_string(area.dir().path().join(name))
                .unwrap()
                .parse()
                .unwrap()
        };
        let (a, b) = (read("a"), read("b"));
        assert!(a == b || a == b + 1, "{fork}: a {a}, b {b}");
    }
}

/// A file with two names in the upstream's area has two names in the
/// fork's copy, one file; a name that is not UTF-8, which no view makes,
/// is left out and does not stop the copy.
#[test]
fn a_copy_keeps_hard_links_and_leaves_out_names_not_utf8() {
    use std::os::unix::ffi::OsStrExt;
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let root = v.untracked_area(TreeId::Root).unwrap();
    let base = root.dir().path().to_path_buf();
    fs::create_dir(base.join("d")).unwrap();
    fs::write(base.join("d/a"), b"one file").unwrap();
    fs::hard_link(base.join("d/a"), base.join("b")).unwrap();
    let odd = base.join(std::ffi::OsStr::from_bytes(b"x\xff"));
    fs::create_dir(&odd).unwrap();
    fs::write(odd.join("in"), b"never a tree's").unwrap();
    let fork = rowless_fork(&mut v);
    let forked = v.untracked_area(fork).unwrap().dir().path().to_path_buf();
    let (a, b) = (
        fs::metadata(forked.join("d/a")).unwrap(),
        fs::metadata(forked.join("b")).unwrap(),
    );
    assert_eq!((a.dev(), a.ino(), a.nlink()), (b.dev(), b.ino(), 2));
    assert_eq!(fs::read(forked.join("b")).unwrap(), b"one file");
    let mut names: Vec<_> = fs::read_dir(&forked)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["b", "d"]);
    // The copy is its own: a write to the upstream's file is not the fork's.
    fs::write(base.join("b"), b"later").unwrap();
    assert_eq!(fs::read(forked.join("d/a")).unwrap(), b"one file");
}

/// The upstream's area for the copy-failure tests: three files.
#[cfg(feature = "failpoints")]
fn three_files(v: &mut TreeActor) {
    let base = v
        .untracked_area(TreeId::Root)
        .unwrap()
        .dir()
        .path()
        .to_path_buf();
    for name in ["a", "b", "c"] {
        fs::write(base.join(name), name).unwrap();
    }
}

/// The fork areas published under `<state>/untracked/fork/`, and what each holds.
#[cfg(feature = "failpoints")]
fn fork_areas(dir: &Path) -> BTreeMap<String, Vec<String>> {
    let forks = dir.join("untracked/fork");
    let Ok(entries) = fs::read_dir(&forks) else {
        return BTreeMap::new();
    };
    entries
        .map(|e| {
            let e = e.unwrap();
            let mut names: Vec<String> = fs::read_dir(e.path())
                .unwrap()
                .map(|f| f.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            (e.file_name().to_string_lossy().into_owned(), names)
        })
        .collect()
}

/// A copy that fails part way, or after its publish and before the fork's
/// record, leaves no fork and no area, in this process too: the next fork
/// takes the same number and gets the whole copy.
#[cfg(feature = "failpoints")]
#[test]
fn a_failed_copy_leaves_no_fork_and_no_area() {
    for spec in ["tree.fork_copy_file=error@2", "tree.fork_after_copy=error"] {
        let tmp = tempfile::tempdir().unwrap();
        let mut v = new_actor(tmp.path());
        three_files(&mut v);
        {
            let _guard = agentfs_core::failpoints::configure(spec).unwrap();
            #[allow(
                clippy::disallowed_methods,
                reason = "a tree actor test forks below the store"
            )]
            let failed = v.fork_with(TreeId::Root, |_, _| Ok(Vec::new()));
            assert!(failed.is_err(), "{spec}");
        }
        assert_eq!(v.trees(), [TreeId::Root], "{spec}");
        assert!(fork_areas(tmp.path()).is_empty(), "{spec}");
        let fork = rowless_fork(&mut v);
        assert_eq!(
            fork_areas(tmp.path()),
            BTreeMap::from([(
                fork.to_string()[5..].to_owned(),
                vec!["a".into(), "b".into(), "c".into()]
            )]),
            "{spec}"
        );
    }
}

/// Killed mid-copy or after its publish, before the fork's record: no fork
/// after the restart, and no area. Killed as the fork's step is written,
/// the area was already published whole: the copy comes before the record.
#[cfg(feature = "failpoints")]
#[test]
fn a_fork_killed_at_any_point_of_its_copy_has_its_whole_area_or_none() {
    use std::os::unix::process::ExitStatusExt;
    for spec in [
        "tree.fork_copy_file=kill@2",
        "tree.fork_after_copy=kill@1",
        "tree.before_step=kill@1",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "copy_worker", "--nocapture"])
            .env("AGENTFS_TREE_COPY_WORKER", &dir)
            .env(agentfs_core::failpoints::ENV_VAR, spec)
            .output()
            .unwrap();
        assert_eq!(
            output.status.signal(),
            Some(libc::SIGKILL),
            "{spec} did not kill the worker: {output:?}"
        );
        trees::wait_for_lease(&dir);
        let published = fork_areas(&dir);
        if spec.starts_with("tree.fork_copy_file") {
            assert!(published.is_empty(), "{spec}: published {published:?}");
        } else {
            let whole = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
            assert_eq!(
                published,
                BTreeMap::from([("1".to_owned(), whole)]),
                "{spec}: the copy is published whole before the step"
            );
        }
        let mut v = reopen(&dir);
        assert_eq!(v.trees(), [TreeId::Root], "{spec}");
        assert!(fork_areas(&dir).is_empty(), "{spec}: swept at open");
        assert!(!dir.join("untracked/.staging").exists(), "{spec}");
        let fork = rowless_fork(&mut v);
        let area = v.untracked_area(fork).unwrap();
        assert_eq!(area.entries("").unwrap().len(), 3, "{spec}");
    }
}

/// The worker [`a_fork_killed_at_any_point_of_its_copy_has_its_whole_area_or_none`]
/// runs: an upstream area of three files, then a fork the failpoint kills.
#[cfg(feature = "failpoints")]
#[test]
fn copy_worker() {
    let Some(dir) = std::env::var_os("AGENTFS_TREE_COPY_WORKER") else {
        return;
    };
    let mut v = new_actor(Path::new(&dir));
    three_files(&mut v);
    rowless_fork(&mut v);
    unreachable!("the failpoint kills this process");
}

/// A fork dropped before anything built it loses its area with the drop,
/// copied secrets included, not at the next open.
#[test]
fn a_fork_dropped_before_any_build_loses_its_area_at_the_drop() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    let base = v
        .untracked_area(TreeId::Root)
        .unwrap()
        .dir()
        .path()
        .to_path_buf();
    fs::write(base.join(".env"), b"TOKEN=x").unwrap();
    let fork = rowless_fork(&mut v);
    let forked = tmp.path().join(format!("untracked/{fork}"));
    assert!(forked.join(".env").exists());
    #[allow(
        clippy::disallowed_methods,
        reason = "the engine's drop, below the store"
    )]
    v.drop_fork(fork, |_| Ok(Vec::new())).unwrap();
    assert!(!forked.exists(), "removed as the drop returns");
}
