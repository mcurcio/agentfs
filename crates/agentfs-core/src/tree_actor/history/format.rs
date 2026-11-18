//! A tree commit's layout, its one home (git-model §4; docs/trees.md, "What a
//! commit holds"). Each file has three entries in hashed sub-folders, and each
//! directory one, so a commit that changes one file rewrites only the folders
//! on its paths:
//!
//! ```text
//! files/<aa>/<bb>/<path>      the file's bytes (a link's: its target), in git's mode
//! lines/<aa>/<bb>/<path>      JSON [identity, line map, meta]
//! names/<cc>/<dd>/<id>        JSON name history, one per file identity, ended ones included
//! dirs/<aa>/<bb>/<enc(path)>  JSON meta of one directory
//! dirs/root                   JSON meta of the root directory
//! ```
//!
//! `aa` and `bb` are hex digits 1-2 and 3-4 of the path's Git blob id
//! (`git hash-object --stdin`), `cc` and `dd` the same of the identity in
//! decimal: two fixed levels of 256, so an entry's place never changes when
//! siblings come or go. A file's path nests under them (a file and a
//! directory never share a path); a directory's record is one leaf, its path
//! with `%` as `%25` and `/` as `%2F`. Object and directory numbers and times
//! are not in history: they are the journal's. The builder writes a commit as
//! edits of its parent's tree; snapshots and checkouts read it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use git2::Oid;

use super::Objects;
use super::feed::Edit;
use super::message;
use crate::tree_actor::meta::{Kind, Meta};
use crate::tree_actor::queue::Key;
use crate::tree_actor::stages::{Timer, Which};
use crate::tree_actor::tree::{FileBytes, Lines, MergeFileIdentity, NameHistory, Tree};
use crate::tree_actor::{Error, Result, referee};
use crate::tree_git::{TreeEntry, TreeGit};

/// An event commit's one blob: its chain's row, as the store wrote it.
pub(in crate::tree_actor) const ROW_FILE: &str = "row.json";
const FILES: &str = "files";
const LINES: &str = "lines";
const NAMES: &str = "names";
const DIRS: &str = "dirs";
/// The root directory's record, beside the fan-out folders of `dirs/`.
const ROOT_DIR: &str = "root";
/// A tree entry's mode for a folder.
const FOLDER: u32 = 0o040000;

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

/// A directory path as one leaf's name: `%` as `%25`, then `/` as `%2F`.
fn encode_dir(path: &str) -> String {
    path.replace('%', "%25").replace('/', "%2F")
}

/// A leaf's path as the tree names it.
#[allow(clippy::unnecessary_wraps, reason = "one of `leaves`' decoders")]
fn as_is(name: &str) -> Result<String> {
    Ok(name.to_owned())
}

/// [`encode_dir`] undone; [`Error::Format`] for a name it cannot give.
fn decode_dir(name: &str) -> Result<String> {
    let path = name.replace("%2F", "/").replace("%25", "%");
    if encode_dir(&path) == name {
        Ok(path)
    } else {
        Err(Error::Format)
    }
}

/// A directory's record in a commit: `dirs/<aa>/<bb>/<enc(path)>`, the
/// root's (`""`) `dirs/root`.
pub(in crate::tree_actor) fn dir_path(path: &str) -> String {
    if path.is_empty() {
        return format!("{DIRS}/{ROOT_DIR}");
    }
    let (aa, bb) = fanout(path);
    format!("{DIRS}/{aa}/{bb}/{}", encode_dir(path))
}

