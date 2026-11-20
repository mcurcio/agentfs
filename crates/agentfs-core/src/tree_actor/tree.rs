//! A tree's shared types: the event numbers a journal gives steps, and the line versions they name
//! (docs/target-architecture/git-model.md §5, §7, §12).

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Deref;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};

use git2::Oid;

use serde::{Deserialize, Serialize};

use super::journal::{Change, SaveOutcome, TakeRecord, TreeLocals};
use super::meta::{Attrs, Kind, Meta, Perm, Times};
use super::recording::FileMap;
use super::{Error, Result, queue, referee};
use crate::tree_reconcile::{Id, LineMap};

/// A step's journal number. It numbers the step's name versions and line versions, and the object,
/// handle or fork a step creates. Volume-wide; never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct EventNumber(u64);

impl EventNumber {
    /// Once per step, in `run_step`.
    pub(super) const fn new(seq: u64) -> Self {
        Self(seq)
    }

    pub(super) const fn get(self) -> u64 {
        self.0
    }

    /// The one event-to-object step: a create's object (and so its first identity), and the new
    /// object a sync gives a file the fork ended (R11.3).
    pub(super) const fn object(self) -> u64 {
        self.0
    }
}

/// Decimal; honours width and fill, so `{:020}` is a queue entry's number.
impl fmt::Display for EventNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// One line version as history stores it, `"<event>:<index>"` (a line map's
/// runs, `Agentfs-Collisions`): the event that wrote it and its place among that event's new versions,
/// from 1. A line's identity is its first version.
///
/// It **orders as its text** (`"10:1" < "9:1"`): the order a map of text keys wrote `ancestors`
/// in, so no stored blob depends on this type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub(super) struct LineVersion {
    event: EventNumber,
    index: u64,
}

/// The longest text: two `u64`s of 20 digits and the colon.
const TEXT: usize = 41;

/// Writes `n` in decimal at the end of `buf[..end]`, returning where it starts.
fn digits(buf: &mut [u8; TEXT], mut end: usize, mut n: u64) -> usize {
    loop {
        end -= 1;
        // `n % 10` is below 10: the cast keeps every bit.
        buf[end] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            return end;
        }
    }
}

impl LineVersion {
    pub(super) const fn event(self) -> EventNumber {
        self.event
    }

    /// The text, formatted on the stack.
    fn text(self, buf: &mut [u8; TEXT]) -> &[u8] {
        let start = digits(buf, TEXT, self.index);
        buf[start - 1] = b':';
        let start = digits(buf, start - 1, self.event.0);
        &buf[start..]
    }
}

/// An event's new versions number on: `<event>:<index>`, `<event>:<index + 1>`, ...
impl Id for LineVersion {
    fn at(writer: u64, index: u64) -> Self {
        Self {
            event: EventNumber(writer),
            index,
        }
    }

    fn writer(&self) -> u64 {
        self.event.0
    }

    fn index(&self) -> u64 {
        self.index
    }
}

/// The byte order of the two texts.
impl Ord for LineVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        let (mut a, mut b) = ([0; TEXT], [0; TEXT]);
        self.text(&mut a).cmp(other.text(&mut b))
    }
}

impl PartialOrd for LineVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for LineVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.event, self.index)
    }
}

/// Decimal without sign or leading zero.
fn canonical(text: &str) -> Option<u64> {
    if text.is_empty()
        || !text.bytes().all(|b| b.is_ascii_digit())
        || (text != "0" && text.starts_with('0'))
    {
        return None;
    }
    text.parse().ok()
}

/// [`Error::Format`] unless canonical: `<u64>:<u64>`, decimal, no sign, no leading zero, index
/// at least 1. So a parsed version prints back to the bytes it came from.
impl FromStr for LineVersion {
    type Err = Error;

    fn from_str(text: &str) -> std::result::Result<Self, Error> {
        let (event, index) = text.split_once(':').ok_or(Error::Format)?;
        let event = canonical(event).ok_or(Error::Format)?;
        let index = canonical(index)
            .filter(|index| *index >= 1)
            .ok_or(Error::Format)?;
        Ok(Self {
            event: EventNumber(event),
            index,
        })
    }
}

impl From<LineVersion> for String {
    fn from(version: LineVersion) -> Self {
        version.to_string()
    }
}

impl TryFrom<String> for LineVersion {
    type Error = Error;

    fn try_from(text: String) -> std::result::Result<Self, Error> {
        text.parse()
    }
}

/// One step's new line versions in order: `<event>:1`, `<event>:2`, ...
pub(super) struct Versions {
    event: EventNumber,
    last: u64,
}

impl Versions {
    pub(super) const fn of(event: EventNumber) -> Self {
        Self { event, last: 0 }
    }

    #[cfg(test)]
    pub(super) fn next(&mut self) -> LineVersion {
        self.take(1)
    }

    /// `n` new versions in order; the first of them.
    pub(super) fn take(&mut self, n: u64) -> LineVersion {
        let first = LineVersion {
            event: self.event,
            index: self.last + 1,
        };
        self.last += n;
        first
    }
}

/// A file's line map as the engine stores it.
pub(super) type Lines = LineMap<LineVersion>;

/// A built file's map and, once built, its blob. A new map is a new value,
/// so a blob found once holds for as long as the map is shared: a build
/// stores only the blobs of the files it changed.
#[derive(Debug, Default)]
pub(super) struct FileBytes {
    map: FileMap,
    blob: OnceLock<Oid>,
}

impl FileBytes {
    /// The bytes' blob, once one is known to be stored.
    pub(super) fn known_blob(&self) -> Option<Oid> {
        self.blob.get().copied()
    }

    /// Notes the bytes' stored blob.
    pub(super) fn set_blob(&self, oid: Oid) {
        // Content-addressed: whoever set it first set the same id.
        let _ = self.blob.set(oid);
    }
}

impl FileBytes {
    /// The same bytes on another map (a rebase), keeping the blob.
    pub(super) fn with_map(&self, map: FileMap) -> Self {
        Self {
            map,
            blob: self.blob.clone(),
        }
    }
}

impl From<FileMap> for FileBytes {
    fn from(map: FileMap) -> Self {
        Self {
            map,
            blob: OnceLock::new(),
        }
    }
}

impl Deref for FileBytes {
    type Target = FileMap;

    fn deref(&self) -> &FileMap {
        &self.map
    }
}

