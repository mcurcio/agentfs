//! A tree commit's layout, its one home (git-model §4): `files/<name>` for
//! each file, and `metadata.json` (each path's object, identity and line map)
//! and `namespace.json` (every name history) beside them. The builder writes
//! it as edits of the parent's tree; snapshots and checkouts read it.

use std::collections::BTreeMap;
use std::sync::Arc;

use git2::Oid;

use super::feed::Edit;
use super::message;
use crate::tree_actor::stages::{Timer, Which};
use crate::tree_actor::tree::{FileBytes, Lines, MergeFileIdentity, Tree};
use crate::tree_actor::{Error, Result, referee};
use crate::tree_git::TreeGit;

const FILES: &str = "files";
const METADATA: &str = "metadata.json";
const NAMESPACE: &str = "namespace.json";

/// A file's path in a commit.
pub(in crate::tree_actor) fn file_path(name: &str) -> String {
    format!("{FILES}/{name}")
}

/// Where a file's bytes are for a commit: a blob already stored, or one this
/// batch fed.
#[derive(Clone, Copy)]
pub(in crate::tree_actor) enum BlobRef {
    Stored(Oid),
    Mark(u64),
}

/// The edits that take `before`'s commit to `after`'s: each file whose name
/// or bytes changed, each name gone, and the two metadata files whole. With no
/// `before` (its commit's tree is not known to match it) the whole tree.
pub(in crate::tree_actor) fn edits(
    before: Option<&Tree>,
    after: &Tree,
    mut blob: impl FnMut(&Arc<FileBytes>) -> Result<BlobRef>,
) -> Result<Vec<Edit>> {
    let _timer = Timer::start(Which::Objects);
    let mut edits = Vec::new();
    if before.is_none() {
        edits.push(Edit::DeleteAll);
    }
    for (name, object) in after.names.iter() {
        let bytes = after.bytes.get(object).ok_or(Error::Format)?;
        let unchanged = before.is_some_and(|before| {
            before.names.get(name) == Some(object)
                && before
                    .bytes
                    .get(object)
                    .is_some_and(|old| Arc::ptr_eq(old, bytes))
        });
        if unchanged {
            continue;
        }
        let path = file_path(name);
        edits.push(match blob(bytes)? {
            BlobRef::Stored(oid) => Edit::Stored { path, oid },
            BlobRef::Mark(mark) => Edit::Blob { path, mark },
        });
    }
    if let Some(before) = before {
        for name in before.names.keys() {
            if !after.names.contains_key(name) {
                edits.push(Edit::Delete {
                    path: file_path(name),
                });
            }
        }
    }
    let mut metadata = BTreeMap::new();
    for (name, object) in after.names.iter() {
        let id = after.identities.get(object).ok_or(Error::Format)?;
        let lines = after.lines.get(object).ok_or(Error::Format)?;
        metadata.insert(name, (object, id, &**lines));
    }
    edits.push(Edit::Inline {
        path: METADATA.to_owned(),
        bytes: serde_json::to_vec(&metadata)?,
    });
    edits.push(Edit::Inline {
        path: NAMESPACE.to_owned(),
        bytes: serde_json::to_vec(&after.namespace)?,
    });
    Ok(edits)
}

/// The snapshot a commit holds; the Root commit's is empty. Objects are the
/// ones its metadata names (an object table may renumber them).
pub(in crate::tree_actor) fn read_snapshot(
    repo: &TreeGit,
    commit: Oid,
) -> Result<referee::Snapshot> {
    let _timer = Timer::start(Which::SnapshotRead);
    let commit = repo.read_commit(commit)?;
    if !message::is_envelope(&commit.message) {
        // The Root commit alone has no envelope, and an empty tree.
        if repo.list_tree(commit.tree)?.is_empty() {
            return Ok(referee::Snapshot::default());
        }
        return Err(Error::Format);
    }
    let entries = repo.list_tree(commit.tree)?;
    let find = |name: &str| {
        entries
            .iter()
            .find(|e| e.name == name.as_bytes())
            .map(|e| e.oid)
            .ok_or(Error::Format)
    };
    let by_path: BTreeMap<String, (u64, MergeFileIdentity, Lines)> =
        serde_json::from_slice(&repo.read_blob(find(METADATA)?)?)?;
    let names = serde_json::from_slice(&repo.read_blob(find(NAMESPACE)?)?)?;
    let files = by_path
        .into_values()
        .map(|(object, id, lines)| (id, referee::File { object, lines }))
        .collect();
    Ok(referee::Snapshot { files, names })
}

/// The files tree a commit's checkout holds: `files/`, or the Root commit's
/// own (empty) tree.
pub(in crate::tree_actor) fn files_tree(repo: &TreeGit, commit: Oid) -> Result<Oid> {
    let commit = repo.read_commit(commit)?;
    // The trailer tells the Root commit even if a workload used our names.
    if !message::is_envelope(&commit.message) {
        return Ok(commit.tree);
    }
    // Git keeps no empty tree: a commit with no file has no `files/`.
    Ok(repo
        .list_tree(commit.tree)?
        .into_iter()
        .find(|e| e.name == FILES.as_bytes())
        .map_or_else(empty_tree, |e| e.oid))
}

/// The empty tree's id; the Root commit's tree, so always stored.
pub(in crate::tree_actor) fn empty_tree() -> Oid {
    Oid::hash_object(git2::ObjectType::Tree, b"").unwrap_or(Oid::ZERO_SHA1)
}
