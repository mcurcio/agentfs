//! Directories, metadata and links: what a landing judges beside lines and names, and what a
//! sync keeps of a fork's (MODEL.md rules 13, 14 and 15; docs/library/engine.md, "Landing
//! directories, metadata and links").
//!
//! An entry is a file (a link is a file whose bytes are its target, rule 15) or a directory.
//! Each has one metadata value: its kind, mode bits, owner and group. A file's follows its
//! identity; a directory has none: it is its path. Whether an entry is there is part of its
//! value.

use alloc::borrow::ToOwned;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Bound;

use serde::{Deserialize, Serialize};

use super::{File, NameHistory, Snapshot};
use crate::MergeFileIdentity;
use crate::{Error, Result, queue};
use agentfs_contracts::meta::Meta;
use agentfs_contracts::paths::parent_of;

/// Why a change lost by rule 13 or 14: the cause its collision names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cause {
    /// Rule 13: the upstream holds a value the change's writer never saw.
    Metadata,
    /// Rule 14: the change removes or moves a directory, or puts an entry onto a path, where
    /// the upstream has an entry its writer never saw.
    DirectoryNotEmpty,
    /// Rule 14: the change puts an entry into a directory the upstream no longer has, and did
    /// not make that directory itself.
    DirectoryGone,
}

/// One loss by rule 13 or 14: its cause, and the entry it is about: a file's path, or a
/// directory's path and a `/`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Loss {
    pub cause: Cause,
    pub path: String,
}

/// An entry at a path: a file, with its identity, or a directory (none).
type Entry = (String, Option<MergeFileIdentity>);

/// A record rule 13 compares: a file's metadata by identity, or a directory's by path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Record<'a> {
    Meta(MergeFileIdentity),
    Dir(&'a str),
}

/// File `id`'s path and metadata in `snapshot`, if it is live there.
fn live<L>(snapshot: &Snapshot<L>, id: MergeFileIdentity) -> Result<Option<(&str, Meta)>> {
    let (Some(file), Some(name)) = (snapshot.files.get(&id), snapshot.names.get(&id)) else {
        return Ok(None);
    };
    Ok(name.path()?.map(|path| (path, file.meta)))
}

/// A tree's entries as rules 13 and 14 read them.
struct Entries<'a> {
    files: BTreeMap<MergeFileIdentity, (&'a str, Meta)>,
    dirs: &'a BTreeMap<String, Meta>,
    all: BTreeSet<Entry>,
}

impl<'a> Entries<'a> {
    fn of<L>(snapshot: &'a Snapshot<L>) -> Result<Self> {
        let mut files = BTreeMap::new();
        let mut all = BTreeSet::new();
        for id in snapshot.files.keys() {
            if let Some((path, meta)) = live(snapshot, *id)? {
                files.insert(*id, (path, meta));
                all.insert((path.to_owned(), Some(*id)));
            }
        }
        all.extend(snapshot.dirs.keys().map(|path| (path.clone(), None)));
        Ok(Self {
            files,
            dirs: &snapshot.dirs,
            all,
        })
    }

    fn value(&self, record: Record<'_>) -> Option<Meta> {
        match record {
            Record::Meta(id) => self.files.get(&id).map(|(_, meta)| *meta),
            Record::Dir(path) => self.dirs.get(path).copied(),
        }
    }

    fn is_dir(&self, path: &str) -> bool {
        is_dir(self.dirs, path)
    }

    /// The entries at `path`: one, as invariant D1 holds.
    fn at(&self, path: &str) -> impl Iterator<Item = &Entry> {
        self.all
            .range((path.to_owned(), None)..=(path.to_owned(), Some(MergeFileIdentity(u64::MAX))))
    }

    /// The entries under directory `dir`, at any depth.
    fn under(&self, dir: &str) -> impl Iterator<Item = &Entry> {
        // `0` follows `/`: every path under `dir/` sorts before `dir0`.
        self.all.range((
            Bound::Included((format!("{dir}/"), None)),
            Bound::Excluded((format!("{dir}0"), None)),
        ))
    }

    /// Every record either side holds.
    fn records<'b>(&'b self, other: &'b Entries<'_>) -> BTreeSet<Record<'b>> {
        let files = self.files.keys().chain(other.files.keys());
        let dirs = self.dirs.keys().chain(other.dirs.keys());
        files
            .map(|id| Record::Meta(*id))
            .chain(dirs.map(|path| Record::Dir(path)))
            .collect()
    }
}

