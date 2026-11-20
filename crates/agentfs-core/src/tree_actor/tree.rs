//! A tree's shared types: the event numbers a journal gives steps, and the line versions they name
//! (docs/target-architecture/git-model.md §5, §7, §12).

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::{Bound, Deref};
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

/// One shared path string: every index of a tree holds the same allocation,
/// and so do the actor's overlay and the builder's shape it rests on. Orders,
/// compares and hashes as its text.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct PathName(Arc<str>);

impl Borrow<str> for PathName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl Deref for PathName {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PathName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for PathName {
    fn from(path: &str) -> Self {
        Self(Arc::from(path))
    }
}

impl From<String> for PathName {
    fn from(path: String) -> Self {
        Self(Arc::from(path))
    }
}

/// What one changed entry of the overlay holds in memory, measured
/// (`tests/tree_overlay_limit.rs` holds it to the allocation a create's
/// entries make): [`SavedTree::overlay_bytes`]'s unit.
#[allow(dead_code, reason = "frozen interface of agentfs-ln6.13")]
pub(super) const OVERLAY_ENTRY_BYTES: u64 = 256;

/// The step an overlay entry is noted at before its step is known: a save,
/// a map or an object dropped, none of which is given its step. Sealed to the
/// actor's last record at a built event ([`SavedTree::seal`]), so it is never
/// dropped before the build that covers it.
const UNSEALED: EventNumber = EventNumber(u64::MAX);

/// The overlay's changed layer of one index: each key this actor's steps
/// changed since the built point the index rests on, with the last step that
/// changed it. `None` for the builder's own shapes, which rest on nothing.
#[derive(Clone, Debug)]
struct Changed<K>(Option<BTreeMap<K, EventNumber>>);

impl<K> Default for Changed<K> {
    fn default() -> Self {
        Self(None)
    }
}

impl<K: Ord + Clone> Changed<K> {
    fn tracking() -> Self {
        Self(Some(BTreeMap::new()))
    }

    /// `key` changed at `step`: steps come in order, so the last noted is
    /// the latest.
    fn note(&mut self, key: &K, step: EventNumber) {
        if let Some(changed) = &mut self.0 {
            changed.insert(key.clone(), step);
        }
    }

    /// Every entry noted at no known step is noted at `last`.
    fn seal(&mut self, last: EventNumber) {
        for step in self.0.iter_mut().flat_map(BTreeMap::values_mut) {
            if *step == UNSEALED {
                *step = last;
            }
        }
    }

    /// The keys changed after `through`, with their steps: what a build
    /// through `through` does not hold yet.
    fn after(&self, through: EventNumber) -> impl Iterator<Item = (&K, EventNumber)> {
        self.0
            .iter()
            .flatten()
            .filter(move |(_, step)| **step > through)
            .map(|(key, step)| (key, *step))
    }
}

/// A tree's names, by path and by object: a view's lookups by name and by
/// inode cost one map lookup, not a scan of the tree. Reads go through
/// `Deref` to the map by path; every change goes through these methods, so
/// the two maps cannot disagree. Persistent maps: a copy shares every node
/// and costs O(1), a change copies only its path. The actor's names are an
/// overlay: the builder's names at the built point, with each path changed
/// since laid over them ([`SavedTree::rebuilt`]).
#[derive(Clone, Debug, Default)]
pub(super) struct Names {
    by_path: imbl::OrdMap<PathName, u64>,
    /// One entry per (object, path), not a set per object: a set's node costs
    /// hundreds of bytes for the one path most objects have.
    by_object: imbl::OrdSet<(u64, PathName)>,
    changed: Changed<PathName>,
}

impl Names {
    pub(super) fn insert(&mut self, path: PathName, object: u64, step: EventNumber) -> Option<u64> {
        self.changed.note(&path, step);
        self.put(path, object)
    }

    pub(super) fn remove(&mut self, path: &str, step: EventNumber) -> Option<u64> {
        let (path, object) = self.by_path.remove_with_key(path)?;
        self.by_object.remove(&(object, path.clone()));
        self.changed.note(&path, step);
        Some(object)
    }

    fn put(&mut self, path: PathName, object: u64) -> Option<u64> {
        let replaced = self.by_path.insert(path.clone(), object);
        if let Some(replaced) = replaced {
            self.by_object.remove(&(replaced, path.clone()));
        }
        self.by_object.insert((object, path));
        replaced
    }

    fn clear(&mut self, step: EventNumber) {
        if self.changed.0.is_some() {
            let paths: Vec<PathName> = self.by_path.keys().cloned().collect();
            for path in &paths {
                self.changed.note(path, step);
            }
        }
        self.by_path.clear();
        self.by_object.clear();
    }

    /// The paths naming `object`, in path order.
    pub(super) fn of(&self, object: u64) -> impl Iterator<Item = &str> {
        self.by_object
            .range((object, PathName::from(""))..)
            .take_while(move |(o, _)| *o == object)
            .map(|(_, path)| &**path)
    }

    /// `built`'s names with every path this overlay changed after `through`
    /// as this overlay has it.
    fn rebuilt(&self, built: &Self, through: EventNumber) -> Self {
        let mut next = Self {
            by_path: built.by_path.clone(),
            by_object: built.by_object.clone(),
            changed: Changed::tracking(),
        };
        for (path, step) in self.changed.after(through) {
            match self.by_path.get(path) {
                Some(object) => {
                    next.put(path.clone(), *object);
                }
                None => {
                    if let Some(object) = next.by_path.remove(path) {
                        next.by_object.remove(&(object, path.clone()));
                    }
                }
            }
            next.changed.note(path, step);
        }
        next
    }
}

impl Deref for Names {
    type Target = imbl::OrdMap<PathName, u64>;