/// The Root commit's tree: the root directory's record alone.
pub(in crate::tree_actor) fn root_tree(repo: &TreeGit, root: &Meta) -> Result<Oid> {
    let record = repo.write_blob(&serde_json::to_vec(root)?)?;
    let dirs = repo.write_tree(&[TreeEntry {
        name: ROOT_DIR.as_bytes().to_vec(),
        mode: 0o100644,
        oid: record,
    }])?;
    Ok(repo.write_tree(&[TreeEntry {
        name: DIRS.as_bytes().to_vec(),
        mode: FOLDER,
        oid: dirs,
    }])?)
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
        let meta = after.meta.get(&object).ok_or(Error::Format)?;
        let path = file_path(name);
        let mode = meta.git_mode();
        edits.push(match blob(bytes)? {
            BlobRef::Stored(oid) => Edit::Stored { path, oid, mode },
            BlobRef::Mark(mark) => Edit::Blob { path, mark, mode },
        });
        edits.push(Edit::Inline {
            path: lines_path(name),
            bytes: serde_json::to_vec(&(id, &**lines, meta))?,
        });
        Ok(())
    };
    let dir = |edits: &mut Vec<Edit>, path: &str, meta: &Meta| -> Result<()> {
        edits.push(Edit::Inline {
            path: dir_path(path),
            bytes: serde_json::to_vec(meta)?,
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
        for (path, state) in after.dirs.iter() {
            dir(&mut edits, path, &state.meta)?;
        }
        for (id, history) in &after.namespace {
            names(&mut edits, *id, history)?;
        }
        return Ok(edits);
    };
    for key in changed {
        match key {
            Key::Name(name) => {
                match after.names.get(name) {
                    Some(object) => file(&mut edits, name, *object)?,
                    None if !name.is_empty() => {
                        edits.push(Edit::Delete {
                            path: file_path(name),
                        });
                        edits.push(Edit::Delete {
                            path: lines_path(name),
                        });
                    }
                    None => {}
                }
                match after.dirs.get(name) {
                    Some(state) => dir(&mut edits, name, &state.meta)?,
                    None => edits.push(Edit::Delete {
                        path: dir_path(name),
                    }),
                }
            }
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

/// The edits of an event commit: its tree is `row.json` alone, whatever its
/// parent's held.
pub(in crate::tree_actor) fn row_edits(row: &str) -> Vec<Edit> {
    vec![
        Edit::DeleteAll,
        Edit::Inline {
            path: ROW_FILE.to_owned(),
            bytes: row.as_bytes().to_vec(),
        },
    ]
}

/// A built event commit's row: its `row.json`.
///
/// # Errors
///
/// [`Error::Format`] for a commit with no `row.json` or one that is not UTF-8.
pub(in crate::tree_actor) fn read_row(repo: &TreeGit, commit: Oid) -> Result<String> {
    let tree = repo.read_commit(commit)?.tree;
    let blob = entry(&repo.list_tree(tree)?, ROW_FILE).ok_or(Error::Format)?;
    String::from_utf8(repo.read_blob(blob)?).map_err(|_| Error::Format)
}

/// The entry `name` of a listing.
fn entry(entries: &[TreeEntry], name: &str) -> Option<Oid> {
    entries
        .iter()
        .find(|e| e.name == name.as_bytes())
        .map(|e| e.oid)
}

/// Every leaf under a top folder's two fan-out levels, its path nesting
/// under them: its path (`decode` of what the tree names it) and blob. A
/// leaf in a folder its path does not hash to cannot come from the builder:
/// [`Error::Format`]. With `root`, the top's blob of that name is the leaf
/// `""`.
fn leaves(
    repo: &TreeGit,
    top: Option<Oid>,
    root: Option<&str>,
    decode: fn(&str) -> Result<String>,
) -> Result<Vec<(String, Oid)>> {
    fn nested(
        repo: &TreeGit,
        folder: Oid,
        prefix: &str,
        leaves: &mut Vec<(String, Oid)>,
    ) -> Result<()> {
        for entry in repo.list_tree(folder)? {
            let name = String::from_utf8(entry.name).map_err(|_| Error::Format)?;
            let path = format!("{prefix}{name}");
            if entry.mode == FOLDER {
                nested(repo, entry.oid, &format!("{path}/"), leaves)?;
            } else {
                leaves.push((path, entry.oid));
            }
        }
        Ok(())
    }
    let mut leaves = Vec::new();
    let Some(top) = top else {
        return Ok(leaves);
    };
    for aa in repo.list_tree(top)? {
        if root.is_some_and(|root| aa.name == root.as_bytes()) && aa.mode != FOLDER {
            leaves.push((String::new(), aa.oid));
            continue;
        }
        for bb in repo.list_tree(aa.oid)? {
            let mut found = Vec::new();
            nested(repo, bb.oid, "", &mut found)?;
            for (name, oid) in found {
                let path = decode(&name)?;
                let (a, b) = fanout(&path);
                if a.as_bytes() != aa.name || b.as_bytes() != bb.name {
                    return Err(Error::Format);
                }
                leaves.push((path, oid));
            }
        }
    }
    Ok(leaves)
}

/// Each directory record of a commit's top tree, by path (`""` the root).
fn read_dirs(repo: &TreeGit, entries: &[TreeEntry]) -> Result<BTreeMap<String, Meta>> {
    let mut dirs = BTreeMap::new();
    for (path, oid) in leaves(repo, entry(entries, DIRS), Some(ROOT_DIR), decode_dir)? {
        let meta: Meta = serde_json::from_slice(&repo.read_blob(oid)?)?;
        if meta.kind != Kind::Dir {
            return Err(Error::Format);
        }
        dirs.insert(path, meta);
    }
    Ok(dirs)
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
    let dirs = read_dirs(repo, &entries)?;
    if !message::is_envelope(&commit.message) {
        // The Root commit alone has no envelope, and the root directory's
        // record alone.
        if entries.iter().all(|e| e.name == DIRS.as_bytes()) && dirs.contains_key("") {
            return Ok(referee::Snapshot {
                dirs,
                ..referee::Snapshot::default()
            });
        }
        return Err(Error::Format);
    }
    let mut names = BTreeMap::new();
    for (id, oid) in leaves(repo, entry(&entries, NAMES), None, as_is)? {
        let id = MergeFileIdentity(id.parse().map_err(|_| Error::Format)?);
        let history: NameHistory = serde_json::from_slice(&repo.read_blob(oid)?)?;
        names.insert(id, history);
    }
    let mut files = BTreeMap::new();
    for (name, oid) in leaves(repo, entry(&entries, LINES), None, as_is)? {
        let (id, lines, meta): (MergeFileIdentity, Lines, Meta) =
            serde_json::from_slice(&repo.read_blob(oid)?)?;
        if meta.kind == Kind::Dir {
            return Err(Error::Format);
        }
        // A file's line map is at the path its name history's head names.
        let history = names.get(&id).ok_or(Error::Format)?;
        if history.path()? != Some(name.as_str()) {
            return Err(Error::Format);
        }
        let object = objects.get(&id.0).copied().unwrap_or(id.0);
        if files
            .insert(
                id,
                referee::File {
                    object,
                    lines,
                    meta,
                },
            )
            .is_some()
        {
            return Err(Error::Format);
        }
    }
    Ok(referee::Snapshot { files, names, dirs })
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
    let (id, lines, _): (MergeFileIdentity, Lines, Meta) =
        serde_json::from_slice(&repo.read_blob(lines)?)?;
    if id != identity {
        return Err(Error::Format);
    }
    Ok(Some((lines, blob)))
}

/// The files tree a commit's checkout holds: `files/`, or the empty tree
/// (the Root commit's, and any commit's with no file: git keeps no empty
/// tree).
pub(in crate::tree_actor) fn files_tree(repo: &TreeGit, commit: Oid) -> Result<Oid> {
    let commit = repo.read_commit(commit)?;
    Ok(entry(&repo.list_tree(commit.tree)?, FILES).unwrap_or_else(empty_tree))
}

/// The empty tree's id; written at create, so always stored.
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
        let (aa, bb) = fanout("a/b");
        assert_eq!(file_path("a/b"), format!("files/{aa}/{bb}/a/b"));
        assert_eq!(dir_path("a/b"), format!("dirs/{aa}/{bb}/a%2Fb"));
        assert_eq!(dir_path(""), "dirs/root");
    }

    #[test]
    fn a_directory_record_is_one_leaf_whatever_its_path_holds() {
        for path in ["a", "a/b", "50%/x", "%2F", "a%2Fb/c"] {
            let name = encode_dir(path);
            assert!(!name.contains('/'), "{name}");
            assert_eq!(decode_dir(&name).unwrap(), path);
        }
        assert!(decode_dir("a%2fb").is_err());
        assert!(decode_dir("a/b").is_err());
    }
}