/// A tree's names, by path and by object: a view's lookups by name and by
/// inode cost one map lookup, not a scan of the tree. Reads go through
/// `Deref` to the map by path; every change goes through these methods, so
/// the two maps cannot disagree.
#[derive(Clone, Debug, Default)]
pub(super) struct Names {
    by_path: BTreeMap<String, u64>,
    /// One entry per (object, path), not a set per object: a set's node costs
    /// hundreds of bytes for the one path most objects have.
    by_object: BTreeSet<(u64, String)>,
}

impl Names {
    pub(super) fn insert(&mut self, path: String, object: u64) -> Option<u64> {
        let replaced = self.by_path.insert(path.clone(), object);
        if let Some(replaced) = replaced {
            self.unindex(replaced, &path);
        }
        self.by_object.insert((object, path));
        replaced
    }

    pub(super) fn remove(&mut self, path: &str) -> Option<u64> {
        let object = self.by_path.remove(path)?;
        self.unindex(object, path);
        Some(object)
    }

    pub(super) fn clear(&mut self) {
        self.by_path.clear();
        self.by_object.clear();
    }

    /// The paths naming `object`, in path order.
    pub(super) fn of(&self, object: u64) -> impl Iterator<Item = &str> {
        self.by_object
            .range((object, String::new())..)
            .take_while(move |(o, _)| *o == object)
            .map(|(_, path)| path.as_str())
    }

    fn unindex(&mut self, object: u64, path: &str) {
        self.by_object.remove(&(object, path.to_owned()));
    }
}

impl Deref for Names {
    type Target = BTreeMap<String, u64>;

    fn deref(&self) -> &BTreeMap<String, u64> {
        &self.by_path
    }
}

/// One directory of a tree: its number (its mkdir record's; the root's is 0
/// and never an inode's), its metadata and times, and how many directories
/// it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DirState {
    pub(super) number: u64,
    pub(super) meta: Meta,
    pub(super) times: Times,
    pub(super) subdirs: u32,
}

/// A tree's directories, by path (`""` the root) and by number: a view's
/// lookups cost one map lookup. Reads go through `Deref` to the map by path;
/// every change goes through these methods, so the two maps cannot disagree.
#[derive(Clone, Debug, Default)]
pub(super) struct Dirs {
    by_path: BTreeMap<String, DirState>,
    by_number: BTreeMap<u64, String>,
}

impl Dirs {
    fn insert(&mut self, path: String, state: DirState) {
        if let Some(old) = self.by_path.remove(&path) {
            self.by_number.remove(&old.number);
        }
        if !path.is_empty() {
            self.by_number.insert(state.number, path.clone());
        }
        self.by_path.insert(path, state);
    }

    fn remove(&mut self, path: &str) -> Option<DirState> {
        let state = self.by_path.remove(path)?;
        if !path.is_empty() {
            self.by_number.remove(&state.number);
        }
        Some(state)
    }

    /// The directory's state, to change all but its number.
    fn get_mut(&mut self, path: &str) -> Option<&mut DirState> {
        self.by_path.get_mut(path)
    }

    /// The path of directory `number`; never the root's.
    pub(super) fn path_of(&self, number: u64) -> Option<&str> {
        self.by_number.get(&number).map(String::as_str)
    }

    fn clear(&mut self) {
        self.by_path.clear();
        self.by_number.clear();
    }
}

impl Deref for Dirs {
    type Target = BTreeMap<String, DirState>;

    fn deref(&self) -> &BTreeMap<String, DirState> {
        &self.by_path
    }
}

/// `S_ISGID`.
const SET_GROUP_ID: u32 = 0o2000;

/// The directory holding `path`: `""` for a top-level entry.
pub(super) fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// The prefix every path under directory `dir` starts with.
fn under(dir: &str) -> String {
    if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    }
}

/// `path` with its leading `from` replaced by `to`.
fn moved(path: &str, from: &str, to: &str) -> String {
    format!("{to}{}", &path[from.len()..])
}

/// The keys of `map` that are direct children of the directory whose paths
/// start with `prefix`, in order: a child directory's subtree is jumped
/// over, never walked.
fn direct_children<'a, V>(
    map: &'a BTreeMap<String, V>,
    prefix: &'a str,
) -> impl Iterator<Item = (&'a str, &'a V)> + 'a {
    let mut cursor = prefix.to_owned();
    std::iter::from_fn(move || {
        loop {
            let (key, value) = map
                .range::<str, _>((
                    std::ops::Bound::Included(cursor.as_str()),
                    std::ops::Bound::Unbounded,
                ))
                .next()?;
            if !key.starts_with(prefix) {
                return None;
            }
            let rest = &key[prefix.len()..];
            match rest.find('/') {
                // The directory itself (the root's `""`), never its own child.
                _ if rest.is_empty() => cursor = format!("{key}\0"),
                // A deeper entry: past the whole subtree of its child, whose
                // paths all start with `<child>/` ('/' + 1 is '0').
                Some(at) => cursor = format!("{prefix}{}0", &rest[..at]),
                None => {
                    cursor = format!("{key}\0");
                    return Some((key.as_str(), value));
                }
            }
        }
    })
}

/// How many directories one take may add: the index field of
/// [`taken_dir_number`].
pub(super) const TAKEN_DIRS: u64 = 1 << 20;

/// The number of the `index`th directory a take adds that its tree lacks: a
/// take records directories by path only, so their numbers are its step's,
/// apart from every record's (`EventNumber::object`) by the top bit. Unique
/// by construction: [`Error::TooLarge`] for an index past [`TAKEN_DIRS`] or a
/// step past 43 bits, which would share a number.
pub(super) const fn taken_dir_number(step: EventNumber, index: u64) -> Result<u64> {
    if index >= TAKEN_DIRS || step.0 >= 1 << 43 {
        return Err(Error::TooLarge);
    }
    Ok((1 << 63) | (step.0 << 20) | index)
}

/// The names-level state of one tree, which both projections of the journal
/// keep: the actor's saved state and the builder's trees. [`Shape::apply`] is
/// its one transition. Each file's and link's metadata and times are by
/// object, each directory's in its [`DirState`].
#[derive(Clone, Debug, Default)]
pub(super) struct Shape {
    pub(super) names: Names,
    pub(super) dirs: Dirs,
    pub(super) anonymous: BTreeSet<u64>,
    pub(super) identities: BTreeMap<u64, MergeFileIdentity>,
    pub(super) fresh_names: BTreeSet<u64>,
    pub(super) meta: BTreeMap<u64, Meta>,
    pub(super) times: BTreeMap<u64, Times>,
}

impl Shape {
    /// Adds an object that has no name.
    pub(super) fn add_anonymous(&mut self, object: u64, meta: Meta, at_ns: u64) {
        self.anonymous.insert(object);
        self.identities.insert(object, MergeFileIdentity(object));
        self.fresh_names.insert(object);
        self.meta.insert(object, meta);
        self.times.insert(object, Times::at(at_ns));
    }