    fn deref(&self) -> &imbl::OrdMap<PathName, u64> {
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
/// Persistent maps and an overlay, as [`Names`].
#[derive(Clone, Debug, Default)]
pub(super) struct Dirs {
    by_path: imbl::OrdMap<PathName, DirState>,
    by_number: imbl::OrdMap<u64, PathName>,
    changed: Changed<PathName>,
}

impl Dirs {
    fn insert(&mut self, path: PathName, state: DirState, step: EventNumber) {
        self.changed.note(&path, step);
        self.put(path, state);
    }

    fn put(&mut self, path: PathName, state: DirState) {
        if let Some(old) = self.by_path.remove(&path) {
            self.unnumber(old.number, &path);
        }
        if !path.is_empty() {
            self.by_number.insert(state.number, path.clone());
        }
        self.by_path.insert(path, state);
    }

    /// Drops `number`'s entry while it names `path`: a directory renamed in
    /// an overlay may already be numbered at its new path.
    fn unnumber(&mut self, number: u64, path: &str) {
        if self.by_number.get(&number).is_some_and(|at| &**at == path) {
            self.by_number.remove(&number);
        }
    }

    fn remove(&mut self, path: &str, step: EventNumber) -> Option<DirState> {
        let (path, state) = self.by_path.remove_with_key(path)?;
        self.unnumber(state.number, &path);
        self.changed.note(&path, step);
        Some(state)
    }

    /// The directory's state, to change all but its number.
    fn get_mut(&mut self, path: &str, step: EventNumber) -> Option<&mut DirState> {
        if self.changed.0.is_some() {
            let (key, _) = self.by_path.get_key_value(path)?;
            let key = key.clone();
            self.changed.note(&key, step);
        }
        self.by_path.get_mut(path)
    }

    /// The path of directory `number`; never the root's.
    pub(super) fn path_of(&self, number: u64) -> Option<&str> {
        self.by_number.get(&number).map(|path| &**path)
    }

    fn clear(&mut self, step: EventNumber) {
        if self.changed.0.is_some() {
            let paths: Vec<PathName> = self.by_path.keys().cloned().collect();
            for path in &paths {
                self.changed.note(path, step);
            }
        }
        self.by_path.clear();
        self.by_number.clear();
    }

    /// As [`Names::rebuilt`].
    fn rebuilt(&self, built: &Self, through: EventNumber) -> Self {
        let mut next = Self {
            by_path: built.by_path.clone(),
            by_number: built.by_number.clone(),
            changed: Changed::tracking(),
        };
        for (path, step) in self.changed.after(through) {
            match self.by_path.get(path) {
                Some(state) => next.put(path.clone(), state.clone()),
                None => {
                    if let Some(state) = next.by_path.remove(path) {
                        next.unnumber(state.number, path);
                    }
                }
            }
            next.changed.note(path, step);
        }
        next
    }
}

impl Deref for Dirs {
    type Target = imbl::OrdMap<PathName, DirState>;

    fn deref(&self) -> &imbl::OrdMap<PathName, DirState> {
        &self.by_path
    }
}

/// One object's merge identity, explicit: never "absent means the object".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Identity {
    /// No identity: no merge file is on the object.
    None,
    /// The object's own: the identity numbered as the object.
    Own,
    /// Another's: a fresh name renamed over a file took that file's.
    Merged(MergeFileIdentity),
}

impl Identity {
    /// `object`'s identity `id`, as this type keeps it.
    const fn of(object: u64, id: MergeFileIdentity) -> Self {
        if id.0 == object {
            Self::Own
        } else {
            Self::Merged(id)
        }
    }

    /// The identity this is of `object`, if any.
    const fn of_object(self, object: u64) -> Option<MergeFileIdentity> {
        match self {
            Self::None => None,
            Self::Own => Some(MergeFileIdentity(object)),
            Self::Merged(id) => Some(id),
        }
    }
}

/// One object's state: its metadata, times, whether its name is fresh (made
/// by this tree's own create, never moved since) and its merge identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ObjectState {
    pub(super) meta: Meta,
    pub(super) times: Times,
    pub(super) fresh: bool,
    pub(super) identity: Identity,
}

/// A tree's objects' states by object number: persistent, and an overlay, as
/// [`Names`].
#[derive(Clone, Debug, Default)]
struct Objects {
    map: imbl::OrdMap<u64, ObjectState>,
    changed: Changed<u64>,
}

impl Objects {
    fn get(&self, object: u64) -> Option<&ObjectState> {
        self.map.get(&object)
    }

    fn get_mut(&mut self, object: u64, step: EventNumber) -> Option<&mut ObjectState> {
        let state = self.map.get_mut(&object)?;
        self.changed.note(&object, step);
        Some(state)
    }

    fn insert(&mut self, object: u64, state: ObjectState, step: EventNumber) {
        self.changed.note(&object, step);
        self.map.insert(object, state);
    }

    fn remove(&mut self, object: u64, step: EventNumber) {
        if self.map.remove(&object).is_some() {
            self.changed.note(&object, step);
        }
    }

    /// As [`Names::rebuilt`].
    fn rebuilt(&self, built: &Self, through: EventNumber) -> Self {
        let mut next = Self {
            map: built.map.clone(),
            changed: Changed::tracking(),
        };
        for (object, step) in self.changed.after(through) {
            match self.map.get(object) {
                Some(state) => {
                    next.map.insert(*object, state.clone());
                }
                None => {
                    next.map.remove(object);
                }
            }
            next.changed.note(object, step);
        }
        next
    }
}

/// The actor's saved maps by object: the builder's maps at the built point
/// (`built`, shared, never copied) with each map this actor saved, replaced
/// or dropped since laid over them (`changed`, read first).
#[derive(Clone, Default)]
pub(super) struct Files {
    built: imbl::OrdMap<u64, Arc<FileBytes>>,
    changed: BTreeMap<u64, (EventNumber, Option<FileMap>)>,
}

impl Files {
    pub(super) fn get(&self, object: &u64) -> Option<&FileMap> {
        match self.changed.get(object) {
            Some((_, map)) => map.as_ref(),
            None => self.built.get(object).map(|bytes| &***bytes),
        }
    }

