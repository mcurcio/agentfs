//! Queues a one-commit patch in a volume's own history repository, as a
//! producer pushing to the state area's git directory would (design §12).
//!
//! Shared (through `#[path]`) by `crates/agentfs/tests`.

#![allow(dead_code)]

use std::path::Path;

/// A patch on the head of `refs/heads/agentfs/<volume>` in the bare
/// repository `git`, setting `path` to `content`, queued as inbox `n`.
/// Returns the patch commit.
pub fn queue_patch(git: &Path, volume: &str, n: u64, path: &str, content: &str) -> git2::Oid {
    let repo = git2::Repository::open_bare(git).unwrap();
    let parent = repo
        .find_reference(&format!("refs/heads/agentfs/{volume}"))
        .unwrap()
        .peel_to_commit()
        .unwrap();
    let mut index = git2::Index::new().unwrap();
    index.read_tree(&parent.tree().unwrap()).unwrap();
    let blob = repo.blob(content.as_bytes()).unwrap();
    index
        .add(&git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            file_size: 0,
            id: blob,
            flags: 0,
            flags_extended: 0,
            path: path.as_bytes().to_vec(),
        })
        .unwrap();
    let tree = repo.find_tree(index.write_tree_to(&repo).unwrap()).unwrap();
    let sig = git2::Signature::now("Ada", "ada@example.com").unwrap();
    let oid = repo
        .commit(None, &sig, &sig, &format!("patch {n}"), &tree, &[&parent])
        .unwrap();
    repo.reference(
        &format!("refs/agentfs/{volume}/inbox/{n:010}"),
        oid,
        true,
        "test",
    )
    .unwrap();
    oid
}