    /// The paths naming `object`, in path order.
    pub(super) fn paths_of(&self, object: u64) -> impl Iterator<Item = &str> {
        self.names.of(object)
    }

    /// Whether any name reaches `object`. One no name reaches is an orphan: its close saves
    /// nothing and its opens hold no keys.
    pub(super) fn is_named(&self, object: u64) -> bool {
        self.paths_of(object).next().is_some()
    }

    /// What `path` names: a file's or link's kind, or a directory.
    pub(super) fn kind_at(&self, path: &str) -> Option<Kind> {
        if self.dirs.contains_key(path) {
            return Some(Kind::Dir);
        }
        let object = self.names.get(path)?;
        Some(self.meta.get(object).map_or(Kind::File, |meta| meta.kind))
    }

    /// The entries directly in directory `dir`, in name order: each one's
    /// name, number (a file's or link's object, a directory's number) and
    /// kind, the `skip`th on, at most `count`. Read from the sorted maps,
    /// jumping over each child directory's subtree.
    pub(super) fn children(
        &self,
        dir: &str,
        skip: usize,
        count: usize,
    ) -> Vec<(String, u64, Kind)> {
        let prefix = under(dir);
        let mut files = direct_children(&*self.names, &prefix).peekable();
        let mut dirs = direct_children(&*self.dirs, &prefix).peekable();
        let entries = std::iter::from_fn(move || {
            let take_file = match (files.peek(), dirs.peek()) {
                (Some((f, _)), Some((d, _))) => f < d,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => return None,
            };
            if take_file {
                let (path, object) = files.next()?;
                let kind = self.meta.get(object).map_or(Kind::File, |m| m.kind);
                Some((path, *object, kind))
            } else {
                let (path, state) = dirs.next()?;
                Some((path, state.number, Kind::Dir))
            }
        });
        entries
            .skip(skip)
            .take(count)
            .map(|(path, number, kind)| (path[prefix.len()..].to_owned(), number, kind))
            .collect()
    }

    /// Whether directory `dir` holds no entry.
    fn is_empty_dir(&self, dir: &str) -> bool {
        let prefix = under(dir);
        direct_children(&*self.names, &prefix).next().is_none()
            && direct_children(&*self.dirs, &prefix).next().is_none()
    }

    /// Every entry under directory `dir`: its files' and links' paths with
    /// their objects, and its directories' paths, in path order.
    fn descendants(&self, dir: &str) -> (Vec<(String, u64)>, Vec<String>) {
        let prefix = under(dir);
        let files = self
            .names
            .range(prefix.clone()..)
            .take_while(|(path, _)| path.starts_with(&prefix))
            .map(|(path, object)| (path.clone(), *object))
            .collect();
        let dirs = self
            .dirs
            .range(prefix.clone()..)
            .take_while(|(path, _)| path.starts_with(&prefix))
            .map(|(path, _)| path.clone())
            .collect();
        (files, dirs)
    }

    /// [`Error::NotFound`] unless `path`'s parent is a directory of this
    /// tree, [`Error::NotADirectory`] when a file stands where a directory of
    /// its path should be.
    fn check_parent(&self, path: &str) -> Result<()> {
        let mut at = parent_of(path);
        if self.dirs.contains_key(at) {
            return Ok(());
        }
        loop {
            if self.names.contains_key(at) {
                return Err(Error::NotADirectory);
            }
            if self.dirs.contains_key(at) || at.is_empty() {
                return Err(Error::NotFound);
            }
            at = parent_of(at);
        }
    }

    /// The metadata a new entry of `kind` at `path` takes from `attrs`: in a
    /// set-group-ID directory, the directory's group, and a new directory
    /// keeps the set-group-ID bit (Linux's `inode_init_owner`).
    pub(super) fn new_meta(&self, path: &str, kind: Kind, attrs: Attrs) -> Meta {
        let mut meta = Meta::new(kind, attrs);
        if let Some(parent) = self.dirs.get(parent_of(path))
            && parent.meta.mode.bits() & SET_GROUP_ID != 0
        {
            meta.gid = parent.meta.gid;
            if kind == Kind::Dir {
                meta.mode = Perm::from_mode(meta.mode.bits() | SET_GROUP_ID);
            }
        }
        meta
    }

    /// A new entry at `path`: [`Error::Exists`] if one is there, else as
    /// [`Shape::check_parent`].
    pub(super) fn check_new(&self, path: &str) -> Result<()> {
        if self.kind_at(path).is_some() {
            return Err(Error::Exists);
        }
        self.check_parent(path)
    }

    /// Whether `change` applies to this tree, as [`Shape::apply`] would
    /// find: what a step checks before its record, so it applies after.
    ///
    /// # Errors
    ///
    /// As [`Shape::apply`].
    pub(super) fn check(&self, change: &Change) -> Result<()> {
        match change {
            Change::Create { path, .. }
            | Change::Mkdir { path, .. }
            | Change::Symlink { path, .. } => self.check_new(path),
            Change::Link { object, path, .. } => {
                if !self.anonymous.contains(object) {
                    return Err(Error::NotFound);
                }
                self.check_new(path)
            }
            Change::Rmdir { path, .. } => match self.kind_at(path) {
                _ if path.is_empty() => Err(Error::Name),
                Some(Kind::Dir) if self.is_empty_dir(path) => Ok(()),
                Some(Kind::Dir) => Err(Error::NotEmpty),
                Some(_) => Err(Error::NotADirectory),
                None => Err(Error::NotFound),
            },
            Change::Unlink { path, .. } => match self.kind_at(path) {
                Some(Kind::Dir) => Err(Error::IsADirectory),
                Some(_) => Ok(()),
                None => Err(Error::NotFound),
            },
            Change::Rename { from, to, .. } => self.check_rename(from, to),
            Change::SetMeta { path, meta, .. } => match self.kind_at(path) {
                Some(kind) if kind == meta.kind => Ok(()),
                Some(_) => Err(Error::Format),
                None => Err(Error::NotFound),
            },
            Change::SetTimes { path, .. } => self.kind_at(path).map(drop).ok_or(Error::NotFound),
            Change::Replace { path, meta, .. } => match self.kind_at(path) {
                _ if meta.kind != Kind::File => Err(Error::Format),
                Some(Kind::File) => Ok(()),
                Some(Kind::Dir) => Err(Error::IsADirectory),
                Some(Kind::Symlink) => Err(Error::Invalid),
                None => self.check_parent(path),
            },
            Change::Save { .. }
            | Change::Fork { .. }
            | Change::Rows
            | Change::Drop { .. }
            | Change::Take { .. } => Ok(()),
        }
    }

