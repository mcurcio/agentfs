//! Tree history: the builder that makes commits behind the call path, History that owns the
//! store, snapshots read from commits, and checkouts (target git-model §4, §5, §12;
//! docs/trees.md, "History trails the files").

pub(super) mod builder;
pub(super) mod built;
pub(super) mod built_log;
pub(super) mod compactor;
pub(super) mod feed;
pub(super) mod format;
pub(super) mod lag;
pub(super) mod message;
pub(super) mod refs;
pub(super) mod repack;
pub(super) mod retired;
pub(super) mod take;
pub(super) mod upstreams;
pub(super) mod verify;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentfs_os::inherited_lock::InheritedLock;
use git2::Oid;

use super::pointers::Pointers;
use super::stages::{Timer, Which};
use super::tree::{FileBytes, Shape, Tree};
use super::{Error, Result};
use crate::tree_git::TreeGit;
use agentfs_contracts::TreeId;

/// One tree's object numbers: each named file's object, by its identity's
/// number. The journal's, never history's (git-model §3): a checkpoint holds
/// them as of its built point, and the steps after it move them. Persistent:
/// a batch changes only what its steps moved, and hands the map over in O(1).
pub(in crate::tree_actor) type Objects = imbl::OrdMap<u64, u64>;

/// The object each named file of `shape` is on, by identity.
pub(in crate::tree_actor) fn objects_of(shape: &Shape) -> Result<Objects> {
    shape
        .names
        .values()
        .map(|object| Ok((shape.identity_of(*object).ok_or(Error::Format)?.0, *object)))
        .collect()
}

/// [`objects_of`] `after`, from `objects`, those of `before`, which `after`
/// was changed from: only the objects a name or a state moved are visited,
/// so a batch costs its own steps, never every name.
pub(in crate::tree_actor) fn objects_after(
    before: &Shape,
    objects: &Objects,
    after: &Shape,
) -> Result<Objects> {
    let mut next = objects.clone();
    let moved = after.objects_changed_since(before);
    for object in &moved {
        if let Some(id) = before.identity_of(*object)
            && next.get(&id.0) == Some(object)
        {
            next.remove(&id.0);
        }
    }
    for object in &moved {
        if after.is_named(*object) {
            next.insert(after.identity_of(*object).ok_or(Error::Format)?.0, *object);
        }
    }
    Ok(next)
}

/// What one batch moved of one tree's object numbers and locals (directory
/// numbers and times, files' times): the built log's record of them, so a
/// start after a kill folds the checkpoint's objects and locals forward to
/// the built point without reading any tree (the checkpoint branch of
/// `TreeActor::start_history`). O(what the batch moved), never the tree's
/// size, but for a tree the batch made (a fork: every entry) or dropped.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(in crate::tree_actor) struct TreeMoved {
    /// The tree, as the journal writes it (`TreeId`'s text).
    pub tree: String,
    /// The batch dropped the tree (a fork's drop): every other field empty.
    pub dropped: bool,
    /// Each identity whose object the batch set: (identity, object).
    pub objects: Vec<(u64, u64)>,
    /// Each identity the batch left with no object (unnamed, dropped).
    pub unnamed: Vec<u64>,
    /// Each directory whose number or times the batch set, `None` for one
    /// it removed: path (`""` the root) and (number, times).
    pub dirs: Vec<(String, Option<(u64, super::Times)>)>,
    /// Each identity whose file times the batch set, `None` for one it
    /// dropped.
    pub files: Vec<(u64, Option<super::Times>)>,
}

impl TreeMoved {
    /// What the batch moved of `tree` from `before` (its shape and objects
    /// at the last built point; `None` for a tree the batch made) to `after`
    /// (`None` for a tree the batch dropped): imbl's diff of the persistent
    /// maps, so O(changes). `None` when nothing moved.
    ///
    /// # Errors
    ///
    /// [`Error::Format`] for an object with no identity.
    pub(in crate::tree_actor) fn between(
        tree: TreeId,
        before: Option<(&Shape, &Objects)>,
        after: Option<(&Shape, &Objects)>,
    ) -> Result<Option<Self>> {
        let mut moved = Self {
            tree: tree.to_string(),
            ..Self::default()
        };
        match (before, after) {
            (None, None) => return Ok(None),
            (Some(_), None) => moved.dropped = true,
            (None, Some((shape, objects))) => {
                moved.objects = objects.iter().map(|(id, object)| (*id, *object)).collect();
                let locals = shape.locals();
                moved.dirs = locals
                    .dirs
                    .into_iter()
                    .map(|(path, local)| (path, Some(local)))
                    .collect();
                moved.files = locals
                    .files
                    .into_iter()
                    .map(|(id, times)| (id, Some(times)))
                    .collect();
            }
            (Some((was, was_objects)), Some((shape, objects))) => {
                moved.diff(was, was_objects, shape, objects)?;
            }
        }
        let empty = Self {
            tree: moved.tree.clone(),
            ..Self::default()
        };
        Ok((moved != empty).then_some(moved))
    }

