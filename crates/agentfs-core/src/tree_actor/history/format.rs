//! A tree commit's layout, its one home (git-model §4; docs/trees.md, "What a
//! commit holds"). Each file has three entries in hashed sub-folders, so a
//! commit that changes one file rewrites only the folders on its paths:
//!
//! ```text
//! files/<aa>/<bb>/<name>   the file's bytes
//! lines/<aa>/<bb>/<name>   JSON [identity, line map]
//! names/<cc>/<dd>/<id>     JSON name history, one per file identity, ended ones included
//! ```
//!
//! `aa` and `bb` are hex digits 1-2 and 3-4 of the file name's Git blob id
//! (`git hash-object --stdin`), `cc` and `dd` the same of the identity in
//! decimal: two fixed levels of 256, so a file's path never changes when
//! siblings come or go. Object numbers are not in history: they are the
//! journal's. The builder writes a commit as edits of its parent's tree;
//! snapshots and checkouts read it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use git2::Oid;

use super::Objects;
use super::feed::Edit;
use super::message;
use crate::tree_actor::queue::Key;
use crate::tree_actor::stages::{Timer, Which};
use crate::tree_actor::tree::{FileBytes, Lines, MergeFileIdentity, NameHistory, Tree};
use crate::tree_actor::{Error, Result, referee};
use crate::tree_git::{TreeEntry, TreeGit};

const FILES: &str = "files";
const LINES: &str = "lines";
const NAMES: &str = "names";

/// The two fan-out folders of `name`: hex digits 1-2 and 3-4 of its bytes'
/// Git blob id. The one hash of the layout.
pub(in crate::tree_actor) fn fanout(name: &str) -> (String, String) {
    let hex = Oid::hash_object(git2::ObjectType::Blob, name.as_bytes())
        .unwrap_or(Oid::ZERO_SHA1)
        .to_string();
    (hex[..2].to_owned(), hex[2..4].to_owned())
}

fn hashed(top: &str, name: &str) -> String {
    let (aa, bb) = fanout(name);
    format!("{top}/{aa}/{bb}/{name}")
}

/// A file's bytes in a commit: `files/<aa>/<bb>/<name>`.
pub(in crate::tree_actor) fn file_path(name: &str) -> String {
    hashed(FILES, name)
}

/// A file's line map in a commit: `lines/<aa>/<bb>/<name>`.
pub(in crate::tree_actor) fn lines_path(name: &str) -> String {
    hashed(LINES, name)
}

/// A file identity's name history in a commit: `names/<cc>/<dd>/<id>`.
pub(in crate::tree_actor) fn names_path(identity: MergeFileIdentity) -> String {
    hashed(NAMES, &identity.0.to_string())
}

/// Where a file's bytes are for a commit: a blob already stored, or one this
/// batch fed.
#[derive(Clone, Copy)]
pub(in crate::tree_actor) enum BlobRef {
    Stored(Oid),
    Mark(u64),
}

/// The edits that take a commit of the tree before a step to one of `after`:
/// for each path in `changed`, its file and line map written or deleted, and
/// for each identity, its name history. With no `changed` (the parent's tree
/// is not known to match) the whole tree.
pub(in crate::tree_actor) fn edits(
    after: &Tree,
    changed: Option<&BTreeSet<Key>>,
    mut blob: impl FnMut(&Arc<FileBytes>) -> Result<BlobRef>,
) -> Result<Vec<Edit>> {
    let _timer = Timer::start(Which::Objects);
    let mut edits = Vec::new();
    let mut file = |edits: &mut Vec<Edit>, name: &str, object: u64| -> Result<()> {
        let bytes = after.bytes.get(&object).ok_or(Error::Format)?;
        let id = after.identities.get(&object).ok_or(Error::Format)?;
        let lines = after.lines.get(&object).ok_or(Error::Format)?;
        let path = file_path(name);
        edits.push(match blob(bytes)? {
            BlobRef::Stored(oid) => Edit::Stored { path, oid },
            BlobRef::Mark(mark) => Edit::Blob { path, mark },
        });
        edits.push(Edit::Inline {
            path: lines_path(name),
            bytes: serde_json::to_vec(&(id, &**lines))?,
        });
        Ok(())
    };
    let names = |edits: &mut Vec<Edit>, id: MergeFileIdentity, history: &NameHistory| {
        edits.push(Edit::Inline {
            path: names_path(id),
            bytes: serde_json::to_vec(history)?,
        });
        Ok::<_, Error>(())
    };
    let Some(changed) = changed else {
        edits.push(Edit::DeleteAll);
        for (name, object) in after.names.iter() {
            file(&mut edits, name, *object)?;
        }
        for (id, history) in &after.namespace {
            names(&mut edits, *id, history)?;
        }
        return Ok(edits);
    };
    for key in changed {
        match key {
            Key::Name(name) => match after.names.get(name) {
                Some(object) => file(&mut edits, name, *object)?,
                None => {
                    edits.push(Edit::Delete {
                        path: file_path(name),
                    });
                    edits.push(Edit::Delete {
                        path: lines_path(name),
                    });
                }
            },
            Key::File(id) => match after.namespace.get(id) {
                Some(history) => names(&mut edits, *id, history)?,
                None => edits.push(Edit::Delete {
                    path: names_path(*id),
                }),
            },
        }
    }
    Ok(edits)
}