    /// A rename's errors, as POSIX gives them.
    fn check_rename(&self, from: &str, to: &str) -> Result<()> {
        let kind = self.kind_at(from).ok_or(Error::NotFound)?;
        if from == to {
            return Ok(());
        }
        self.check_parent(to)?;
        let replaced = self.kind_at(to);
        match (kind, replaced) {
            (Kind::Dir, _) if to.starts_with(&under(from)) => Err(Error::Invalid),
            (Kind::Dir, Some(Kind::Dir)) if !self.is_empty_dir(to) => Err(Error::NotEmpty),
            (Kind::Dir, Some(Kind::File | Kind::Symlink)) => Err(Error::NotADirectory),
            (Kind::File | Kind::Symlink, Some(Kind::Dir)) => Err(Error::IsADirectory),
            _ => Ok(()),
        }
    }

    /// The keys a change to `object` touches, as a set.
    pub(super) fn keys(&self, object: u64) -> BTreeSet<queue::Key> {
        let mut keys = BTreeSet::new();
        self.keys_of(object, &mut keys);
        keys
    }

    /// The keys a change to `object` touches: its identity and every path naming it.
    fn keys_of(&self, object: u64, keys: &mut BTreeSet<queue::Key>) {
        if let Some(id) = self.identities.get(&object) {
            keys.insert(queue::Key::File(*id));
        }
        keys.extend(
            self.paths_of(object)
                .map(|p| queue::Key::Name(p.to_owned())),
        );
    }

    /// Directory `dir`'s entries changed at `at_ns`: its mtime and ctime move.
    fn dir_modified(&mut self, dir: &str, at_ns: u64) {
        if let Some(state) = self.dirs.get_mut(dir) {
            state.times.modified(at_ns);
        }
    }

    fn object_changed(&mut self, object: u64, at_ns: u64) {
        if let Some(times) = self.times.get_mut(&object) {
            times.changed(at_ns);
        }
    }

    /// Adds a file or link object at `path`, new at `at_ns`.
    fn add_named(&mut self, path: &str, object: u64, meta: Meta, at_ns: u64) {
        self.names.insert(path.to_owned(), object);
        self.identities.insert(object, MergeFileIdentity(object));
        self.fresh_names.insert(object);
        self.meta.insert(object, meta);
        self.times.insert(object, Times::at(at_ns));
        self.dir_modified(parent_of(path), at_ns);
    }

    fn add_dir(&mut self, path: &str, number: u64, meta: Meta, at_ns: u64) {
        self.dirs.insert(
            path.to_owned(),
            DirState {
                number,
                meta,
                times: Times::at(at_ns),
                subdirs: 0,
            },
        );
        let parent = parent_of(path);
        if let Some(state) = self.dirs.get_mut(parent) {
            state.subdirs += 1;
        }
        self.dir_modified(parent, at_ns);
    }

    fn remove_dir(&mut self, path: &str, at_ns: u64) -> Option<DirState> {
        let state = self.dirs.remove(path)?;
        let parent = parent_of(path);
        if let Some(state) = self.dirs.get_mut(parent) {
            state.subdirs = state.subdirs.saturating_sub(1);
        }
        self.dir_modified(parent, at_ns);
        Some(state)
    }

