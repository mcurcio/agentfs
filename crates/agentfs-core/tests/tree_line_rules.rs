//! Line rules R10.8, R10.6 and R10.9 through the real `TreeActor`: its saves, its
//! queue and its Git history. Target git-model §7.
#![allow(
    clippy::disallowed_methods,
    reason = "a test reads the store History writes"
)]
use agentfs_core::tree_actor::{TreeActor, TreeId};
use trees::{fanned, new_actor, writer};

const ROOT: TreeId = TreeId::Root;
use agentfs_core::tree_git::TreeGit;

#[path = "support/trees.rs"]
mod trees;

const BASE: &[u8] = b"-\n-\n-\n-\n-\n-\n";
const PARENT: &[u8] = b"-\nm1.1\n-\n-\n-\n-\n";

fn put(v: &mut TreeActor, tree: TreeId, path: &str, bytes: &[u8]) {
    let h = v
        .write_open(tree, path, &writer("test-writer"), true)
        .unwrap();
    v.truncate(h, 0).unwrap();
    v.write(h, 0, bytes, false).unwrap();
    v.close(h).unwrap();
}

fn drain(v: &mut TreeActor) {
    while v.progress().unwrap() {}
}

/// The blob at `path` in a commit.
fn at_path(repo: &TreeGit, head: git2::Oid, path: &str) -> Vec<u8> {
    let mut oid = repo.read_commit(head).unwrap().tree;
    for part in path.split('/') {
        oid = repo
            .list_tree(oid)
            .unwrap()
            .into_iter()
            .find(|e| e.name == part.as_bytes())
            .unwrap()
            .oid;
    }
    repo.read_blob(oid).unwrap()
}

/// The bytes of each live line of `path` as the commit's own line map divides
/// its file: each live run's lines end at a newline, its last at `open` bytes
/// when it has none (docs/trees.md, "What a commit holds").
fn live_lines(repo: &TreeGit, head: git2::Oid, path: &str) -> Vec<Vec<u8>> {
    let map: serde_json::Value =
        serde_json::from_slice(&at_path(repo, head, &format!("lines/{}", fanned(path)))).unwrap();
    let bytes = at_path(repo, head, &format!("files/{}", fanned(path)));
    let mut rest = &bytes[..];
    let mut lines = Vec::new();
    for run in map[1]["lines"].as_array().unwrap() {
        if run["dead"].as_bool() == Some(true) {
            continue;
        }
        let count = run["count"].as_u64().unwrap();
        for i in 0..count {
            let len = match run["open"].as_u64() {
                Some(open) if i + 1 == count => open as usize,
                _ => rest.iter().position(|b| *b == b'\n').unwrap() + 1,
            };
            lines.push(rest[..len].to_vec());
            rest = &rest[len..];
        }
    }
    assert!(rest.is_empty(), "the map holds every byte");
    lines
}

fn lost(repo: &TreeGit, save: git2::Oid) -> bool {
    repo.list_refs("refs/collisions/root/")
        .unwrap()
        .iter()
        .any(|(_, oid)| *oid == save)
}

/// Finding 32's exact calls (R10.8): the first save loses to the root's edit of a
/// line it deleted, the second to the line the root keeps after it. Nothing of
/// either reaches the root, both stay in history, and no line joins another.
#[test]
fn a_save_with_an_unfinished_line_before_a_kept_line_loses_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "c", BASE);
    drain(&mut v);
    let fork = v.fork().unwrap();
    put(&mut v, fork, "c", b"y");
    let first = v.head(fork).unwrap();
    let edit = v.write_open(fork, "c", &writer("editor"), false).unwrap();
    put(&mut v, ROOT, "c", PARENT);
    v.write(edit, 0, b"n6", false).unwrap();
    v.close(edit).unwrap();
    let second = v.head(fork).unwrap();
    assert_ne!(first, second);
    assert_eq!(v.read(fork, "c").unwrap(), b"n6");
    drain(&mut v);

    assert_eq!(v.read(ROOT, "c").unwrap(), PARENT);
    assert_eq!(v.read(fork, "c").unwrap(), PARENT);
    let repo = TreeGit::open(tmp.path().join("history.git")).unwrap();
    assert!(lost(&repo, first), "the first save keeps its collision ref");
    assert!(
        lost(&repo, second),
        "the second save keeps its collision ref"
    );
    let head = v.head(ROOT).unwrap();
    assert!(repo.is_ancestor(first, head).unwrap());
    assert!(repo.is_ancestor(second, head).unwrap());
    let live = live_lines(&repo, head, "c");
    assert!(live[..live.len() - 1].iter().all(|l| l.ends_with(b"\n")));
    assert_eq!(live.concat(), PARENT);
}