/// The entry `name` of a listing.
fn entry(entries: &[TreeEntry], name: &str) -> Option<Oid> {
    entries
        .iter()
        .find(|e| e.name == name.as_bytes())
        .map(|e| e.oid)
}

/// Every leaf under a top folder's two fan-out levels: its name and blob. A
/// leaf in a folder its name does not hash to cannot come from the builder:
/// [`Error::Format`].
fn leaves(repo: &TreeGit, top: Option<Oid>) -> Result<Vec<(String, Oid)>> {
    let mut leaves = Vec::new();
    let Some(top) = top else {
        return Ok(leaves);
    };
    for aa in repo.list_tree(top)? {
        for bb in repo.list_tree(aa.oid)? {
            for leaf in repo.list_tree(bb.oid)? {
                let name = String::from_utf8(leaf.name).map_err(|_| Error::Format)?;
                let (a, b) = fanout(&name);
                if a.as_bytes() != aa.name || b.as_bytes() != bb.name {
                    return Err(Error::Format);
                }
                leaves.push((name, leaf.oid));
            }
        }
    }
    Ok(leaves)
}

/// The snapshot a commit holds; the Root commit's is empty. Each file's object
/// is the one `objects` gives its identity; one `objects` lacks keeps its
/// identity's number, the object its create made (a commit of another tree).
pub(in crate::tree_actor) fn read_snapshot(
    repo: &TreeGit,
    commit: Oid,
    objects: &Objects,
) -> Result<referee::Snapshot> {
    let _timer = Timer::start(Which::SnapshotRead);
    let commit = repo.read_commit(commit)?;
    let entries = repo.list_tree(commit.tree)?;
    if !message::is_envelope(&commit.message) {
        // The Root commit alone has no envelope, and an empty tree.
        if entries.is_empty() {
            return Ok(referee::Snapshot::default());
        }
        return Err(Error::Format);
    }
    let mut names = BTreeMap::new();
    for (id, oid) in leaves(repo, entry(&entries, NAMES))? {
        let id = MergeFileIdentity(id.parse().map_err(|_| Error::Format)?);
        let history: NameHistory = serde_json::from_slice(&repo.read_blob(oid)?)?;
        names.insert(id, history);
    }
    let mut files = BTreeMap::new();
    for (name, oid) in leaves(repo, entry(&entries, LINES))? {
        let (id, lines): (MergeFileIdentity, Lines) =
            serde_json::from_slice(&repo.read_blob(oid)?)?;
        // A file's line map is at the path its name history's head names.
        let history = names.get(&id).ok_or(Error::Format)?;
        if history.path()? != Some(name.as_str()) {
            return Err(Error::Format);
        }
        let object = objects.get(&id.0).copied().unwrap_or(id.0);
        if files.insert(id, referee::File { object, lines }).is_some() {
            return Err(Error::Format);
        }
    }
    Ok(referee::Snapshot { files, names })
}

/// The object at `path` (slash-separated) under tree `tree`, if any.
fn lookup(repo: &TreeGit, tree: Oid, path: &str) -> Result<Option<Oid>> {
    let mut at = tree;
    for part in path.split('/') {
        match entry(&repo.list_tree(at)?, part) {
            Some(oid) => at = oid,
            None => return Ok(None),
        }
    }
    Ok(Some(at))
}

/// One live file of a commit, by identity: its line map and its blob; none
/// where the commit has no live file of that identity. Three paths read,
/// never the whole snapshot.
pub(in crate::tree_actor) fn file_of(
    repo: &TreeGit,
    commit: Oid,
    identity: MergeFileIdentity,
) -> Result<Option<(Lines, Oid)>> {
    let commit = repo.read_commit(commit)?;
    if !message::is_envelope(&commit.message) {
        return Ok(None);
    }
    let Some(names) = lookup(repo, commit.tree, &names_path(identity))? else {
        return Ok(None);
    };
    let history: NameHistory = serde_json::from_slice(&repo.read_blob(names)?)?;
    let Some(path) = history.path()? else {
        return Ok(None);
    };
    let (Some(lines), Some(blob)) = (
        lookup(repo, commit.tree, &lines_path(path))?,
        lookup(repo, commit.tree, &file_path(path))?,
    ) else {
        return Err(Error::Format);
    };
    let (id, lines): (MergeFileIdentity, Lines) = serde_json::from_slice(&repo.read_blob(lines)?)?;
    if id != identity {
        return Err(Error::Format);
    }
    Ok(Some((lines, blob)))
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
    Ok(entry(&repo.list_tree(commit.tree)?, FILES).unwrap_or_else(empty_tree))
}

/// The empty tree's id; the Root commit's tree, so always stored.
pub(in crate::tree_actor) fn empty_tree() -> Oid {
    Oid::hash_object(git2::ObjectType::Tree, b"").unwrap_or(Oid::ZERO_SHA1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_path_is_the_layouts_literal_text() {
        // `printf a | git hash-object --stdin` is 2e65efe2a145dda7ee51d1741299f848e5bf752e.
        assert_eq!(fanout("a"), ("2e".to_owned(), "65".to_owned()));
        assert_eq!(file_path("a"), "files/2e/65/a");
        assert_eq!(lines_path("a"), "lines/2e/65/a");
        // `printf 7 | git hash-object --stdin` is c7930257dfef505fd996e1d6f22f2f35149990d0.
        assert_eq!(names_path(MergeFileIdentity(7)), "names/c7/93/7");
    }
}
