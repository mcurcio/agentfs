//! The whole-file replace (`TreeActor::replace_file`; docs/library/core.md):
//! the atomic save a program makes by renaming a finished file over a
//! tracked one, as one step that keeps the file's identity and its unchanged
//! lines' identities, built as a close's save is.
#![allow(
    clippy::disallowed_methods,
    reason = "a test reads the store History writes"
)]

use std::fs;
use std::io::Read;
use std::path::Path;

use agentfs_contracts::clients::FileOwner;
use agentfs_core::tree_actor::{Attrs, Error, Kind, Perm, Saved, SetTime, TreeActor, TreeId};
use agentfs_core::tree_git::TreeGit;
use trees::{ATTRS, fanned, new_actor, reopen, rowless_fork, writer};

use crate::trees;

const ROOT: TreeId = TreeId::Root;

fn repo(root: &Path) -> TreeGit {
    TreeGit::open(root.join("history.git")).unwrap()
}

/// The blob at `path`, '/'-separated, in a commit's tree; `None` if absent.
fn at_path(repo: &TreeGit, commit: git2::Oid, path: &str) -> Option<Vec<u8>> {
    let mut oid = repo.read_commit(commit).unwrap().tree;
    for part in path.split('/') {
        oid = repo
            .list_tree(oid)
            .unwrap()
            .into_iter()
            .find(|e| e.name == part.as_bytes())?
            .oid;
    }
    Some(repo.read_blob(oid).unwrap())
}

/// A file's `[identity, line map, meta]` entry at a commit.
fn lines_entry(repo: &TreeGit, commit: git2::Oid, name: &str) -> serde_json::Value {
    serde_json::from_slice(&at_path(repo, commit, &format!("lines/{}", fanned(name))).unwrap())
        .unwrap()
}

/// What history holds of one file at a commit: its `files/` blob, its
/// `lines/` entry and its identity's `names/` entry.
fn entries(repo: &TreeGit, commit: git2::Oid, name: &str) -> (Vec<u8>, String, String) {
    let lines = lines_entry(repo, commit, name);
    let id = lines[0].as_u64().unwrap();
    let names = at_path(repo, commit, &format!("names/{}", fanned(&id.to_string()))).unwrap();
    (
        at_path(repo, commit, &format!("files/{}", fanned(name))).unwrap(),
        lines.to_string(),
        String::from_utf8(names).unwrap(),
    )
}

fn message(repo: &TreeGit, commit: git2::Oid) -> String {
    String::from_utf8(repo.read_commit(commit).unwrap().message).unwrap()
}

fn put(v: &mut TreeActor, tree: TreeId, path: &str, bytes: &[u8]) {
    let h = v.write_open(tree, path, &writer("w"), Some(ATTRS)).unwrap();
    v.truncate(h, 0).unwrap();
    v.write(h, 0, bytes, false).unwrap();
    v.close(h).unwrap();
}

fn replace(v: &mut TreeActor, tree: TreeId, path: &str, attrs: Attrs, bytes: &[u8]) -> Saved {
    v.replace_file(
        tree,
        path,
        attrs,
        &mut &bytes[..],
        bytes.len() as u64,
        &writer("w"),
    )
    .unwrap()
}

fn ten_lines() -> Vec<u8> {
    (1..=10)
        .map(|i| format!("line {i}\n"))
        .collect::<String>()
        .into_bytes()
}

/// Each live line's `field` (`identity` or `version`) event, in order
/// (`"e:i"` runs expanded).
fn live_events(entry: &serde_json::Value, field: &str) -> Vec<u64> {
    let mut events = Vec::new();
    for run in entry[1]["lines"].as_array().unwrap() {
        if run.get("dead").and_then(serde_json::Value::as_bool) == Some(true) {
            continue;
        }
        let named = run[field].as_str().unwrap();
        let event: u64 = named.split_once(':').unwrap().0.parse().unwrap();
        let count = run["count"].as_u64().unwrap();
        events.extend(std::iter::repeat_n(event, count as usize));
    }
    events
}

#[test]
fn a_replace_builds_what_a_truncating_open_write_and_close_build() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let mut va = new_actor(a.path());
    let mut vb = new_actor(b.path());
    for v in [&mut va, &mut vb] {
        put(v, ROOT, "f", &ten_lines());
    }
    let mut after = ten_lines();
    after.splice(0..0, b"first\n".iter().copied());
    after.extend_from_slice(b"last");
    // The twin's save is its sixth record (open, truncate, write, close
    // record, layout chunk, close): four steps that build no commit put the
    // replace at the same number.
    for _ in 0..4 {
        va.set_times(ROOT, "", None, Some(SetTime::Now), &writer("w"))
            .unwrap();
    }
    let Saved::Commit(replaced) = replace(&mut va, ROOT, "f", ATTRS, &after) else {
        panic!("a replace that changes bytes commits");
    };
    let h = vb
        .write_open_truncating(ROOT, "f", &writer("w"), None)
        .unwrap();
    vb.write(h, 0, &after, false).unwrap();
    let Saved::Commit(saved) = vb.close(h).unwrap() else {
        panic!("saves");
    };
    assert_eq!(replaced, saved, "the twins' steps share a number");
    let (ra, rb) = (repo(a.path()), repo(b.path()));
    let (ha, hb) = (va.head(ROOT).unwrap(), vb.head(ROOT).unwrap());
    assert_eq!(entries(&ra, ha, "f"), entries(&rb, hb, "f"));
    assert_eq!(va.read(ROOT, "f").unwrap(), after);
    let text = message(&ra, ha);
    assert!(text.starts_with("Direct by w\n"), "{text}");
    assert!(!text.contains("Agentfs-Open"), "{text}");
    assert!(message(&rb, hb).contains("Agentfs-Open"));
}