    /// The one transition of names, directories, objects, identities, their
    /// metadata and times for a step of this tree, which the actor runs at the
    /// call and the builder at the build; the keys it touched. `at_ns` is the
    /// step's record's time. A fork's step is the caller's (it copies another
    /// tree), and a save's is [`Shape::save`].
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], [`Error::Exists`], [`Error::NotADirectory`],
    /// [`Error::IsADirectory`], [`Error::NotEmpty`] and [`Error::Invalid`] as
    /// POSIX gives them ([`Shape::check`], before anything changes), and
    /// [`Error::Format`] for a take that would put two files on one object or
    /// leave an entry without its directory.
    pub(super) fn apply(
        &mut self,
        change: &Change,
        step: EventNumber,
        at_ns: u64,
    ) -> Result<BTreeSet<queue::Key>> {
        self.check(change)?;
        let mut keys = BTreeSet::new();
        match change {
            Change::Create { path, meta, .. } | Change::Symlink { path, meta, .. } => {
                let object = step.object();
                self.add_named(path, object, *meta, at_ns);
                self.keys_of(object, &mut keys);
            }
            Change::Mkdir { path, meta, .. } => {
                self.add_dir(path, step.get(), *meta, at_ns);
                keys.insert(queue::Key::Name(path.clone()));
            }
            Change::Rmdir { path, .. } => {
                self.remove_dir(path, at_ns);
                keys.insert(queue::Key::Name(path.clone()));
            }
            Change::Link { object, path, .. } => {
                self.anonymous.remove(object);
                self.names.insert(path.clone(), *object);
                self.object_changed(*object, at_ns);
                self.dir_modified(parent_of(path), at_ns);
                self.keys_of(*object, &mut keys);
            }
            Change::Rename { from, to, .. } if from == to => {}
            Change::Rename { from, to, .. } if self.dirs.contains_key(from.as_str()) => {
                self.rename_dir(from, to, at_ns, &mut keys);
            }
            Change::Rename { from, to, .. } => {
                let object = self.names.get(from).copied().ok_or(Error::NotFound)?;
                self.keys_of(object, &mut keys);
                let replaced = self.names.get(to).copied();
                if let Some(replaced) = replaced {
                    self.keys_of(replaced, &mut keys);
                }
                self.names.remove(from);
                match replaced {
                    Some(replaced) if self.fresh_names.contains(&object) => {
                        let id = *self.identities.get(&replaced).ok_or(Error::Format)?;
                        self.identities.insert(object, id);
                        if !self.fresh_names.contains(&replaced) {
                            self.fresh_names.remove(&object);
                        }
                    }
                    _ => {
                        self.fresh_names.remove(&object);
                    }
                }
                self.names.insert(to.clone(), object);
                self.object_changed(object, at_ns);
                self.dir_modified(parent_of(from), at_ns);
                self.dir_modified(parent_of(to), at_ns);
                keys.insert(queue::Key::Name(to.clone()));
            }
            Change::Unlink { path, .. } => {
                let object = self.names.get(path).copied().ok_or(Error::NotFound)?;
                self.keys_of(object, &mut keys);
                self.names.remove(path);
                self.object_changed(object, at_ns);
                self.dir_modified(parent_of(path), at_ns);
            }
            Change::SetMeta { path, meta, .. } => {
                if let Some(state) = self.dirs.get_mut(path) {
                    state.meta = *meta;
                    state.times.changed(at_ns);
                    keys.insert(queue::Key::Name(path.clone()));
                } else {
                    let object = self.names.get(path).copied().ok_or(Error::NotFound)?;
                    self.meta.insert(object, *meta);
                    self.object_changed(object, at_ns);
                    self.keys_of(object, &mut keys);
                }
            }
            Change::SetTimes {
                path, atime, mtime, ..
            } => {
                let times = match self.dirs.get_mut(path) {
                    Some(state) => &mut state.times,
                    None => {
                        let object = self.names.get(path).copied().ok_or(Error::NotFound)?;
                        self.times.entry(object).or_default()
                    }
                };
                if let Some(atime) = atime {
                    times.atime_ns = atime.resolve(at_ns);
                }
                if let Some(mtime) = mtime {
                    times.mtime_ns = mtime.resolve(at_ns);
                }
                times.changed(at_ns);
            }
            Change::Take { take, .. } => self.take(take, step, at_ns, &mut keys)?,
            // A save of the file there, with its new metadata; for a new
            // path, a create and then a save.
            Change::Replace { path, meta, .. } => {
                let object = match self.names.get(path).copied() {
                    Some(object) => {
                        self.meta.insert(object, *meta);
                        object
                    }
                    None => {
                        let object = step.object();
                        self.add_named(path, object, *meta, at_ns);
                        object
                    }
                };
                if let Some(times) = self.times.get_mut(&object) {
                    times.modified(at_ns);
                }
                self.keys_of(object, &mut keys);
            }
            Change::Save { .. } | Change::Fork { .. } | Change::Rows | Change::Drop { .. } => {}
        }
        Ok(keys)
    }

    /// A directory's rename: every entry under it moves with it, each
    /// directory keeping its number and each file its object and identity.
    /// A moved file's name is no longer fresh. An empty directory at `to` is
    /// replaced.
    fn rename_dir(&mut self, from: &str, to: &str, at_ns: u64, keys: &mut BTreeSet<queue::Key>) {
        let (files, dirs) = self.descendants(from);
        if self.dirs.contains_key(to) {
            self.remove_dir(to, at_ns);
        }
        keys.insert(queue::Key::Name(from.to_owned()));
        keys.insert(queue::Key::Name(to.to_owned()));
        if let Some(mut state) = self.remove_dir(from, at_ns) {
            state.times.changed(at_ns);
            let subdirs = state.subdirs;
            self.add_dir(to, state.number, state.meta, at_ns);
            if let Some(moved_state) = self.dirs.get_mut(to) {
                moved_state.times = state.times;
                moved_state.subdirs = subdirs;
            }
        }
        for dir in dirs {
            if let Some(state) = self.dirs.remove(&dir) {
                let new = moved(&dir, from, to);
                keys.insert(queue::Key::Name(dir));
                keys.insert(queue::Key::Name(new.clone()));
                self.dirs.insert(new, state);
            }
        }
        for (path, object) in files {
            self.keys_of(object, keys);
            self.names.remove(&path);
            let new = moved(&path, from, to);
            keys.insert(queue::Key::Name(new.clone()));
            self.names.insert(new, object);
            self.fresh_names.remove(&object);
        }
    }

    /// A close's part: an anonymous object's close drops it; a save moves
    /// its file's mtime and ctime; any other changes nothing. The keys a
    /// saved file's change touches.
    pub(super) fn save(
        &mut self,
        object: u64,
        outcome: SaveOutcome,
        at_ns: u64,
    ) -> BTreeSet<queue::Key> {
        let mut keys = BTreeSet::new();
        match outcome {
            SaveOutcome::AnonymousDropped => self.drop_object(object),
            SaveOutcome::Saved { .. } => {
                if let Some(times) = self.times.get_mut(&object) {
                    times.modified(at_ns);
                }
                self.keys_of(object, &mut keys);
            }
            SaveOutcome::Unchanged | SaveOutcome::OrphanDropped => {}
        }
        keys
    }

    pub(super) fn drop_object(&mut self, object: u64) {
        self.anonymous.remove(&object);
        self.identities.remove(&object);
        self.fresh_names.remove(&object);
        self.meta.remove(&object);
        self.times.remove(&object);
    }

    /// A take: the files, names and directories it changed laid over this
    /// tree's, as installing its whole snapshot would leave them; then D1:
    /// every entry's directory is a directory of the tree, and a path names
    /// one entry at most.
    fn take(
        &mut self,
        take: &TakeRecord,
        step: EventNumber,
        at_ns: u64,
        keys: &mut BTreeSet<queue::Key>,
    ) -> Result<()> {
        // Each named file: its object, path and whether its name is fresh.
        let mut files: BTreeMap<MergeFileIdentity, (u64, String, bool)> = BTreeMap::new();
        for (path, object) in self.names.iter() {
            let id = *self.identities.get(object).ok_or(Error::Format)?;
            files
                .entry(id)
                .or_insert_with(|| (*object, path.clone(), self.fresh_names.contains(object)));
        }
        let before: BTreeSet<String> = self.names.keys().cloned().collect();
        for (id, file) in &take.files {
            keys.insert(queue::Key::File(*id));
            if let Some((_, path, _)) = files.get(id) {
                keys.insert(queue::Key::Name(path.clone()));
            }
            match file {
                Some(file) => {
                    match files.get_mut(id) {
                        Some(entry) => entry.0 = file.object,
                        None => {
                            files.insert(*id, (file.object, String::new(), false));
                        }
                    }
                    self.meta.insert(file.object, file.meta);
                    self.times
                        .entry(file.object)
                        .or_insert_with(|| Times::at(at_ns))
                        .modified(at_ns);
                }
                None => {
                    files.remove(id);
                }
            }
        }
        for (id, history) in &take.names {
            keys.insert(queue::Key::File(*id));
            let Some(entry) = files.get_mut(id) else {
                continue;
            };
            let history = history.as_ref().ok_or(Error::Format)?;
            keys.insert(queue::Key::Name(entry.1.clone()));
            entry.1 = history.path()?.ok_or(Error::Format)?.to_owned();
            entry.2 = history.head_version()?.over.is_none();
        }
        self.names.clear();
        self.fresh_names.clear();
        let mut objects = BTreeSet::new();
        for (id, (object, path, fresh)) in files {
            if !objects.insert(object) || path.is_empty() {
                return Err(Error::Format);
            }
            keys.insert(queue::Key::Name(path.clone()));
            self.names.insert(path, object);
            self.identities.insert(object, id);
            if fresh {
                self.fresh_names.insert(object);
            }
        }
        // Directories: parents before children when adding, children
        // before parents when removing.
        let mut dirs: Vec<&(String, Option<Meta>)> = take.dirs.iter().collect();
        dirs.sort_by(|a, b| a.0.cmp(&b.0));
        let mut added = 0;
        for (path, meta) in &dirs {
            let Some(meta) = meta else { continue };
            keys.insert(queue::Key::Name(path.clone()));
            match self.dirs.get_mut(path) {
                Some(state) => {
                    state.meta = *meta;
                    state.times.changed(at_ns);
                }
                None => {
                    self.add_dir(path, taken_dir_number(step, added)?, *meta, at_ns);
                    added += 1;
                }
            }
        }
        for (path, meta) in dirs.iter().rev() {
            if meta.is_none() {
                keys.insert(queue::Key::Name(path.clone()));
                self.remove_dir(path, at_ns).ok_or(Error::Format)?;
            }
        }
        // Entries added or removed: their directories' mtimes move.
        let after: BTreeSet<String> = self.names.keys().cloned().collect();
        for path in before.symmetric_difference(&after) {
            self.dir_modified(parent_of(path), at_ns);
        }
        self.check_d1()
    }

    /// D1: every entry's directory is a directory of this tree, and no path
    /// is both a file and a directory. [`Error::Format`] otherwise.
    pub(super) fn check_d1(&self) -> Result<()> {
        if !self.dirs.contains_key("") {
            return Err(Error::Format);
        }
        let dirs_ok = self
            .dirs
            .keys()
            .filter(|path| !path.is_empty())
            .all(|path| self.dirs.contains_key(parent_of(path)));
        let files_ok = self.names.keys().all(|path| {
            self.dirs.contains_key(parent_of(path)) && !self.dirs.contains_key(path.as_str())
        });
        if dirs_ok && files_ok {
            Ok(())
        } else {
            Err(Error::Format)
        }
    }

    /// What a checkpoint keeps of this shape that history never holds.
    pub(super) fn locals(&self) -> TreeLocals {
        TreeLocals {
            dirs: self
                .dirs
                .iter()
                .map(|(path, state)| (path.clone(), (state.number, state.times)))
                .collect(),
            files: self
                .names
                .values()
                .chain(self.anonymous.iter())
                .filter_map(|object| {
                    Some((self.identities.get(object)?.0, *self.times.get(object)?))
                })
                .collect(),
        }
    }

    /// Restores what [`Shape::locals`] kept, on a shape read from history:
    /// each directory's number and times, each file's times.
    /// [`Error::Format`] for a directory `locals` lacks.
    pub(super) fn restore_locals(&mut self, locals: &TreeLocals) -> Result<()> {
        let dirs: Vec<(String, DirState)> = self
            .dirs
            .iter()
            .map(|(path, state)| {
                let (number, times) = *locals.dirs.get(path).ok_or(Error::Format)?;
                Ok((
                    path.clone(),
                    DirState {
                        number,
                        times,
                        ..state.clone()
                    },
                ))
            })
            .collect::<Result<_>>()?;
        self.dirs.clear();
        for (path, state) in dirs {
            self.dirs.insert(path, state);
        }
        let objects: Vec<u64> = self.names.values().copied().collect();
        for object in objects {
            let Some(id) = self.identities.get(&object) else {
                continue;
            };
            if let Some(times) = locals.files.get(&id.0) {
                self.times.insert(object, *times);
            }
        }
        Ok(())
    }

    /// Installs `dirs`, each directory's metadata, as this shape's
    /// directories: numbers and times wait for [`Shape::restore_locals`].
    fn install_dirs(&mut self, dirs: &BTreeMap<String, Meta>) {
        self.dirs.clear();
        for (path, meta) in dirs {
            self.dirs.insert(
                path.clone(),
                DirState {
                    number: 0,
                    meta: *meta,
                    times: Times::default(),
                    subdirs: 0,
                },
            );
        }
        let parents: Vec<String> = dirs
            .keys()
            .filter(|path| !path.is_empty())
            .map(|path| parent_of(path).to_owned())
            .collect();
        for parent in parents {
            if let Some(state) = self.dirs.get_mut(&parent) {
                state.subdirs += 1;
            }
        }
    }
}

