//! Tree history: the builder that makes commits behind the call path, History that owns the
//! store, snapshots read from commits, and checkouts (target git-model §4, §5, §12;
//! docs/trees.md, "History trails the files").

pub(super) mod builder;
pub(super) mod built;
pub(super) mod built_log;
pub(super) mod feed;
pub(super) mod format;
pub(super) mod lag;
pub(super) mod message;
pub(super) mod refs;
pub(super) mod take;
pub(super) mod upstreams;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use git2::Oid;

use super::pointers::Pointers;
use super::stages::{Timer, Which};
use super::tree::{EventNumber, FileBytes, Shape, Tree};
use super::{Error, Result, TreeId, referee};
use crate::file_lock::FileLock;
use crate::tree_git::TreeGit;

/// One tree's object numbers: each named file's object, by its identity's
/// number. The journal's, never history's (git-model §3): a checkpoint holds
/// them as of its built point, and the steps after it move them.
pub(in crate::tree_actor) type Objects = BTreeMap<u64, u64>;

/// The object each named file of `shape` is on, by identity.
pub(in crate::tree_actor) fn objects_of(shape: &Shape) -> Result<Objects> {
    shape
        .names
        .values()
        .map(|object| {
            Ok((
                shape.identities.get(object).ok_or(Error::Format)?.0,
                *object,
            ))
        })
        .collect()
}

// What is lost of any pointer's commit, tree and blobs refuses to serve. The
// second pass, on this failure path only, names the pointer.
pub(in crate::tree_actor) fn check_objects(repo: &TreeGit, pointers: &Pointers) -> Result<()> {
    let missing = repo.missing_objects(&pointers.tips())?;
    let Some(first) = missing.first() else {
        return Ok(());
    };
    for (name, tip) in pointers.under(refs::ALL) {
        if let Some(oid) = repo.missing_objects(&[tip])?.first() {
            return Err(Error::MissingObject { name, oid: *oid });
        }
    }
    Err(Error::MissingObject {
        name: refs::ALL.to_owned(),
        oid: *first,
    })
}

// The exclusive actor lock is held first; no other writer can own these.
pub(in crate::tree_actor) fn remove_stale_locks(gitdir: &Path) -> io::Result<()> {
    fn refs(dir: &Path) -> io::Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                refs(&entry.path())?;
            } else if entry.file_type()?.is_file()
                && entry.path().extension().is_some_and(|s| s == "lock")
            {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }
    refs(&gitdir.join("refs"))?;
    for name in ["HEAD.lock", "config.lock", "packed-refs.lock", "index.lock"] {
        let path = gitdir.join(name);
        if path.is_file() {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

/// Creates a volume's Git store: the one place that does (clippy.toml).
pub(in crate::tree_actor) fn init_store(git_dir: &Path, lease: Arc<FileLock>) -> Result<TreeGit> {
    #[expect(clippy::disallowed_methods, reason = "History's store is made here")]
    Ok(TreeGit::init_with_lease(git_dir, lease)?)
}

/// Opens a volume's Git store for History: the one place that does.
pub(in crate::tree_actor) fn open_store(git_dir: &Path, lease: Arc<FileLock>) -> Result<TreeGit> {
    #[expect(clippy::disallowed_methods, reason = "History's store is opened here")]
    Ok(TreeGit::open_with_lease(git_dir, lease)?)
}

/// `tree`'s snapshot as built: its head's, each file on the object
/// `objects` gives it. A named file `objects` lacks is [`Error::Format`].
pub(in crate::tree_actor) fn tree_snapshot(
    repo: &TreeGit,
    pointers: &Pointers,
    objects: &Objects,
    tree: TreeId,
) -> Result<referee::Snapshot> {
    let head = pointers.get(&refs::branch(tree)).ok_or(Error::NotFound)?;
    let snapshot = format::read_snapshot(repo, head, objects)?;
    if snapshot.files.keys().any(|id| !objects.contains_key(&id.0)) {
        return Err(Error::Format);
    }
    Ok(snapshot)
}

/// Every tree as built: each branch's snapshot installed, each file's map
/// from `map` (given its tree, object and path).
pub(in crate::tree_actor) fn read_trees(
    repo: &TreeGit,
    pointers: &Pointers,
    objects: &BTreeMap<TreeId, Objects>,
    mut map: impl FnMut(TreeId, u64, &str) -> Result<Arc<FileBytes>>,
) -> Result<BTreeMap<TreeId, Tree>> {
    let mut trees = BTreeMap::new();
    let none = Objects::new();
    for id in refs::branches(pointers)?.into_keys() {
        let mut tree = Tree::default();
        let objects = objects.get(&id).unwrap_or(&none);
        tree.install(
            tree_snapshot(repo, pointers, objects, id)?,
            |_, object, path| map(id, object, path),
        )?;
        trees.insert(id, tree);
    }
    if !trees.contains_key(&TreeId::Root) {
        return Err(Error::Format);
    }
    Ok(trees)
}

/// The commit of every direct save the pointers reach, by its event number:
/// one Git child.
pub(in crate::tree_actor) fn owners(
    repo: &TreeGit,
    pointers: &Pointers,
) -> Result<BTreeMap<EventNumber, Oid>> {
    let _timer = Timer::start(Which::EventOwners);
    Ok(repo
        .commit_messages(&refs::owner_tips(pointers))?
        .into_iter()
        .filter_map(|(oid, message)| Some((message::direct_event(&message)?, oid)))
        .collect())
}

/// Where `tree`'s checkout holds the file named `name`: its `files/` path
/// under `trees/<tree>/` (`format::file_path`).
pub(in crate::tree_actor) fn checkout_path(root: &Path, tree: TreeId, name: &str) -> PathBuf {
    let (aa, bb) = format::fanout(name);
    root.join("trees")
        .join(tree.to_string())
        .join(aa)
        .join(bb)
        .join(name)
}

/// Makes `tree`'s checkout `head`'s files, rewriting only what differs from
/// `previous`, the files tree the checkout holds now. Returns the files tree
/// it holds after.
pub(in crate::tree_actor) fn checkout(
    repo: &TreeGit,
    root: &Path,
    tree: TreeId,
    head: Oid,
    previous: Option<Oid>,
) -> Result<Oid> {
    let _timer = Timer::start(Which::Checkout);
    let dir = root.join("trees").join(tree.to_string());
    fs::create_dir_all(&dir)?;
    let files = format::files_tree(repo, head)?;
    repo.checkout_tree(files, &dir, previous)?;
    Ok(files)
}

/// The marker blob of a sync queue entry, written at create.
pub(in crate::tree_actor) fn sync_marker() -> Oid {
    Oid::hash_object(git2::ObjectType::Blob, b"sync").unwrap_or(Oid::ZERO_SHA1)
}

/// What a cut-short build left in the store, which nothing names: fast-import's
/// temporary packs, `.keep` files, an index without its pack.
pub(in crate::tree_actor) fn clean_packs(gitdir: &Path) -> io::Result<()> {
    let dir = gitdir.join("objects").join("pack");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(());
    };
    for entry in entries {
        let path = entry?.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let orphan_index =
            path.extension().is_some_and(|e| e == "idx") && !path.with_extension("pack").exists();
        if name.starts_with("tmp_") || path.extension().is_some_and(|e| e == "keep") || orphan_index
        {
            fs::remove_file(&path)?;
        }
    }
    File::open(&dir)?.sync_all()
}