#[test]
fn a_replace_that_changes_one_line_keeps_the_other_nine() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "f", &ten_lines());
    let repo = repo(tmp.path());
    let before = lines_entry(&repo, v.head(ROOT).unwrap(), "f");
    let old = live_events(&before, "version")[0];
    let changed = String::from_utf8(ten_lines())
        .unwrap()
        .replace("line 5\n", "line five\n");
    let Saved::Commit(step) = replace(&mut v, ROOT, "f", ATTRS, changed.as_bytes()) else {
        panic!("commits");
    };
    let after = lines_entry(&repo, v.head(ROOT).unwrap(), "f");
    assert_eq!(after[0], before[0], "the file keeps its identity");
    // Every line keeps its identity; only the changed one has a new version.
    assert_eq!(live_events(&after, "identity"), vec![old; 10]);
    let versions = live_events(&after, "version");
    assert_eq!(versions.iter().filter(|e| **e == old).count(), 9);
    assert_eq!(versions[4], step.get(), "the changed line is the replace's");
}

#[test]
fn a_replace_onto_a_new_path_creates_it_with_its_attributes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    v.mkdir(ROOT, "d", ATTRS, &writer("w")).unwrap();
    let attrs = Attrs {
        perm: Perm::from_mode(0o600),
        owner: FileOwner { uid: 7, gid: 8 },
    };
    let Saved::Commit(step) = replace(&mut v, ROOT, "d/new", attrs, b"fresh\n") else {
        panic!("commits");
    };
    assert_eq!(v.read(ROOT, "d/new").unwrap(), b"fresh\n");
    let node = v.node(ROOT, "d/new").unwrap();
    assert_eq!(node.number, Some(step.get()), "its object is the step's");
    assert_eq!(node.meta.kind, Kind::File);
    assert_eq!(node.meta.mode.bits(), 0o600);
    assert_eq!((node.meta.uid, node.meta.gid), (7, 8));
    let repo = repo(tmp.path());
    let entry = lines_entry(&repo, v.head(ROOT).unwrap(), "d/new");
    assert_eq!(entry[0].as_u64(), Some(step.get()));
    assert_eq!(entry[2]["mode"], "0600");
}

#[test]
fn same_bytes_and_metadata_change_nothing_and_metadata_alone_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "f", b"same\n");
    let head = v.head(ROOT).unwrap();
    assert_eq!(
        replace(&mut v, ROOT, "f", ATTRS, b"same\n"),
        Saved::Unchanged
    );
    assert_eq!(v.head(ROOT).unwrap(), head);
    let attrs = Attrs {
        perm: Perm::from_mode(0o755),
        ..ATTRS
    };
    assert!(matches!(
        replace(&mut v, ROOT, "f", attrs, b"same\n"),
        Saved::Commit(_)
    ));
    let moved = v.head(ROOT).unwrap();
    assert_ne!(moved, head);
    assert_eq!(v.node(ROOT, "f").unwrap().meta.mode.bits(), 0o755);
    let repo = repo(tmp.path());
    assert_eq!(lines_entry(&repo, moved, "f")[2]["mode"], "0755");
}

#[test]
fn a_path_the_rules_leave_out_directories_links_and_missing_parents_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    drop(new_actor(tmp.path()));
    let info = tmp.path().join("history.git/info");
    fs::create_dir_all(&info).unwrap();
    fs::write(info.join("exclude"), "*.log\n").unwrap();
    let mut v = reopen(tmp.path());
    let w = writer("w");
    v.mkdir(ROOT, "d", ATTRS, &w).unwrap();
    v.symlink(ROOT, "l", "d", ATTRS.owner, &w).unwrap();
    let before = v.metrics().journal_bytes;
    let mut src: &[u8] = b"x";
    let result = v.replace_file(ROOT, "a.log", ATTRS, &mut src, 1, &w);
    match result {
        Err(error @ Error::Untracked { .. }) => assert_eq!(error.errno(), libc::EXDEV),
        other => panic!("expected Untracked, got {other:?}"),
    }
    assert!(matches!(
        v.replace_file(ROOT, "d", ATTRS, &mut &b"x"[..], 1, &w),
        Err(Error::IsADirectory)
    ));
    assert!(matches!(
        v.replace_file(ROOT, "l", ATTRS, &mut &b"x"[..], 1, &w),
        Err(Error::Invalid)
    ));
    assert!(matches!(
        v.replace_file(ROOT, "none/f", ATTRS, &mut &b"x"[..], 1, &w),
        Err(Error::NotFound)
    ));
    assert_eq!(v.metrics().journal_bytes, before, "nothing journaled");
}