/// Whether `path` is a directory of `dirs`: the root always is.
fn is_dir(dirs: &BTreeMap<String, Meta>, path: &str) -> bool {
    path.is_empty() || dirs.contains_key(path)
}

/// A directory's label in a collision: its path and a `/`.
fn dir_label(path: &str) -> String {
    format!("{path}/")
}

/// Rules 13 and 14 at the upstream's queue: a change from `base` (its first parent) to `tip`
/// into the upstream at `ours`. Empty: what it does here may land; else each loss, once, and
/// the change loses whole (rule 3e).
pub(super) fn land<L>(
    base: &Snapshot<L>,
    tip: &Snapshot<L>,
    ours: &Snapshot<L>,
) -> Result<Vec<Loss>> {
    let (base, tip, ours) = (Entries::of(base)?, Entries::of(tip)?, Entries::of(ours)?);
    let mut out = BTreeSet::new();
    // Rule 13: a new value lands over the value its writer saw, or onto the same new value;
    // else the upstream's value wins. Making or removing an entry is a value too.
    for record in base.records(&tip) {
        let (b, t, o) = (base.value(record), tip.value(record), ours.value(record));
        if b != t && o != b && o != t {
            let path = match record {
                Record::Meta(id) => tip
                    .files
                    .get(&id)
                    .or_else(|| base.files.get(&id))
                    .map(|(path, _)| (*path).to_owned())
                    .ok_or(Error::Format)?,
                Record::Dir(path) => dir_label(path),
            };
            out.insert(Loss {
                cause: Cause::Metadata,
                path,
            });
        }
    }
    // Rule 14: a directory the change removes or moves away, where the upstream has an entry
    // under it that the change's first parent lacks.
    for dir in base.dirs.keys().filter(|dir| !tip.is_dir(dir)) {
        let seen: BTreeSet<&Entry> = base.under(dir).collect();
        if ours.under(dir).any(|entry| !seen.contains(entry)) {
            out.insert(Loss {
                cause: Cause::DirectoryNotEmpty,
                path: dir_label(dir),
            });
        }
    }
    for (path, id) in tip.all.difference(&base.all) {
        // Rule 14: an entry onto a path where the upstream has an entry of the other kind its
        // writer never saw (two files at one path are rule 11's); a directory moved there, with
        // what is under it, where the upstream has an entry under that path its writer never
        // saw. A directory made empty agrees with the upstream's (rule 13).
        let seen: BTreeSet<&Entry> = base.at(path).chain(base.under(path)).collect();
        let clash = ours
            .at(path)
            .any(|entry| entry.1.is_some() != id.is_some() && !seen.contains(entry));
        let filled = id.is_none()
            && tip.under(path).next().is_some()
            && ours.under(path).any(|entry| !seen.contains(entry));
        if clash || filled {
            out.insert(Loss {
                cause: Cause::DirectoryNotEmpty,
                path: if id.is_some() {
                    path.clone()
                } else {
                    dir_label(path)
                },
            });
        }
        // Rule 14: an entry into a directory the upstream no longer has, which the change did
        // not make itself.
        let dir = parent_of(path);
        if !ours.is_dir(dir) && base.is_dir(dir) {
            out.insert(Loss {
                cause: Cause::DirectoryGone,
                path: dir_label(dir),
            });
        }
    }
    Ok(out.into_iter().collect())
}

/// The keys rule 14 reads of the upstream for a change from `base` to `tip`: each directory it
/// removes or moves, each path an entry arrives at, and that path's directory. A step that
/// touches one after the decision refuses the take; an entry made or moved under a directory
/// touches the directory's key too ([`with_directories`]).
pub(super) fn consulted<L>(base: &Snapshot<L>, tip: &Snapshot<L>) -> Result<BTreeSet<queue::Key>> {
    let (base, tip) = (Entries::of(base)?, Entries::of(tip)?);
    let mut keys = BTreeSet::new();
    for dir in base.dirs.keys().filter(|dir| !tip.is_dir(dir)) {
        keys.insert(queue::Key::Name(dir.clone()));
    }
    for (path, _) in tip.all.difference(&base.all) {
        keys.insert(queue::Key::Name(path.clone()));
        keys.insert(queue::Key::Name(parent_of(path).to_owned()));
    }
    Ok(keys)
}