/// The actor's saved state of one tree: its shape and each object's saved
/// map. No line map and no name history: those are the builder's.
#[derive(Clone, Default)]
pub(super) struct SavedTree {
    pub(super) shape: Shape,
    pub(super) bytes: BTreeMap<u64, FileMap>,
}

impl Deref for SavedTree {
    type Target = Shape;

    fn deref(&self) -> &Shape {
        &self.shape
    }
}

impl SavedTree {
    /// The saved state a built tree shows: its maps shared, never copied.
    pub(super) fn of(tree: &Tree) -> Self {
        Self {
            shape: tree.shape.clone(),
            bytes: tree
                .bytes
                .iter()
                .map(|(object, bytes)| (*object, (**bytes).clone()))
                .collect(),
        }
    }

    pub(super) fn add_anonymous(&mut self, object: u64, meta: Meta, at_ns: u64) {
        self.shape.add_anonymous(object, meta, at_ns);
        self.bytes.insert(object, FileMap::empty());
    }

    pub(super) fn drop_object(&mut self, object: u64) {
        self.shape.drop_object(object);
        self.bytes.remove(&object);
    }
}

/// The builder's state of one tree: its shape, bytes, line maps and name
/// histories. A step builds on a copy and installs it after its commit is
/// built, so file bytes and line maps sit behind `Arc`.
#[derive(Clone, Default)]
pub(super) struct Tree {
    pub(super) shape: Shape,
    pub(super) bytes: BTreeMap<u64, Arc<FileBytes>>,
    pub(super) lines: BTreeMap<u64, Arc<Lines>>,
    pub(super) namespace: BTreeMap<MergeFileIdentity, NameHistory>,
}

impl Deref for Tree {
    type Target = Shape;

    fn deref(&self) -> &Shape {
        &self.shape
    }
}

impl Tree {
    /// Adds an object that has no name and no saved state.
    pub(super) fn add_anonymous(&mut self, object: u64, meta: Meta, at_ns: u64) {
        self.shape.add_anonymous(object, meta, at_ns);
        self.bytes.insert(object, Arc::default());
        self.lines.insert(object, Arc::default());
    }

    pub(super) fn drop_object(&mut self, object: u64) {
        self.shape.drop_object(object);
        self.bytes.remove(&object);
        self.lines.remove(&object);
    }