/// A source that gives `ok` bytes, then fails.
struct Failing {
    ok: u64,
}

impl Read for Failing {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.ok == 0 {
            return Err(std::io::Error::other("the source failed"));
        }
        let n = buf.len().min(usize::try_from(self.ok).unwrap());
        buf[..n].fill(b'z');
        self.ok -= n as u64;
        Ok(n)
    }
}

#[test]
fn a_source_short_of_its_length_leaves_the_file_and_later_chunked_steps_work() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"base\n");
    put(&mut v, ROOT, "f", b"old\n");
    let head = v.head(ROOT).unwrap();
    let w = writer("w");
    match v.replace_file(ROOT, "f", ATTRS, &mut &b"abc"[..], 10, &w) {
        Err(Error::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
        other => panic!("expected UnexpectedEof, got {other:?}"),
    }
    // One whole chunk record, then the source fails: the record is left
    // with no step after it.
    let chunk = 16 << 20;
    let failed = v.replace_file(ROOT, "f", ATTRS, &mut Failing { ok: chunk }, chunk + 10, &w);
    assert!(matches!(failed, Err(Error::Io(_))), "{failed:?}");
    assert_eq!(v.read(ROOT, "f").unwrap(), b"old\n");
    assert_eq!(v.head(ROOT).unwrap(), head);
    // A landing merge carries its files in chunk records, and so does the
    // next replace: neither reads the abandoned one.
    let fork = rowless_fork(&mut v);
    put(&mut v, fork, "b", b"from the fork\n");
    put(&mut v, ROOT, "c", b"from the root\n");
    while v.progress().unwrap() {}
    assert_eq!(v.read(ROOT, "b").unwrap(), b"from the fork\n");
    assert!(matches!(
        replace(&mut v, ROOT, "f", ATTRS, b"new\n"),
        Saved::Commit(_)
    ));
    drop(v);
    let v = reopen(tmp.path());
    assert_eq!(v.read(ROOT, "f").unwrap(), b"new\n");
    assert_eq!(v.read(ROOT, "b").unwrap(), b"from the fork\n");
    let repo = repo(tmp.path());
    let head = v.head(ROOT).unwrap();
    assert_eq!(
        at_path(&repo, head, &format!("files/{}", fanned("f"))).unwrap(),
        b"new\n"
    );
}

#[test]
fn a_replace_survives_a_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "f", b"old\n");
    replace(&mut v, ROOT, "f", ATTRS, b"new\n");
    replace(&mut v, ROOT, "g", ATTRS, b"made\n");
    let object = v.object_at(ROOT, "f").unwrap();
    drop(v);
    let v = reopen(tmp.path());
    assert_eq!(v.read(ROOT, "f").unwrap(), b"new\n");
    assert_eq!(v.read(ROOT, "g").unwrap(), b"made\n");
    assert_eq!(v.object_at(ROOT, "f").unwrap(), object);
    let repo = repo(tmp.path());
    let head = v.head(ROOT).unwrap();
    assert_eq!(
        at_path(&repo, head, &format!("files/{}", fanned("g"))).unwrap(),
        b"made\n"
    );
}

#[cfg(feature = "failpoints")]
#[test]
fn a_crash_before_the_step_leaves_the_old_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    // Nothing is built: the reopen replays every record, the chunk records
    // the cut-short replace left among them.
    let guard = agentfs_failpoints::configure(
        "tree.build_before_batch=error@*;tree.replace_before_step=error",
    )
    .unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "f", b"old\n");
    let failed = v.replace_file(ROOT, "f", ATTRS, &mut &b"new\n"[..], 4, &writer("w"));
    assert!(failed.is_err());
    assert_eq!(v.read(ROOT, "f").unwrap(), b"old\n");
    v.durability().make_all_durable().unwrap();
    drop(v);
    drop(guard);
    let mut v = reopen(tmp.path());
    assert_eq!(v.read(ROOT, "f").unwrap(), b"old\n");
    assert!(matches!(
        replace(&mut v, ROOT, "f", ATTRS, b"newer\n"),
        Saved::Commit(_)
    ));
    drop(v);
    let v = reopen(tmp.path());
    assert_eq!(v.read(ROOT, "f").unwrap(), b"newer\n");
}

#[test]
fn a_forks_replace_lands_in_its_upstream_as_a_save_would() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "f", &ten_lines());
    let fork = rowless_fork(&mut v);
    let changed = String::from_utf8(ten_lines())
        .unwrap()
        .replace("line 9\n", "line nine\n");
    replace(&mut v, fork, "f", ATTRS, changed.as_bytes());
    while v.progress().unwrap() {}
    assert_eq!(v.read(ROOT, "f").unwrap(), changed.as_bytes());
    let repo = repo(tmp.path());
    let text = message(&repo, v.head(ROOT).unwrap());
    assert!(text.contains(&format!("Agentfs-Tree: {fork}")), "{text}");
    assert!(text.contains("Agentfs-Writer: w"), "{text}");
}