    /// What moved from `was` to `shape`, two shapes that share what neither
    /// changed: only the diffs of their persistent maps are visited.
    fn diff(
        &mut self,
        was: &Shape,
        was_objects: &Objects,
        shape: &Shape,
        objects: &Objects,
    ) -> Result<()> {
        use imbl::ordmap::DiffItem;
        for item in was_objects.diff(objects) {
            match item {
                DiffItem::Add(id, object)
                | DiffItem::Update {
                    new: (id, object), ..
                } => {
                    self.objects.push((*id, *object));
                }
                DiffItem::Remove(id, _) => self.unnamed.push(*id),
            }
        }
        for item in was.dirs.diff(&shape.dirs) {
            match item {
                DiffItem::Add(path, dir) => {
                    self.dirs
                        .push((path.to_string(), Some((dir.number, dir.times))));
                }
                DiffItem::Update {
                    old,
                    new: (path, dir),
                } => {
                    if (old.1.number, old.1.times) != (dir.number, dir.times) {
                        self.dirs
                            .push((path.to_string(), Some((dir.number, dir.times))));
                    }
                }
                DiffItem::Remove(path, _) => self.dirs.push((path.to_string(), None)),
            }
        }
        // A file's times are kept by identity: each identity an object that
        // moved had, before or after, is read again on both sides.
        let mut touched: BTreeSet<u64> = shape.objects_changed_since(was);
        for item in was.anonymous.diff(&shape.anonymous) {
            match item {
                imbl::ordset::DiffItem::Add(object) | imbl::ordset::DiffItem::Remove(object) => {
                    touched.insert(*object);
                }
            }
        }
        let mut ids: BTreeSet<u64> = touched
            .iter()
            .flat_map(|object| [was.identity_of(*object), shape.identity_of(*object)])
            .flatten()
            .map(|id| id.0)
            .collect();
        ids.extend(self.objects.iter().map(|(id, _)| *id));
        ids.extend(self.unnamed.iter().copied());
        let (was_anonymous, anonymous) = (anonymous_times(was), anonymous_times(shape));
        for id in ids {
            let old = file_times(was, was_objects, &was_anonymous, id);
            let new = file_times(shape, objects, &anonymous, id);
            if old != new {
                self.files.push((id, new));
            }
        }
        Ok(())
    }

    /// Moves `objects` and `locals` (every tree's, as a checkpoint holds
    /// them) by this batch's change of its tree: the inverse of
    /// [`TreeMoved::between`], so folding a batch's moves onto its
    /// before-state gives `objects_of` and `Shape::locals` of its
    /// after-state.
    ///
    /// # Errors
    ///
    /// [`Error::Format`] for a tree text that is no [`TreeId`], or a move of a
    /// tree `objects` lacks that the batch did not make.
    pub(in crate::tree_actor) fn apply(
        &self,
        objects: &mut BTreeMap<TreeId, Objects>,
        locals: &mut BTreeMap<TreeId, super::journal::TreeLocals>,
    ) -> Result<()> {
        let tree: TreeId = self.tree.parse().map_err(|_| Error::Format)?;
        if self.dropped {
            objects.remove(&tree);
            locals.remove(&tree);
            return Ok(());
        }
        // A tree the batch made lists every directory, its root among them.
        let made = self
            .dirs
            .iter()
            .any(|(path, dir)| path.is_empty() && dir.is_some());
        if !locals.contains_key(&tree) && !made {
            return Err(Error::Format);
        }
        let tree_objects = objects.entry(tree).or_default();
        for id in &self.unnamed {
            tree_objects.remove(id);
        }
        for (id, object) in &self.objects {
            tree_objects.insert(*id, *object);
        }
        let tree_locals = locals.entry(tree).or_default();
        for (path, dir) in &self.dirs {
            match dir {
                Some(dir) => tree_locals.dirs.insert(path.clone(), *dir),
                None => tree_locals.dirs.remove(path),
            };
        }
        for (id, times) in &self.files {
            match times {
                Some(times) => tree_locals.files.insert(*id, *times),
                None => tree_locals.files.remove(id),
            };
        }
        Ok(())
    }
}