    /// Replaces the names, directories and files with `snapshot`'s, each
    /// file's map from `map` (given its identity, object and path). Two files
    /// on one object would read each other's bytes, so a view could differ
    /// from its commit: a step that would do it fails before its record
    /// (finding 39). Directory numbers and times wait for
    /// [`Shape::restore_locals`].
    pub(super) fn install(
        &mut self,
        snapshot: referee::Snapshot,
        mut map: impl FnMut(MergeFileIdentity, u64, &str) -> Result<Arc<FileBytes>>,
    ) -> Result<()> {
        self.shape.names.clear();
        self.shape.fresh_names.clear();
        self.shape.install_dirs(&snapshot.dirs);
        let mut objects = BTreeSet::new();
        for (id, file) in snapshot.files {
            if !objects.insert(file.object) {
                return Err(Error::Format);
            }
            let history = snapshot.names.get(&id).ok_or(Error::Format)?;
            let path = history.path()?.ok_or(Error::Format)?;
            if history.head_version()?.over.is_none() {
                self.shape.fresh_names.insert(file.object);
            }
            self.shape.names.insert(path.to_owned(), file.object);
            self.bytes.insert(file.object, map(id, file.object, path)?);
            self.lines.insert(file.object, Arc::new(file.lines));
            self.shape.identities.insert(file.object, id);
            self.shape.meta.insert(file.object, file.meta);
            self.shape.times.entry(file.object).or_default();
        }
        self.namespace = snapshot.names;
        self.shape.check_d1()
    }

    /// The tree as the referee reads it: each named file's object, lines and
    /// metadata, every name history and each directory's metadata.
    pub(super) fn snapshot(&self) -> referee::Snapshot {
        let files = self
            .names
            .values()
            .map(|object| {
                (
                    self.identities[object],
                    referee::File {
                        object: *object,
                        lines: (*self.lines[object]).clone(),
                        meta: self.meta[object],
                    },
                )
            })
            .collect();
        referee::Snapshot {
            files,
            names: self.namespace.clone(),
            dirs: self
                .dirs
                .iter()
                .map(|(path, state)| (path.clone(), state.meta))
                .collect(),
        }
    }
}

// Merge identity follows an atomic replacement; the object number follows an
// open handle. Keep the types distinct even though both are allocated from
// the same durable event sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct MergeFileIdentity(pub(super) u64);

/// A file's name versions, including deletion, travel in the same commit as
/// its content. Each history is keyed by event number, which is volume-wide.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct NameHistory {
    pub(super) head: EventNumber,
    pub(super) versions: NameVersions,
}

/// A name history's versions by event number, in order: a vector, because
/// most files have one version and a map's node costs hundreds of bytes for
/// it. Stored as the map it stands for, so the commit's bytes are the same.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct NameVersions(Vec<(EventNumber, NameVersion)>);

impl NameVersions {
    pub(super) fn get(&self, event: &EventNumber) -> Option<&NameVersion> {
        let at = self.0.binary_search_by_key(event, |(e, _)| *e).ok()?;
        Some(&self.0[at].1)
    }

    /// Records `version` at `event`, replacing the one there.
    pub(super) fn insert(&mut self, event: EventNumber, version: NameVersion) {
        match self.0.binary_search_by_key(&event, |(e, _)| *e) {
            Ok(at) => self.0[at].1 = version,
            Err(at) => self.0.insert(at, (event, version)),
        }
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&EventNumber, &NameVersion)> {
        self.0.iter().map(|(e, v)| (e, v))
    }

    #[cfg(test)]
    pub(super) fn keys(&self) -> impl Iterator<Item = &EventNumber> {
        self.0.iter().map(|(e, _)| e)
    }
}

impl Serialize for NameVersions {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_map(self.iter())
    }
}