    /// The map, to change: a built one is first copied into the changed
    /// layer (O(1): a map's pieces are shared).
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "the saved maps' API; tests change maps through it"
        )
    )]
    pub(super) fn get_mut(&mut self, object: &u64) -> Option<&mut FileMap> {
        if !self.changed.contains_key(object) {
            let map = (**self.built.get(object)?).clone();
            self.changed.insert(*object, (UNSEALED, Some(map)));
        }
        self.changed_mut(object)
    }

    /// The map if it is in the changed layer: none for one the built layer
    /// gives.
    pub(super) fn changed_mut(&mut self, object: &u64) -> Option<&mut FileMap> {
        let (step, map) = self.changed.get_mut(object)?;
        *step = UNSEALED;
        map.as_mut()
    }

    pub(super) fn insert(&mut self, object: u64, map: FileMap) -> Option<FileMap> {
        let old = self.get(&object).cloned();
        self.changed.insert(object, (UNSEALED, Some(map)));
        old
    }

    /// Drops the map: a dropped map stays in the changed layer, as none,
    /// until a build drops it too.
    pub(super) fn remove(&mut self, object: &u64) -> Option<FileMap> {
        let old = self.get(object).cloned();
        if old.is_some() {
            self.changed.insert(*object, (UNSEALED, None));
        }
        old
    }

    /// The changed layer's maps: the built layer's are the builder's to move.
    pub(super) fn values_mut(&mut self) -> impl Iterator<Item = &mut FileMap> {
        self.changed
            .values_mut()
            .filter_map(|(_, map)| map.as_mut())
    }

    /// Every object's map, in object order.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "the saved maps' API; tests read maps through it")
    )]
    pub(super) fn iter(&self) -> impl Iterator<Item = (&u64, &FileMap)> {
        let mut built = self
            .built
            .iter()
            .filter(|(object, _)| !self.changed.contains_key(object))
            .map(|(object, bytes)| (object, &***bytes))
            .peekable();
        let mut changed = self
            .changed
            .iter()
            .filter_map(|(object, (_, map))| Some((object, map.as_ref()?)))
            .peekable();
        std::iter::from_fn(move || match (built.peek(), changed.peek()) {
            (Some((a, _)), Some((b, _))) if a < b => built.next(),
            (_, Some(_)) => changed.next(),
            (Some(_), None) => built.next(),
            (None, None) => None,
        })
    }

    fn seal(&mut self, last: EventNumber) {
        for (step, _) in self.changed.values_mut() {
            if *step == UNSEALED {
                *step = last;
            }
        }
    }

    /// `built`'s maps with every map changed after `through` as this has it.
    fn rebuilt(&mut self, built: &imbl::OrdMap<u64, Arc<FileBytes>>, through: EventNumber) {
        self.built = built.clone();
        self.changed.retain(|_, (step, _)| *step > through);
    }
}

#[cfg(test)]
impl std::ops::Index<&u64> for Files {
    type Output = FileMap;