/// Each anonymous object's times by identity, as [`Shape::locals`] keeps
/// them: a later object wins. O(anonymous objects), which are open files
/// with no name.
fn anonymous_times(shape: &Shape) -> BTreeMap<u64, super::Times> {
    shape
        .anonymous
        .iter()
        .filter_map(|object| Some((shape.identity_of(*object)?.0, *shape.times_of(*object)?)))
        .collect()
}

/// The times [`Shape::locals`] keeps for identity `id`: an anonymous object's
/// (those come after the names), else the named file's `objects` gives it,
/// the last by path as there.
fn file_times(
    shape: &Shape,
    objects: &Objects,
    anonymous: &BTreeMap<u64, super::Times>,
    id: u64,
) -> Option<super::Times> {
    anonymous
        .get(&id)
        .copied()
        .or_else(|| shape.times_of(*objects.get(&id)?).copied())
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
    for name in [
        "HEAD.lock",
        "config.lock",
        "packed-refs.lock",
        "index.lock",
        "objects/pack/multi-pack-index.lock",
    ] {
        let path = gitdir.join(name);
        if path.is_file() {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

/// Creates a volume's Git store: the one place that does (clippy.toml).
pub(in crate::tree_actor) fn init_store(
    git_dir: &Path,
    lease: Arc<InheritedLock>,
) -> Result<TreeGit> {
    #[expect(clippy::disallowed_methods, reason = "History's store is made here")]
    Ok(TreeGit::init_with_lease(git_dir, lease)?)
}

/// Opens a volume's Git store for History: the one place that does.
pub(in crate::tree_actor) fn open_store(
    git_dir: &Path,
    lease: Arc<InheritedLock>,
) -> Result<TreeGit> {
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
) -> Result<agentfs_referee::Snapshot> {
    let head = pointers.get(&refs::branch(tree)).ok_or(Error::NotFound)?;
    let snapshot = format::read_snapshot(repo, head, objects)?;
    if snapshot.files.keys().any(|id| !objects.contains_key(&id.0)) {
        return Err(Error::Format);
    }
    Ok(snapshot)
}

#[cfg(test)]
thread_local! {
    /// The [`read_trees`] calls this thread made, for tests: a start after a
    /// kill reads every tree once.
    pub(in crate::tree_actor) static TREE_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Every tree as built: each branch's snapshot installed, each file's map
/// from `map` (given its tree, object and path).
pub(in crate::tree_actor) fn read_trees(
    repo: &TreeGit,
    pointers: &Pointers,
    objects: &BTreeMap<TreeId, Objects>,
    mut map: impl FnMut(TreeId, u64, &str) -> Result<Arc<FileBytes>>,
) -> Result<BTreeMap<TreeId, Tree>> {
    #[cfg(test)]
    TREE_READS.with(|n| n.set(n.get() + 1));
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

/// The commit of every direct save the pointers reach, by its event number,
/// and every one of them history flagged unverified (`verify`): one Git
/// child, its messages folded in as they stream, none held.
pub(in crate::tree_actor) fn owners(
    repo: &TreeGit,
    pointers: &Pointers,
) -> Result<(built::Owners, Vec<verify::Unverified>)> {
    let _timer = Timer::start(Which::EventOwners);
    let mut owners = built::Owners::new();
    let mut flagged = Vec::new();
    repo.for_each_commit_message(&refs::owner_tips(pointers), |oid, message| {
        if let Some(event) = message::direct_event(message) {
            owners.insert(event, oid);
            flagged.extend(verify::Unverified::of_commit(oid, message));
        }
    })?;
    Ok((owners, flagged))
}

/// Where `tree`'s checkout holds the file named `name`: its `files/` path
/// under `trees/<tree>/` (`format::file_path`).
/// The state area's directory of links to checked-out files as built
/// (`recording::file_map::BaseFile`).
pub(in crate::tree_actor) const BASES: &str = "bases";

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

/// What a cut-short build or a killed repack left in the store, which nothing
/// names: fast-import's temporary packs, `.keep` files, an index without its
/// pack, and `git repack`'s `.tmp-*` files.
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
        if name.starts_with("tmp_")
            || name.starts_with(".tmp-")
            || path.extension().is_some_and(|e| e == "keep")
            || orphan_index
        {
            fs::remove_file(&path)?;
        }
    }
    File::open(&dir)?.sync_all()
}