/// `keys` with the key of every directory above each name: a step that makes, moves or
/// removes an entry under a directory touches that directory for the takes that judged it.
/// The root is never removed or moved, so it is left out.
pub fn with_directories(keys: impl IntoIterator<Item = queue::Key>) -> BTreeSet<queue::Key> {
    let mut out = BTreeSet::new();
    for key in keys {
        if let queue::Key::Name(path) = &key {
            let mut dir = parent_of(path);
            while !dir.is_empty() && out.insert(queue::Key::Name(dir.to_owned())) {
                dir = parent_of(dir);
            }
        }
        out.insert(key);
    }
    out
}

/// A landing's records applied over `applied` (the upstream's snapshot with the change's names
/// and lines): each value the change wrote, from `base` to `tip`. Rule 3e decides whether any
/// of it stays.
pub(super) fn apply<L>(
    base: &Snapshot<L>,
    tip: &Snapshot<L>,
    applied: &mut Snapshot<L>,
) -> Result<()> {
    let (base, tip) = (Entries::of(base)?, Entries::of(tip)?);
    for record in base.records(&tip) {
        let value = tip.value(record);
        if base.value(record) == value {
            continue;
        }
        match (record, value) {
            (Record::Meta(id), Some(meta)) => {
                if let Some(file) = applied.files.get_mut(&id) {
                    file.meta = meta;
                }
            }
            // A file's end is its name's (rule 11).
            (Record::Meta(_), None) => {}
            (Record::Dir(path), Some(meta)) => {
                applied.dirs.insert(path.to_owned(), meta);
            }
            (Record::Dir(path), None) => {
                applied.dirs.remove(path);
            }
        }
    }
    Ok(())
}

/// Rule 14 at a sync: the upstream's directories, the fork's undecided directory records
/// (`own`) kept, then every directory an entry of the result needs kept as the fork has it,
/// until the upstream decides; a file the fork has keeps the fork's name.
///
/// An upstream entry the fork never had, under a directory the fork's undecided change
/// removed or moved, is a case the rules do not decide (`crates/agentfs-model/MODEL.md`,
/// "Left out"). The result leaves it out and names its path ([`Synced::unruled`]): a sync
/// that would show that result fails (the engine's `Error::Unruled`); one that delivers a later
/// upstream commit, which decided the change, passes through it.
pub(super) fn sync<L>(
    upstream: &Snapshot<L>,
    fork: &Snapshot<L>,
    own: &BTreeSet<String>,
    names: &mut BTreeMap<MergeFileIdentity, NameHistory>,
    files: &mut BTreeMap<MergeFileIdentity, File<L>>,
) -> Result<(BTreeMap<String, Meta>, Option<String>)> {
    let mut unruled = None;
    let mut dirs = upstream.dirs.clone();
    for path in own {
        match fork.dirs.get(path) {
            Some(meta) => dirs.insert(path.clone(), *meta),
            None => dirs.remove(path),
        };
    }
    loop {
        let mut orphan = None;
        for path in dirs.keys().filter(|path| !path.is_empty()) {
            if !is_dir(&dirs, parent_of(path)) {
                orphan = Some((path.clone(), None));
                break;
            }
        }
        if orphan.is_none() {
            for (id, name) in names.iter() {
                if let (Some(path), true) = (name.path()?, files.contains_key(id))
                    && !is_dir(&dirs, parent_of(path))
                {
                    orphan = Some((path.to_owned(), Some(*id)));
                    break;
                }
            }
        }
        let Some((path, id)) = orphan else {
            return Ok((dirs, unruled));
        };
        let dir = parent_of(&path);
        if let Some(meta) = fork.dirs.get(dir) {
            dirs.insert(dir.to_owned(), *meta);
            continue;
        }
        match id.map(|id| (id, fork.names.get(&id))) {
            Some((id, Some(own))) if fork.files.contains_key(&id) => {
                let name = names.get_mut(&id).ok_or(Error::Format)?;
                if name.head == own.head {
                    // The fork's own name needs a directory the fork lacks.
                    return Err(Error::Format);
                }
                name.head = own.head;
            }
            Some((id, _)) => {
                names.remove(&id);
                files.remove(&id);
                unruled.get_or_insert(path);
            }
            None => {
                dirs.remove(&path);
                unruled.get_or_insert(path);
            }
        }
    }
}