    fn index(&self, object: &u64) -> &FileMap {
        self.get(object).expect("a map for the object")
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
    map: &'a imbl::OrdMap<PathName, V>,
    prefix: &'a str,
) -> impl Iterator<Item = (&'a str, &'a V)> + 'a {
    let mut cursor = prefix.to_owned();
    std::iter::from_fn(move || {
        loop {
            let (key, value) = map
                .range::<_, str>((
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
                    return Some((&**key, value));
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
/// its one transition. Each file's and link's state is by object (its
/// [`ObjectState`]), each directory's in its [`DirState`]. Every map is
/// persistent: a shape's copy is O(1), so the builder's batch and the actor's
/// saved state share what neither changed. The actor's shape is an overlay
/// on the builder's ([`SavedTree::rebuilt`]).
#[derive(Clone, Debug, Default)]
pub(super) struct Shape {
    pub(super) names: Names,
    pub(super) dirs: Dirs,
    pub(super) anonymous: imbl::OrdSet<u64>,
    objects: Objects,
}

impl Shape {
    /// Adds an object that has no name.
    pub(super) fn add_anonymous(&mut self, object: u64, meta: Meta, at_ns: u64) {
        self.anonymous.insert(object);
        self.add_object(object, meta, at_ns, UNSEALED);
    }

    /// A new object's state: its own identity, a fresh name.
    fn add_object(&mut self, object: u64, meta: Meta, at_ns: u64, step: EventNumber) {
        let state = ObjectState {
            meta,
            times: Times::at(at_ns),
            fresh: true,
            identity: Identity::Own,
        };
        self.objects.insert(object, state, step);
    }

    /// `object`'s state.
    pub(super) fn object(&self, object: u64) -> Option<&ObjectState> {
        self.objects.get(object)
    }

    /// `object`'s merge identity, if it has one.
    pub(super) fn identity_of(&self, object: u64) -> Option<MergeFileIdentity> {
        self.object(object)?.identity.of_object(object)
    }

    /// Whether `object`'s name is fresh: made by this tree's own create and
    /// never moved since.
    pub(super) fn is_fresh(&self, object: u64) -> bool {
        self.object(object).is_some_and(|state| state.fresh)
    }

    /// Every object whose name is fresh, in order.
    pub(super) fn fresh_objects(&self) -> impl Iterator<Item = u64> {
        self.objects
            .map
            .iter()
            .filter(|(_, state)| state.fresh)
            .map(|(object, _)| *object)
    }

    /// `object`'s metadata: the read the call path and the journal use, so
    /// the per-object state can change shape behind it.
    pub(super) fn meta_of(&self, object: u64) -> Option<&Meta> {
        self.object(object).map(|state| &state.meta)
    }

    /// `object`'s times, as [`Shape::meta_of`].
    pub(super) fn times_of(&self, object: u64) -> Option<&Times> {
        self.object(object).map(|state| &state.times)
    }

    /// The overlay's changed layer, kept from now on: the actor's shape.
    fn tracked(&self) -> Self {
        let mut shape = self.clone();
        shape.names.changed = Changed::tracking();
        shape.dirs.changed = Changed::tracking();
        shape.objects.changed = Changed::tracking();
        shape
    }

    /// As [`SavedTree::rebuilt`]: the anonymous objects are this shape's.
    fn rebuilt(&self, built: &Self, through: EventNumber) -> Self {
        Self {
            names: self.names.rebuilt(&built.names, through),
            dirs: self.dirs.rebuilt(&built.dirs, through),
            anonymous: self.anonymous.clone(),
            objects: self.objects.rebuilt(&built.objects, through),
        }
    }

    /// Every object a path or an object's state names differently here than
    /// in `before`: O(changes) where the two share their maps' nodes, as a
    /// shape and the copy it was changed from do.
    pub(super) fn objects_changed_since(&self, before: &Shape) -> BTreeSet<u64> {
        let mut changed = BTreeSet::new();
        for item in before.names.by_path.diff(&self.names.by_path) {
            match item {
                imbl::ordmap::DiffItem::Add(_, object)
                | imbl::ordmap::DiffItem::Remove(_, object) => {
                    changed.insert(*object);
                }
                imbl::ordmap::DiffItem::Update { old, new } => {
                    changed.insert(*old.1);
                    changed.insert(*new.1);
                }
            }
        }
        for item in before.objects.map.diff(&self.objects.map) {
            let object = match item {
                imbl::ordmap::DiffItem::Add(object, _)
                | imbl::ordmap::DiffItem::Remove(object, _) => object,
                imbl::ordmap::DiffItem::Update { new, .. } => new.0,
            };
            changed.insert(*object);
        }
        changed
    }

    /// Names `object` at `path`, its identity `id`: a test's tree, built
    /// without the steps that would make it.
    #[cfg(test)]
    pub(super) fn name(&mut self, path: &str, object: u64, id: MergeFileIdentity) {
        self.names.insert(path.into(), object, UNSEALED);
        let state = ObjectState {
            meta: super::meta::TEST_FILE,
            times: Times::default(),
            fresh: false,
            identity: Identity::of(object, id),
        };
        self.objects.insert(object, state, UNSEALED);
    }

    /// No object's name is fresh, an unnamed object's included.
    fn clear_fresh(&mut self, step: EventNumber) {
        let fresh: Vec<u64> = self
            .objects
            .map
            .iter()
            .filter(|(_, state)| state.fresh)
            .map(|(object, _)| *object)
            .collect();
        for object in fresh {
            if let Some(state) = self.objects.get_mut(object, step) {
                state.fresh = false;
            }
        }
    }

    fn seal(&mut self, last: EventNumber) {
        self.names.changed.seal(last);
        self.dirs.changed.seal(last);
        self.objects.changed.seal(last);
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
        Some(self.meta_of(*object).map_or(Kind::File, |meta| meta.kind))
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
                let kind = self.meta_of(*object).map_or(Kind::File, |m| m.kind);
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
            .range::<_, str>((Bound::Included(prefix.as_str()), Bound::Unbounded))
            .take_while(|(path, _)| path.starts_with(&prefix))
            .map(|(path, object)| (path.to_string(), *object))
            .collect();
        let dirs = self
            .dirs
            .range::<_, str>((Bound::Included(prefix.as_str()), Bound::Unbounded))
            .take_while(|(path, _)| path.starts_with(&prefix))
            .map(|(path, _)| path.to_string())
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
        if let Some(id) = self.identity_of(object) {
            keys.insert(queue::Key::File(id));
        }
        keys.extend(
            self.paths_of(object)
                .map(|p| queue::Key::Name(p.to_owned())),
        );
    }

    /// Directory `dir`'s entries changed at `at_ns`: its mtime and ctime move.
    fn dir_modified(&mut self, dir: &str, at_ns: u64, step: EventNumber) {
        if let Some(state) = self.dirs.get_mut(dir, step) {
            state.times.modified(at_ns);
        }
    }

    fn object_changed(&mut self, object: u64, at_ns: u64, step: EventNumber) {
        if let Some(state) = self.objects.get_mut(object, step) {
            state.times.changed(at_ns);
        }
    }

    /// Adds a file or link object at `path`, new at `at_ns`.
    fn add_named(&mut self, path: &str, object: u64, meta: Meta, at_ns: u64, step: EventNumber) {
        self.names.insert(path.into(), object, step);
        self.add_object(object, meta, at_ns, step);
        self.dir_modified(parent_of(path), at_ns, step);
    }

    fn add_dir(&mut self, path: &str, number: u64, meta: Meta, at_ns: u64, step: EventNumber) {
        self.dirs.insert(
            path.into(),
            DirState {
                number,
                meta,
                times: Times::at(at_ns),
                subdirs: 0,
            },
            step,
        );
        let parent = parent_of(path);
        if let Some(state) = self.dirs.get_mut(parent, step) {
            state.subdirs += 1;
        }
        self.dir_modified(parent, at_ns, step);
    }

    fn remove_dir(&mut self, path: &str, at_ns: u64, step: EventNumber) -> Option<DirState> {
        let state = self.dirs.remove(path, step)?;
        let parent = parent_of(path);
        if let Some(state) = self.dirs.get_mut(parent, step) {
            state.subdirs = state.subdirs.saturating_sub(1);
        }
        self.dir_modified(parent, at_ns, step);
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
                self.add_named(path, object, *meta, at_ns, step);
                self.keys_of(object, &mut keys);
            }
            Change::Mkdir { path, meta, .. } => {
                self.add_dir(path, step.get(), *meta, at_ns, step);
                keys.insert(queue::Key::Name(path.clone()));
            }
            Change::Rmdir { path, .. } => {
                self.remove_dir(path, at_ns, step);
                keys.insert(queue::Key::Name(path.clone()));
            }
            Change::Link { object, path, .. } => {
                self.anonymous.remove(object);
                self.names.insert(path.as_str().into(), *object, step);
                self.object_changed(*object, at_ns, step);
                self.dir_modified(parent_of(path), at_ns, step);
                self.keys_of(*object, &mut keys);
            }
            Change::Rename { from, to, .. } if from == to => {}
            Change::Rename { from, to, .. } if self.dirs.contains_key(from.as_str()) => {
                self.rename_dir(from, to, at_ns, step, &mut keys);
            }
            Change::Rename { from, to, .. } => {
                let object = self
                    .names
                    .get(from.as_str())
                    .copied()
                    .ok_or(Error::NotFound)?;
                self.keys_of(object, &mut keys);
                let replaced = self.names.get(to.as_str()).copied();
                if let Some(replaced) = replaced {
                    self.keys_of(replaced, &mut keys);
                }
                self.names.remove(from, step);
                let (identity, fresh) = match replaced {
                    Some(replaced) if self.is_fresh(object) => {
                        let id = self.identity_of(replaced).ok_or(Error::Format)?;
                        (Some(Identity::of(object, id)), self.is_fresh(replaced))
                    }
                    _ => (None, false),
                };
                if let Some(state) = self.objects.get_mut(object, step) {
                    state.identity = identity.unwrap_or(state.identity);
                    state.fresh = fresh;
                    state.times.changed(at_ns);
                }
                self.names.insert(to.as_str().into(), object, step);
                self.dir_modified(parent_of(from), at_ns, step);
                self.dir_modified(parent_of(to), at_ns, step);
                keys.insert(queue::Key::Name(to.clone()));
            }
            Change::Unlink { path, .. } => {
                let object = self
                    .names
                    .get(path.as_str())
                    .copied()
                    .ok_or(Error::NotFound)?;
                self.keys_of(object, &mut keys);
                self.names.remove(path, step);
                self.object_changed(object, at_ns, step);
                self.dir_modified(parent_of(path), at_ns, step);
            }
            Change::SetMeta { path, meta, .. } => {
                if let Some(state) = self.dirs.get_mut(path, step) {
                    state.meta = *meta;
                    state.times.changed(at_ns);
                    keys.insert(queue::Key::Name(path.clone()));
                } else {
                    let object = self
                        .names
                        .get(path.as_str())
                        .copied()
                        .ok_or(Error::NotFound)?;
                    if let Some(state) = self.objects.get_mut(object, step) {
                        state.meta = *meta;
                        state.times.changed(at_ns);
                    }
                    self.keys_of(object, &mut keys);
                }
            }
            Change::SetTimes {
                path, atime, mtime, ..
            } => {
                let times = match self.dirs.get_mut(path, step) {
                    Some(state) => &mut state.times,
                    None => {
                        let object = self
                            .names
                            .get(path.as_str())
                            .copied()
                            .ok_or(Error::NotFound)?;
                        &mut self
                            .objects
                            .get_mut(object, step)
                            .ok_or(Error::NotFound)?
                            .times
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
                let object = match self.names.get(path.as_str()).copied() {
                    Some(object) => {
                        if let Some(state) = self.objects.get_mut(object, step) {
                            state.meta = *meta;
                        }
                        object
                    }
                    None => {
                        let object = step.object();
                        self.add_named(path, object, *meta, at_ns, step);
                        object
                    }
                };
                if let Some(state) = self.objects.get_mut(object, step) {
                    state.times.modified(at_ns);
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
    fn rename_dir(
        &mut self,
        from: &str,
        to: &str,
        at_ns: u64,
        step: EventNumber,
        keys: &mut BTreeSet<queue::Key>,
    ) {
        let (files, dirs) = self.descendants(from);
        if self.dirs.contains_key(to) {
            self.remove_dir(to, at_ns, step);
        }
        keys.insert(queue::Key::Name(from.to_owned()));
        keys.insert(queue::Key::Name(to.to_owned()));
        if let Some(mut state) = self.remove_dir(from, at_ns, step) {
            state.times.changed(at_ns);
            let subdirs = state.subdirs;
            self.add_dir(to, state.number, state.meta, at_ns, step);
            if let Some(moved_state) = self.dirs.get_mut(to, step) {
                moved_state.times = state.times;
                moved_state.subdirs = subdirs;
            }
        }
        for dir in dirs {
            if let Some(state) = self.dirs.remove(&dir, step) {
                let new = moved(&dir, from, to);
                keys.insert(queue::Key::Name(dir));
                keys.insert(queue::Key::Name(new.clone()));
                self.dirs.insert(new.into(), state, step);
            }
        }
        for (path, object) in files {
            self.keys_of(object, keys);
            self.names.remove(&path, step);
            let new = moved(&path, from, to);
            keys.insert(queue::Key::Name(new.clone()));
            self.names.insert(new.into(), object, step);
            if let Some(state) = self.objects.get_mut(object, step) {
                state.fresh = false;
            }
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
                if let Some(state) = self.objects.get_mut(object, UNSEALED) {
                    state.times.modified(at_ns);
                }
                self.keys_of(object, &mut keys);
            }
            SaveOutcome::Unchanged | SaveOutcome::OrphanDropped => {}
        }
        keys
    }

    pub(super) fn drop_object(&mut self, object: u64) {
        self.anonymous.remove(&object);
        self.objects.remove(object, UNSEALED);
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
            let id = self.identity_of(*object).ok_or(Error::Format)?;
            files
                .entry(id)
                .or_insert_with(|| (*object, path.to_string(), self.is_fresh(*object)));
        }
        let before: BTreeSet<PathName> = self.names.keys().cloned().collect();
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
                    let mut state = self.object(file.object).cloned().unwrap_or(ObjectState {
                        meta: file.meta,
                        times: Times::at(at_ns),
                        fresh: false,
                        identity: Identity::None,
                    });
                    state.meta = file.meta;
                    state.times.modified(at_ns);
                    self.objects.insert(file.object, state, step);
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
        // No name is fresh but those the take leaves.
        self.names.clear(step);
        self.clear_fresh(step);
        let mut objects = BTreeSet::new();
        for (id, (object, path, fresh)) in files {
            if !objects.insert(object) || path.is_empty() {
                return Err(Error::Format);
            }
            keys.insert(queue::Key::Name(path.clone()));
            self.names.insert(path.into(), object, step);
            let state = self.objects.get_mut(object, step).ok_or(Error::Format)?;
            state.identity = Identity::of(object, id);
            state.fresh = fresh;
        }
        // Directories: parents before children when adding, children
        // before parents when removing.
        let mut dirs: Vec<&(String, Option<Meta>)> = take.dirs.iter().collect();
        dirs.sort_by(|a, b| a.0.cmp(&b.0));
        let mut added = 0;
        for (path, meta) in &dirs {
            let Some(meta) = meta else { continue };
            keys.insert(queue::Key::Name(path.clone()));
            match self.dirs.get_mut(path, step) {
                Some(state) => {
                    state.meta = *meta;
                    state.times.changed(at_ns);
                }
                None => {
                    self.add_dir(path, taken_dir_number(step, added)?, *meta, at_ns, step);
                    added += 1;
                }
            }
        }
        for (path, meta) in dirs.iter().rev() {
            if meta.is_none() {
                keys.insert(queue::Key::Name(path.clone()));
                self.remove_dir(path, at_ns, step).ok_or(Error::Format)?;
            }
        }
        // Entries added or removed: their directories' mtimes move.
        let after: BTreeSet<PathName> = self.names.keys().cloned().collect();
        for path in before.symmetric_difference(&after) {
            self.dir_modified(parent_of(path), at_ns, step);
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
            self.dirs.contains_key(parent_of(path)) && !self.dirs.contains_key(&**path)
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
                .map(|(path, state)| (path.to_string(), (state.number, state.times)))
                .collect(),
            files: self
                .names
                .values()
                .chain(self.anonymous.iter())
                .filter_map(|object| Some((self.identity_of(*object)?.0, *self.times_of(*object)?)))
                .collect(),
        }
    }

    /// Restores what [`Shape::locals`] kept, on a shape read from history:
    /// each directory's number and times, each file's times.
    /// [`Error::Format`] for a directory `locals` lacks.
    pub(super) fn restore_locals(&mut self, locals: &TreeLocals) -> Result<()> {
        let dirs: Vec<(PathName, DirState)> = self
            .dirs
            .iter()
            .map(|(path, state)| {
                let (number, times) = *locals.dirs.get(&**path).ok_or(Error::Format)?;
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
        self.dirs.clear(UNSEALED);
        for (path, state) in dirs {
            self.dirs.insert(path, state, UNSEALED);
        }
        let objects: Vec<u64> = self.names.values().copied().collect();
        for object in objects {
            let Some(id) = self.identity_of(object) else {
                continue;
            };
            if let Some(times) = locals.files.get(&id.0)
                && let Some(state) = self.objects.get_mut(object, UNSEALED)
            {
                state.times = *times;
            }
        }
        Ok(())
    }

    /// Installs `dirs`, each directory's metadata, as this shape's
    /// directories: numbers and times wait for [`Shape::restore_locals`].
    fn install_dirs(&mut self, dirs: &BTreeMap<String, Meta>) {
        self.dirs.clear(UNSEALED);
        for (path, meta) in dirs {
            self.dirs.insert(
                path.as_str().into(),
                DirState {
                    number: 0,
                    meta: *meta,
                    times: Times::default(),
                    subdirs: 0,
                },
                UNSEALED,
            );
        }
        let parents: Vec<String> = dirs
            .keys()
            .filter(|path| !path.is_empty())
            .map(|path| parent_of(path).to_owned())
            .collect();
        for parent in parents {
            if let Some(state) = self.dirs.get_mut(&parent, UNSEALED) {
                state.subdirs += 1;
            }
        }
    }
}

/// The actor's saved state of one tree: its shape and each object's saved
/// map. No line map and no name history: those are the builder's.
///
/// An overlay: the builder's
/// shape and maps at the last built point, shared, with each name,
/// directory, object and map the actor's steps changed since laid over them.
/// A built event moves it onto the new built point ([`SavedTree::rebuilt`]),
/// so the actor holds one copy of what is built, the builder's.
#[derive(Clone, Default)]
pub(super) struct SavedTree {
    pub(super) shape: Shape,
    pub(super) bytes: Files,
}

impl Deref for SavedTree {
    type Target = Shape;

    fn deref(&self) -> &Shape {
        &self.shape
    }
}

impl SavedTree {
    /// The saved state a built tree shows: its shape and maps shared (O(1)),
    /// never copied, with nothing changed over them.
    pub(super) fn of(tree: &Tree) -> Self {
        Self {
            shape: tree.shape.tracked(),
            bytes: Files {
                built: tree.bytes.clone(),
                changed: BTreeMap::new(),
            },
        }
    }

    /// At a built event through `through`: adopt the builder's shape and maps
    /// and drop every changed entry whose step <= through, keeping the rest
    /// over them. O(entries changed since the last built event). Entries noted
    /// at no step must be sealed first ([`SavedTree::seal`]), or they stay.
    pub(super) fn rebuilt(&mut self, built: &Tree, through: EventNumber) {
        self.shape = self.shape.rebuilt(&built.shape, through);
        self.bytes.rebuilt(&built.bytes, through);
    }

    /// Every changed entry noted at no step (a save, a map, a drop) is noted
    /// at `last`, the last record whose step the actor applied: none is newer.
    pub(super) fn seal(&mut self, last: EventNumber) {
        self.shape.seal(last);
        self.bytes.seal(last);
    }

    pub(super) fn add_anonymous(&mut self, object: u64, meta: Meta, at_ns: u64) {
        self.shape.add_anonymous(object, meta, at_ns);
        self.bytes.insert(object, FileMap::empty());
    }

    /// The overlay's memory, estimated: its changed entries (names,
    /// directories, objects and maps changed since the built point) at
    /// [`OVERLAY_ENTRY_BYTES`] each. O(1): each changed layer's length. A
    /// changed map's own pieces are not counted (the lag gate's byte term
    /// covers a save's bytes).
    #[allow(dead_code, reason = "frozen interface of agentfs-ln6.13")]
    pub(super) fn overlay_bytes(&self) -> u64 {
        todo!("agentfs-ln6.13")
    }

    pub(super) fn drop_object(&mut self, object: u64) {
        self.shape.drop_object(object);
        self.bytes.remove(&object);
    }
}

/// The builder's state of one tree: its shape, bytes, line maps and name
/// histories. A batch builds on a copy and keeps it once its commits are
/// built: every map is persistent, so the copy is O(1), and file bytes and
/// line maps sit behind `Arc`.
#[derive(Clone, Default)]
pub(super) struct Tree {
    pub(super) shape: Shape,
    pub(super) bytes: imbl::OrdMap<u64, Arc<FileBytes>>,
    pub(super) lines: imbl::OrdMap<u64, Arc<Lines>>,
    pub(super) namespace: imbl::OrdMap<MergeFileIdentity, NameHistory>,
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
        self.shape.names.clear(UNSEALED);
        self.shape.clear_fresh(UNSEALED);
        self.shape.install_dirs(&snapshot.dirs);
        let mut objects = BTreeSet::new();
        for (id, file) in snapshot.files {
            if !objects.insert(file.object) {
                return Err(Error::Format);
            }
            let history = snapshot.names.get(&id).ok_or(Error::Format)?;
            let path = history.path()?.ok_or(Error::Format)?;
            let fresh = history.head_version()?.over.is_none();
            self.shape.names.insert(path.into(), file.object, UNSEALED);
            self.bytes.insert(file.object, map(id, file.object, path)?);
            self.lines.insert(file.object, Arc::new(file.lines));
            let times = self
                .shape
                .times_of(file.object)
                .copied()
                .unwrap_or_default();
            let state = ObjectState {
                meta: file.meta,
                times,
                fresh,
                identity: Identity::of(file.object, id),
            };
            self.shape.objects.insert(file.object, state, UNSEALED);
        }
        self.namespace = snapshot.names.into_iter().collect();
        self.shape.check_d1()
    }

    /// The tree as the referee reads it: each named file's object, lines and
    /// metadata, every name history and each directory's metadata.
    pub(super) fn snapshot(&self) -> referee::Snapshot {
        let files = self
            .names
            .values()
            .filter_map(|object| {
                let state = self.object(*object)?;
                Some((
                    state.identity.of_object(*object)?,
                    referee::File {
                        object: *object,
                        lines: (*self.lines[object]).clone(),
                        meta: state.meta,
                    },
                ))
            })
            .collect();
        referee::Snapshot {
            files,
            names: self
                .namespace
                .iter()
                .map(|(id, history)| (*id, history.clone()))
                .collect(),
            dirs: self
                .dirs
                .iter()
                .map(|(path, state)| (path.to_string(), state.meta))
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
    use crate::tree_actor::meta::SetTime;

    #[test]
    fn identities_map_to_object_states_and_back() {
        // Today's three cases of an object's entry in a map of identities:
        // none, its own number, another's.
        let object = 7;
        let cases = [
            (None, Identity::None),
            (Some(MergeFileIdentity(7)), Identity::Own),
            (
                Some(MergeFileIdentity(3)),
                Identity::Merged(MergeFileIdentity(3)),
            ),
        ];
        for (entry, identity) in cases {
            let state = entry.map_or(Identity::None, |id| Identity::of(object, id));
            assert_eq!(state, identity, "{entry:?}");
            let mut shape = Shape::default();
            let times = Times::default();
            let meta = super::super::meta::TEST_FILE;
            let state = ObjectState {
                meta,
                times,
                fresh: false,
                identity: state,
            };
            shape.objects.insert(object, state, UNSEALED);
            assert_eq!(shape.identity_of(object), entry, "{identity:?}");
        }
    }

    #[test]
    fn an_object_without_identity_reads_none() {
        // A take's new object before it is named has metadata and times but
        // no identity: it is not its own, and its change touches no file key.
        let mut shape = Shape::default();
        let state = ObjectState {
            meta: super::super::meta::TEST_FILE,
            times: Times::default(),
            fresh: false,
            identity: Identity::None,
        };
        shape.objects.insert(9, state, UNSEALED);
        assert_eq!(shape.identity_of(9), None);
        assert!(shape.keys(9).is_empty());
        assert!(shape.meta_of(9).is_some());
    }

    /// splitmix64: a test's seeded random numbers.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            (z ^ (z >> 31)) % n.max(1)
        }

        fn pick<'a, T>(&mut self, all: &'a [T]) -> Option<&'a T> {
            all.get(usize::try_from(self.below(all.len() as u64)).unwrap())
        }
    }

    const DIR: Meta = Meta {
        kind: Kind::Dir,
        mode: Perm::from_mode(0o755),
        uid: 1000,
        gid: 1000,
    };

    /// One step of the overlay test, as the actor and the builder each run it.
    #[derive(Clone)]
    enum Op {
        Change(Change),
        Save(u64),
        Anonymous,
        Drop(u64),
        Remap(u64),
    }

    /// A saved map of step `seq`.
    fn map_at(seq: u64) -> FileMap {
        let mut map = FileMap::empty();
        map.saved_at(crate::tree_actor::journal::durable::RecordNo::new(seq));
        map
    }

    /// The actor's run of one step on its saved tree, or the oracle's on a
    /// plain shape and map of maps.
    fn run_saved(
        shape: &mut Shape,
        bytes: &mut dyn FnMut(u64, Option<FileMap>),
        op: &Op,
        seq: u64,
    ) {
        let at_ns = seq * 10;
        match op {
            Op::Change(change) => {
                shape.apply(change, EventNumber::new(seq), at_ns).unwrap();
                if matches!(change, Change::Create { .. } | Change::Symlink { .. }) {
                    bytes(seq, Some(map_at(seq)));
                }
            }
            Op::Save(object) => {
                let outcome = SaveOutcome::Saved { len: 0, pieces: 0 };
                shape.save(*object, outcome, at_ns);
                bytes(*object, Some(map_at(seq)));
            }
            Op::Anonymous => {
                shape.add_anonymous(seq, super::super::meta::TEST_FILE, at_ns);
                bytes(seq, Some(FileMap::empty()));
            }
            Op::Drop(object) => {
                shape.save(*object, SaveOutcome::AnonymousDropped, at_ns);
                shape.drop_object(*object);
                bytes(*object, None);
            }
            Op::Remap(object) => bytes(*object, Some(map_at(seq))),
        }
    }

    /// A random step the oracle's tree takes.
    fn random_op(rng: &mut Rng, shape: &Shape) -> Option<Op> {
        let dirs: Vec<String> = shape.dirs.keys().map(|p| p.to_string()).collect();
        let files: Vec<(String, u64)> = shape
            .names
            .iter()
            .map(|(p, o)| (p.to_string(), *o))
            .collect();
        let anonymous: Vec<u64> = shape.anonymous.iter().copied().collect();
        let path = |rng: &mut Rng| {
            let dir = rng.pick(&dirs).cloned().unwrap_or_default();
            let name = format!("n{}", rng.below(5));
            if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            }
        };
        let tree = "root".to_owned();
        let existing = |rng: &mut Rng| -> Option<String> {
            if rng.below(2) == 0 {
                rng.pick(&files).map(|(p, _)| p.clone())
            } else {
                rng.pick(&dirs).cloned()
            }
        };
        let change = match rng.below(14) {
            0 | 1 => Change::Create {
                tree,
                path: path(rng),
                meta: super::super::meta::TEST_FILE,
            },
            2 => Change::Symlink {
                tree,
                path: path(rng),
                meta: Meta {
                    kind: Kind::Symlink,
                    ..super::super::meta::TEST_FILE
                },
            },
            3 => Change::Mkdir {
                tree,
                path: path(rng),
                meta: DIR,
            },
            4 => Change::Rmdir {
                tree,
                path: rng.pick(&dirs)?.clone(),
            },
            5 => Change::Unlink {
                tree,
                path: rng.pick(&files)?.0.clone(),
            },
            6 | 7 => Change::Rename {
                tree,
                from: existing(rng)?,
                to: if rng.below(3) == 0 {
                    existing(rng)?
                } else {
                    path(rng)
                },
            },
            8 => {
                let path = existing(rng)?;
                let kind = shape.kind_at(&path)?;
                Change::SetMeta {
                    tree,
                    path,
                    meta: Meta {
                        kind,
                        mode: Perm::from_mode(0o600 + rng.below(0o100) as u32),
                        uid: 1000,
                        gid: 1000,
                    },
                }
            }
            9 => Change::SetTimes {
                tree,
                path: existing(rng)?,
                atime: Some(SetTime::At(rng.below(1000))),
                mtime: (rng.below(2) == 0).then_some(SetTime::Now),
            },
            10 => Change::Link {
                tree,
                object: *rng.pick(&anonymous)?,
                path: path(rng),
            },
            11 => return Some(Op::Save(rng.pick(&files)?.1)),
            12 => {
                return Some(if rng.below(2) == 0 || anonymous.is_empty() {
                    Op::Anonymous
                } else {
                    Op::Drop(*rng.pick(&anonymous)?)
                });
            }
            _ => return Some(Op::Remap(rng.pick(&files)?.1)),
        };
        shape.check(&change).ok()?;
        Some(Op::Change(change))
    }

    /// Every read the call path makes of a saved tree, the actor's and the
    /// oracle's, equal.
    fn assert_reads_equal(
        actor: &SavedTree,
        oracle: &Shape,
        bytes: &BTreeMap<u64, FileMap>,
        at: &str,
    ) {
        let names = |s: &Shape| -> Vec<(String, u64)> {
            s.names.iter().map(|(p, o)| (p.to_string(), *o)).collect()
        };
        assert_eq!(names(actor), names(oracle), "names, {at}");
        let dirs = |s: &Shape| -> Vec<(String, DirState)> {
            s.dirs
                .iter()
                .map(|(p, d)| (p.to_string(), d.clone()))
                .collect()
        };
        assert_eq!(dirs(actor), dirs(oracle), "dirs, {at}");
        assert_eq!(actor.anonymous, oracle.anonymous, "anonymous, {at}");
        let objects: BTreeSet<u64> = oracle
            .objects
            .map
            .keys()
            .chain(actor.objects.map.keys())
            .copied()
            .collect();
        for object in objects {
            assert_eq!(
                actor.object(object),
                oracle.object(object),
                "{object}, {at}"
            );
            assert_eq!(actor.meta_of(object), oracle.meta_of(object), "{at}");
            assert_eq!(actor.times_of(object), oracle.times_of(object), "{at}");
            assert!(actor.paths_of(object).eq(oracle.paths_of(object)), "{at}");
            assert_eq!(actor.keys(object), oracle.keys(object), "{at}");
        }
        for (path, state) in oracle.dirs.iter() {
            assert_eq!(
                actor.children(path, 0, usize::MAX),
                oracle.children(path, 0, usize::MAX),
                "children of {path:?}, {at}"
            );
            if !path.is_empty() {
                assert_eq!(actor.dirs.path_of(state.number), Some(&**path), "{at}");
            }
        }
        for path in oracle.names.keys().chain(oracle.dirs.keys()) {
            assert_eq!(actor.kind_at(path), oracle.kind_at(path), "{path}, {at}");
            assert_eq!(actor.names.get(&**path), oracle.names.get(&**path), "{at}");
        }
        let actor_maps: Vec<(u64, Option<u64>)> = actor
            .bytes
            .iter()
            .map(|(o, m)| (*o, m.step().map(|s| s.get())))
            .collect();
        let oracle_maps: Vec<(u64, Option<u64>)> = bytes
            .iter()
            .map(|(o, m)| (*o, m.step().map(|s| s.get())))
            .collect();
        assert_eq!(actor_maps, oracle_maps, "maps, {at}");
        for object in bytes.keys() {
            assert_eq!(
                actor.bytes.get(object).map(FileMap::step),
                bytes.get(object).map(FileMap::step),
                "{at}"
            );
        }
    }

    #[test]
    fn the_overlay_reads_as_the_shape_it_stands_for() {
        for seed in 1..=64 {
            let mut rng = Rng(seed);
            let mut built = Tree::default();
            built
                .shape
                .install_dirs(&BTreeMap::from([(String::new(), DIR)]));
            let mut oracle = built.shape.clone();
            let mut oracle_bytes: BTreeMap<u64, FileMap> = BTreeMap::new();
            let mut actor = SavedTree::of(&built);
            let mut ops: Vec<(u64, Op)> = Vec::new();
            let mut through = 0;
            let mut seq = 0;
            while seq < 300 {
                let Some(op) = random_op(&mut rng, &oracle) else {
                    continue;
                };
                seq += 1;
                run_saved(
                    &mut oracle,
                    &mut |o, map| match map {
                        Some(map) => {
                            oracle_bytes.insert(o, map);
                        }
                        None => {
                            oracle_bytes.remove(&o);
                        }
                    },
                    &op,
                    seq,
                );
                let files = &mut actor.bytes;
                run_saved(
                    &mut actor.shape,
                    &mut |o, map| match map {
                        Some(map) if matches!(op, Op::Remap(_)) => {
                            *files.get_mut(&o).unwrap() = map;
                        }
                        Some(map) => {
                            files.insert(o, map);
                        }
                        None => {
                            files.remove(&o);
                        }
                    },
                    &op,
                    seq,
                );
                ops.push((seq, op));
                if rng.below(6) == 0 {
                    // A built event: the builder runs the steps through a
                    // point at or behind the actor's last.
                    let next = through + rng.below(seq - through + 1);
                    for (step, op) in ops.iter().filter(|(s, _)| *s > through && *s <= next) {
                        let tree = &mut built;
                        let mut maps = Vec::new();
                        run_saved(
                            &mut tree.shape,
                            &mut |o, map| maps.push((o, map)),
                            op,
                            *step,
                        );
                        for (o, map) in maps {
                            match map {
                                Some(map) => tree.bytes.insert(o, Arc::new(map.into())),
                                None => tree.bytes.remove(&o),
                            };
                        }
                    }
                    through = next;
                    actor.seal(EventNumber::new(seq));
                    actor.rebuilt(&built, EventNumber::new(through));
                }
                let at = format!("seed {seed}, step {seq}, built through {through}");
                assert_reads_equal(&actor, &oracle, &oracle_bytes, &at);
            }
        }
    }

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
        let step = EventNumber::new(1);
        names.insert("b".into(), 2, step);
        names.insert("a".into(), 2, step);
        names.insert("c".into(), 3, step);
        assert_eq!(names.of(2).collect::<Vec<_>>(), ["a", "b"]);
        // Renaming over a name moves it to the new object.
        assert_eq!(names.insert("a".into(), 3, step), Some(2));
        assert_eq!(names.of(2).collect::<Vec<_>>(), ["b"]);
        assert_eq!(names.of(3).collect::<Vec<_>>(), ["a", "c"]);
        assert_eq!(names.remove("b", step), Some(2));
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