impl<'de> Deserialize<'de> for NameVersions {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let map = BTreeMap::<EventNumber, NameVersion>::deserialize(deserializer)?;
        Ok(Self(map.into_iter().collect()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct NameVersion {
    pub(super) path: Option<String>,
    pub(super) over: Option<EventNumber>,
    pub(super) replaced_by: Option<MergeFileIdentity>,
}

impl NameHistory {
    pub(super) fn created(event: EventNumber, path: String) -> Self {
        Self {
            head: event,
            versions: NameVersions(vec![(
                event,
                NameVersion {
                    path: Some(path),
                    over: None,
                    replaced_by: None,
                },
            )]),
        }
    }

    pub(super) fn advance(
        &mut self,
        event: EventNumber,
        path: Option<String>,
        replaced_by: Option<MergeFileIdentity>,
    ) {
        self.versions.insert(
            event,
            NameVersion {
                path,
                over: Some(self.head),
                replaced_by,
            },
        );
        self.head = event;
    }

    /// The head's version: [`Error::Format`] if it is not on record.
    pub(super) fn head_version(&self) -> Result<&NameVersion> {
        self.versions.get(&self.head).ok_or(Error::Format)
    }

    /// The head's path: None once the file is ended.
    pub(super) fn path(&self) -> Result<Option<&str>> {
        Ok(self.head_version()?.path.as_deref())
    }

    /// Whether the head ends the file: an unlink or a rename-over.
    pub(super) fn is_ended(&self) -> Result<bool> {
        Ok(self.path()?.is_none())
    }

    /// Whether this history's head is not `base`'s: the change it belongs to moved the name.
    /// A file `base` has no history of moved.
    pub(super) fn moved_since(&self, base: Option<&NameHistory>) -> bool {
        base.is_none_or(|old| old.head != self.head)
    }

    /// Both histories' versions, keeping `self.head`; [`Error::Format`] where they record one
    /// version differently.
    pub(super) fn combine(&self, other: &NameHistory) -> Result<NameHistory> {
        let mut combined = self.clone();
        for (event, version) in other.versions.iter() {
            match combined.versions.get(event) {
                Some(existing) if existing != version => return Err(Error::Format),
                Some(_) => {}
                None => {
                    combined.versions.insert(*event, version.clone());
                }
            }
        }
        Ok(combined)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taken_directory_numbers_never_wrap() {
        let step = EventNumber::new(7);
        let last = taken_dir_number(step, TAKEN_DIRS - 1).unwrap();
        assert_ne!(last, taken_dir_number(step, 0).unwrap());
        assert!(matches!(
            taken_dir_number(step, TAKEN_DIRS),
            Err(Error::TooLarge)
        ));
        assert!(matches!(
            taken_dir_number(EventNumber::new(1 << 43), 0),
            Err(Error::TooLarge)
        ));
    }

    #[test]
    fn names_index_each_object_by_its_paths() {
        let mut names = Names::default();
        names.insert("b".into(), 2);
        names.insert("a".into(), 2);
        names.insert("c".into(), 3);
        assert_eq!(names.of(2).collect::<Vec<_>>(), ["a", "b"]);
        // Renaming over a name moves it to the new object.
        assert_eq!(names.insert("a".into(), 3), Some(2));
        assert_eq!(names.of(2).collect::<Vec<_>>(), ["b"]);
        assert_eq!(names.of(3).collect::<Vec<_>>(), ["a", "c"]);
        assert_eq!(names.remove("b"), Some(2));
        assert_eq!(names.of(2).count(), 0);
        assert_eq!(names.of(1).count(), 0);
    }

    #[test]
    fn name_versions_keep_event_order() {
        let mut name = NameHistory::created(EventNumber::new(5), "a".into());
        name.advance(EventNumber::new(9), Some("b".into()), None);
        let mut older = NameHistory::created(EventNumber::new(5), "a".into());
        older.advance(EventNumber::new(7), None, None);
        let both = name.combine(&older).unwrap();
        assert_eq!(
            both.versions.keys().map(|e| e.get()).collect::<Vec<_>>(),
            [5, 7, 9]
        );
        let stored = serde_json::to_string(&both).unwrap();
        assert_eq!(serde_json::from_str::<NameHistory>(&stored).unwrap(), both);
    }
    use crate::tree_actor::referee::{Collision, CollisionKind, Version};
    use crate::tree_reconcile::Line;

    fn v(text: &str) -> LineVersion {
        text.parse().unwrap()
    }

    #[test]
    fn the_stored_text_is_unchanged() {
        let lines = Lines::from_expanded(&crate::tree_reconcile::Expanded {
            lines: vec![
                Line {
                    identity: v("9:1"),
                    version: v("9:1"),
                    live: true,
                    open: None,
                },
                Line {
                    identity: v("9:2"),
                    version: v("10:1"),
                    live: false,
                    open: None,
                },
            ],
            ancestors: [
                (v("9:1"), None),
                (v("9:2"), None),
                (v("10:1"), Some(v("9:2"))),
            ]
            .into_iter()
            .collect(),
        })
        .unwrap();
        let stored = r#"{"lines":[{"identity":"9:1","version":"9:1","count":1},{"identity":"9:2","version":"10:1","count":1,"dead":true}],"ancestors":[{"version":"9:1","count":2},{"version":"10:1","count":1,"over":"9:2"}]}"#;
        assert_eq!(String::from_utf8(lines.encode().unwrap()).unwrap(), stored);
        assert_eq!(Lines::decode(stored.as_bytes()).unwrap(), lines);

        let mut name = NameHistory::created(EventNumber::new(9), "a".into());
        name.advance(EventNumber::new(10), Some("b".into()), None);
        assert_eq!(
            serde_json::to_string(&name).unwrap(),
            r#"{"head":10,"versions":{"9":{"path":"a","over":null,"replaced_by":null},"10":{"path":"b","over":9,"replaced_by":null}}}"#
        );

        let collision = Collision {
            lost: Version::Line {
                file: MergeFileIdentity(3),
                version: v("10:1"),
            },
            winner: Version::Name {
                file: MergeFileIdentity(2),
                version: EventNumber::new(9),
            },
            kind: CollisionKind::Line,
        };
        let text = r#"{"lost":{"Line":{"file":3,"version":"10:1"}},"winner":{"Name":{"file":2,"version":9}},"kind":"Line"}"#;
        assert_eq!(serde_json::to_string(&collision).unwrap(), text);
        assert_eq!(serde_json::from_str::<Collision>(text).unwrap(), collision);
    }

    #[test]
    fn line_versions_order_as_their_text() {
        let all: Vec<LineVersion> = (1..=120)
            .flat_map(|event| {
                let mut versions = Versions::of(EventNumber::new(event));
                (1..=12).map(move |_| versions.next())
            })
            .collect();
        for a in &all {
            for b in &all {
                assert_eq!(a.cmp(b), a.to_string().cmp(&b.to_string()), "{a} {b}");
            }
        }
        let max = LineVersion {
            event: EventNumber::new(u64::MAX),
            index: u64::MAX,
        };
        assert_eq!(v(&max.to_string()), max);
        assert_eq!(max.cmp(&v("9:1")), max.to_string().cmp(&"9:1".to_owned()));
    }

    #[test]
    fn only_canonical_text_parses() {
        for text in [
            "r1", "05:1", "+5:1", "5:0", "5", "5:1:2", ":1", "5:", "5:01", "-5:1",
        ] {
            assert!(text.parse::<LineVersion>().is_err(), "{text}");
        }
        assert_eq!(v("5:1").to_string(), "5:1");
        assert_eq!(v("0:1").to_string(), "0:1");
        assert!(serde_json::from_str::<LineVersion>(r#""05:1""#).is_err());
    }

    #[test]
    fn versions_count_from_one() {
        let mut versions = Versions::of(EventNumber::new(7));
        assert_eq!(versions.next(), v("7:1"));
        assert_eq!(versions.next(), v("7:2"));
        assert_eq!(versions.next().event(), EventNumber::new(7));
        assert_eq!(
            format!("{:020}", EventNumber::new(7)),
            "00000000000000000007"
        );
    }

    #[test]
    fn combine_keeps_its_head_and_refuses_a_version_recorded_twice_differently() {
        let mut ours = NameHistory::created(EventNumber::new(1), "a".into());
        let mut theirs = ours.clone();
        ours.advance(EventNumber::new(5), Some("b".into()), None);
        theirs.advance(EventNumber::new(6), Some("c".into()), None);
        let combined = ours.combine(&theirs).unwrap();
        assert_eq!(combined.head, EventNumber::new(5));
        assert_eq!(
            combined
                .versions
                .keys()
                .map(|e| e.get())
                .collect::<Vec<_>>(),
            [1, 5, 6]
        );

        let mut other = NameHistory::created(EventNumber::new(1), "a".into());
        other.advance(EventNumber::new(5), Some("elsewhere".into()), None);
        assert!(matches!(ours.combine(&other), Err(Error::Format)));
    }

    #[test]
    fn a_name_is_ended_by_a_head_with_no_path_and_moved_by_a_new_head() {
        let created = NameHistory::created(EventNumber::new(1), "a".into());
        assert!(!created.is_ended().unwrap());
        assert!(created.moved_since(None));
        assert!(!created.moved_since(Some(&created)));
        let mut unlinked = created.clone();
        unlinked.advance(EventNumber::new(2), None, None);
        assert!(unlinked.is_ended().unwrap());
        assert!(unlinked.moved_since(Some(&created)));
    }
}