/// R10.8's guard: an unfinished line past the parent's lines has nothing after
/// it, so it lands with no collision and joins nothing.
#[test]
fn an_unfinished_save_past_the_parents_lines_lands() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"base\n");
    drain(&mut v);
    let fork = v.fork().unwrap();
    put(&mut v, fork, "a", b"base\nchild");
    put(&mut v, ROOT, "a", b"base\nparent\n");
    drain(&mut v);
    assert_eq!(v.read(ROOT, "a").unwrap(), b"base\nparent\nchild");
    assert_eq!(v.read(fork, "a").unwrap(), b"base\nparent\nchild");
    let repo = TreeGit::open(tmp.path().join("history.git")).unwrap();
    assert!(repo.list_refs("refs/collisions/root/").unwrap().is_empty());
}

/// R10.6: while the root's queue holds the sandbox's line back, the sandbox shows
/// that line alone, not the root's unfinished line in front of it; then the root's.
#[test]
fn a_sync_hides_the_parents_unfinished_line_until_decided() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"base\n");
    drain(&mut v);
    let fork = v.fork().unwrap();
    let held = v.write_open(ROOT, "a", &writer("holder"), false).unwrap();
    put(&mut v, fork, "a", b"base\nchild\n");
    put(&mut v, ROOT, "a", b"base\n}");
    drain(&mut v);
    assert_eq!(v.read(fork, "a").unwrap(), b"base\nchild\n");
    v.close(held).unwrap();
    drain(&mut v);
    assert_eq!(v.read(ROOT, "a").unwrap(), b"base\n}");
    assert_eq!(v.read(fork, "a").unwrap(), b"base\n}");
}

/// R10.9 (finding 33): while the root's queue holds back the sandbox's unfinished
/// line, the sandbox shows it and hides the root's later line, so `y` then `m7\n`
/// never reads `ym7\n`. Nothing is lost: the save stays in history, the sandbox
/// then shows the root's version.
#[test]
fn a_sync_hides_the_parents_later_lines_behind_an_undecided_unfinished_line() {
    let tmp = tempfile::tempdir().unwrap();
    let mut v = new_actor(tmp.path());
    put(&mut v, ROOT, "a", b"a\nb\n");
    drain(&mut v);
    let fork = v.fork().unwrap();
    let held = v.write_open(ROOT, "a", &writer("holder"), false).unwrap();
    put(&mut v, fork, "a", b"y");
    let save = v.head(fork).unwrap();
    put(&mut v, ROOT, "a", b"a\nb\nm7\n");
    drain(&mut v);
    assert_eq!(v.read(fork, "a").unwrap(), b"y");
    v.close(held).unwrap();
    drain(&mut v);
    assert_eq!(v.read(ROOT, "a").unwrap(), b"a\nb\nm7\n");
    assert_eq!(v.read(fork, "a").unwrap(), b"a\nb\nm7\n");
    let repo = TreeGit::open(tmp.path().join("history.git")).unwrap();
    assert!(lost(&repo, save));
    assert!(repo.is_ancestor(save, v.head(ROOT).unwrap()).unwrap());
}
