//! Recording: a tree's file calls, each journalled (in the kernel, no fsync) before it returns,
//! and the steps that save them: create, close, mkdir, rmdir, symlink, rename, unlink, chmod,
//! chown and times, each decided, appended and applied with no git (docs/recording.md,
//! docs/trees.md; target git-model §7). A file's bytes are a [`FileMap`] over journal frames and
//! the checkout as built, never bytes in memory.
//!
//! A close does bounded work under the actor's lock: when the file's saved
//! map has not moved since the open and only its length changed, it takes
//! the open's view as the save (O(1)); otherwise it snapshots what it needs
//! ([`CloseWork`]), decides with the lock released ([`CloseWork::decide`]),
//! and lands the decision only if the saved map it read is still current,
//! else redoes it once under the lock ([`TreeActor::close_finish`]). Either
//! way its save is recorded before the close returns.

pub(super) mod file_map;
pub(super) mod layout;
pub(super) mod pages;

use std::collections::BTreeSet;
use std::ops::Range;

use agentfs_contracts::clients::FileOwner;
use agentfs_contracts::name::WriterLabel;
use agentfs_contracts::pod::WriterAt;

pub(in crate::tree_actor) use super::journal::CallHead as Call;
use super::journal::durable::RecordNo;
use super::journal::readback::CallReader;
use super::journal::segment::FramePos;
use super::journal::{
    Change, Entry, FoldedOpen, PagePiece, RunHead, SaveOutcome, TAKE_CHUNK, Tail, TakeRecord,
    chunk_map,
};
use super::meta::{Attrs, Kind, Meta, Perm, SetTime, Times};
use super::queue::Key;
use super::stages::{Hold, section};
use super::tree::{DirState, EventNumber, SavedTree};
use super::{Error, Result, SavePending, Saved, TreeActor, TreeId};
use crate::paths::RepoPath;
pub use file_map::FileMap;
use layout::{Layout, Positions};
use pages::CloseCalls;

/// Calls name byte positions, never lines; their bytes are in their frame's
/// tail. Append finds EOF at save time.
impl Call {
    /// The bytes the call carries in its frame.
    pub(in crate::tree_actor) fn carried(&self) -> u64 {
        match self {
            Self::Write { len, .. } | Self::Append { len } => *len,
            Self::Truncate(_) => 0,
        }
    }

    /// The length the call leaves on a file of length `len`, with the errors
    /// [`FileMap::apply`] returns: a call's errors depend only on lengths, so
    /// a check costs nothing like the bytes.
    pub(in crate::tree_actor) fn len_after(&self, len: u64) -> Result<u64> {
        match *self {
            Self::Write { offset, len: n } => {
                let end = offset.checked_add(n).ok_or(Error::TooLarge)?;
                Ok(if n == 0 { len } else { len.max(end) })
            }
            Self::Append { len: n } => len.checked_add(n).ok_or(Error::TooLarge),
            Self::Truncate(size) => Ok(size),
        }
    }

    /// The file range the call changes on a file of length `len`: a write's,
    /// an append's at replay, a truncate's from its size up.
    pub(in crate::tree_actor) fn touched(&self, len: u64) -> Option<Range<u64>> {
        match *self {
            Self::Write { len: 0, .. } | Self::Append { len: 0 } => None,
            Self::Write { offset, len: n } => Some(offset..offset.saturating_add(n)),
            Self::Append { len: n } => Some(len..len.saturating_add(n)),
            Self::Truncate(size) => Some(size..u64::MAX),
        }
    }

    /// Whether a frame's tail of `tail` bytes is this call's.
    pub(in crate::tree_actor) fn check_tail(&self, tail: u64) -> Result<()> {
        if self.carried() == tail {
            Ok(())
        } else {
            Err(Error::Format)
        }
    }
}

/// One entry of a tree as a view presents it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// Its node number: a file's or link's object, a directory's number;
    /// `None` for the root directory. A view's inode is the number + 2.
    pub number: Option<u64>,
    pub meta: Meta,
    pub times: Times,
    /// Bytes: a file's length, a link's target's; 0 for a directory.
    pub len: u64,
    /// 1 for a named file or link, 0 for an anonymous one, 2 plus its child
    /// directories for a directory.
    pub links: u32,
}

/// One entry of a directory listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub number: u64,
    pub kind: Kind,
}

/// Whether a rename may replace an entry at its destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenameMode {
    Replace,
    /// `RENAME_NOREPLACE`: [`Error::Exists`] when the destination exists.
    NoReplace,
}

/// The longest link target, in bytes (Linux's `PATH_MAX` less its NUL).
const MAX_TARGET: usize = 4095;

/// A directory's node.
fn dir_node(path: &str, state: &DirState) -> Node {
    Node {
        number: (!path.is_empty()).then_some(state.number),
        meta: state.meta,
        times: state.times,
        len: 0,
        links: 2 + state.subdirs,
    }
}

/// The length an open's calls leave on a saved length `L`: `max(L + add,
/// at_least)`, or `len` after a truncate. Composed per call in O(1)
/// (max-plus), so a call is checked against the saved head without
/// replaying the open's earlier calls. `grown` is the `add` of the calls
/// before the first truncate: the fold from `L` fails exactly when `L +
/// grown` does not fit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) enum LenRule {
    Shift { add: u64, at_least: u64 },
    Fixed { len: u64, grown: u64 },
}

impl LenRule {
    /// No call: the saved length as it is.
    pub(in crate::tree_actor) const NONE: Self = Self::Shift {
        add: 0,
        at_least: 0,
    };

    /// The rule with `call` after the calls `self` stands for.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] where the call fails whatever the saved length:
    /// its own bounds overflow ([`Call::len_after`]'s errors), or it appends
    /// past `u64` from the least length the calls before it leave.
    pub(in crate::tree_actor) fn then(self, call: &Call) -> Result<Self> {
        Ok(match (self, call.clone()) {
            (rule, Call::Write { len: 0, .. }) => rule,
            (rule, Call::Write { offset, len }) => {
                let end = offset.checked_add(len).ok_or(Error::TooLarge)?;
                match rule {
                    Self::Shift { add, at_least } => Self::Shift {
                        add,
                        at_least: at_least.max(end),
                    },
                    Self::Fixed { len, grown } => Self::Fixed {
                        len: len.max(end),
                        grown,
                    },
                }
            }
            (Self::Shift { add, at_least }, Call::Append { len }) => Self::Shift {
                add: add.checked_add(len).ok_or(Error::TooLarge)?,
                at_least: at_least.checked_add(len).ok_or(Error::TooLarge)?,
            },
            (Self::Fixed { len, grown }, Call::Append { len: n }) => Self::Fixed {
                len: len.checked_add(n).ok_or(Error::TooLarge)?,
                grown,
            },
            (Self::Shift { add, .. }, Call::Truncate(size)) => Self::Fixed {
                len: size,
                grown: add,
            },
            (Self::Fixed { grown, .. }, Call::Truncate(size)) => Self::Fixed { len: size, grown },
        })
    }

    /// The length the calls leave on a saved length of `saved_len`.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] exactly when folding [`Call::len_after`] over the
    /// calls from `saved_len` would fail.
    pub(in crate::tree_actor) fn on(self, saved_len: u64) -> Result<u64> {
        match self {
            Self::Shift { add, at_least } => Ok(saved_len
                .checked_add(add)
                .ok_or(Error::TooLarge)?
                .max(at_least)),
            Self::Fixed { len, grown } => saved_len
                .checked_add(grown)
                .map(|_| len)
                .ok_or(Error::TooLarge),
        }
    }
}

pub(in crate::tree_actor) struct Handle {
    pub(in crate::tree_actor) tree: TreeId,
    pub(in crate::tree_actor) by: WriterAt,
    pub(in crate::tree_actor) object: u64,
    /// The open's view: the saved map at open (a clone, never a copy of
    /// bytes), its own calls and the file's page writes.
    pub(in crate::tree_actor) bytes: FileMap,
    /// The length this open's calls leave on any saved length (see `call`).
    pub(in crate::tree_actor) lengths: LenRule,
    /// Calls this open made; 0 -> its close reads no journal.
    pub(in crate::tree_actor) call_count: u64,
    /// The saved map's step when the view was taken (open, or rebase_built
    /// moving a call-less view).
    pub(in crate::tree_actor) base_step: Option<RecordNo>,
    /// Its close is being decided with the lock released, or waits its turn
    /// behind an earlier-begun save of its file: a pending save that only
    /// that close's finish saves; close_writer and save_pending leave it.
    pub(in crate::tree_actor) closing: bool,
    /// Its close record ([`Entry::CloseBegun`]), once its close has
    /// begun: the record's number is the close's order, the one a file's
    /// saves land in, live and at restart ([`close_rank`]). None only
    /// before a close begins, or for a failed close whose record could not
    /// be appended: its retry appends it first.
    pub(in crate::tree_actor) close_order: Option<RecordNo>,
    /// When its save became pending: the age the metrics give.
    pub(in crate::tree_actor) pending_since: Option<std::time::Instant>,
}

impl Handle {
    /// A new open whose view is `bytes`, taken when the file's saved map
    /// was the save of `base_step`.
    fn new(
        tree: TreeId,
        by: &WriterAt,
        object: u64,
        bytes: FileMap,
        base_step: Option<RecordNo>,
    ) -> Self {
        Self {
            tree,
            by: by.clone(),
            object,
            bytes,
            lengths: LenRule::NONE,
            call_count: 0,
            base_step,
            closing: false,
            close_order: None,
            pending_since: None,
        }
    }

    /// An open recovery found live, before any call is served: a recovery
    /// close saves it at once, replaying its calls read back from the
    /// journal, in the order of its close record when it has one. Its view
    /// is never read, so it has no base.
    pub(in crate::tree_actor) fn recovered(folded: FoldedOpen) -> Self {
        let FoldedOpen { open, calls } = folded;
        let lengths = calls
            .iter()
            .try_fold(LenRule::NONE, |rule, (_, call, _)| rule.then(call))
            .unwrap_or(LenRule::NONE);
        Self {
            lengths,
            call_count: calls.len() as u64,
            close_order: open.close_begun,
            ..Self::new(open.tree, &open.by, open.object, FileMap::empty(), None)
        }
    }
}

/// The one order a file's saves land in, live and at restart: opens with a
/// close record in its order, then those without (an open whose record
/// could not be appended) in open order. Sorts recovery's closes, a step's
/// pending closes, and the saves an open of its own client reads
/// ([`TreeActor::open_content`], [`PendingFold`]).
pub(in crate::tree_actor) fn close_rank(handle: u64, h: &Handle) -> (bool, Option<RecordNo>, u64) {
    (h.close_order.is_none(), h.close_order, handle)
}

/// What a close took under the lock: the saved map and the open's view as
/// shared snapshots (O(1)), the file's page writes, and a reader of the
/// open's calls when the decision replays them. With neither page writes
/// nor a reader, the saved map has not moved since the open and the view is
/// the save to judge.
pub(in crate::tree_actor) struct CloseWork {
    handle: u64,
    tree: TreeId,
    object: u64,
    saved: FileMap,
    saved_step: Option<RecordNo>,
    view: FileMap,
    pages: Option<(Vec<(u64, pages::Run)>, u64)>,
    reader: Option<CallReader>,
}

/// A changed file's save as a close decided it: its layout's chunk records,
/// what the step records before it, and the saved map, the layout laid on
/// the saved map the close read ([`Layout::lay`]), what a replay of the
/// records lays. Made off the lock for a close decided there.
type Decided = (Vec<Vec<u8>>, FileMap);

/// A close decided with the lock released.
pub(in crate::tree_actor) struct CloseDecision {
    work: CloseWork,
    outcome: SaveOutcome,
    map: Option<Decided>,
    pieces: Vec<(u64, pages::Run)>,
}

/// What an open by a client with pending saves of a file reads, taken
/// under the actor's lock and folded with it released
/// ([`PendingFold::fold`]), as a close's decision is: the saved map (a
/// clone, O(1)), the client's pending saves in close order with how many
/// calls each made, the file's page writes and `saved_through`, and a
/// reader of the journal as it stood. Every input is a snapshot: a pending
/// save takes no more calls, so the fold gives the content as of the hold
/// that took it, whatever lands meanwhile (docs/trees.md, "Pending saves").
#[must_use = "a pending fold must be folded, with the actor's lock released"]
pub(crate) struct PendingFold {
    saved: FileMap,
    /// Each pending save's open and its call count, in close order.
    closes: Vec<(u64, u64)>,
    /// The file's page writes and `saved_through`, when it has any.
    pages: Option<(Vec<(u64, pages::Run)>, u64)>,
    reader: CallReader,
}

impl PendingFold {
    /// The fold, from the snapshots alone, reading no actor state: each
    /// pending save's own calls read back from the journal and replayed on
    /// the saved map in close order, each with the page-write pieces its
    /// save takes ([`pages::calls_for_close`]), as the saves will land.
    /// Never called with the actor locked, but by
    /// [`TreeActor::write_open`]'s one hold (`Hold::PageImage`).
    ///
    /// # Errors
    ///
    /// A failed read of the journal; a call that does not apply.
    pub(crate) fn fold(self) -> Result<FileMap> {
        let Self {
            mut saved,
            closes,
            pages,
            reader,
        } = self;
        let (writes, mut through) = pages.unwrap_or_default();
        for (handle, call_count) in closes {
            let own = if call_count > 0 {
                reader.calls(handle)?
            } else {
                Vec::new()
            };
            for call in pages::calls_for_close(saved.len(), &own, &writes, through) {
                saved.apply(call.call(), call.at())?;
            }
            // Each later save lands after a step past every page write so
            // far: it takes only the pieces over its own calls' bytes.
            through = u64::MAX;
        }
        Ok(saved)
    }
}

/// What an open by a file's own client reads, decided under the lock
/// ([`TreeActor::open_content`]).
#[must_use = "a pending fold must be folded, with the actor's lock released"]
pub(crate) enum OpenContent {
    /// The saved map, or the one pending save's view whose base did not
    /// move with no page write: a clone, O(1).
    Ready(FileMap),
    /// Two or more pending saves, a moved base or page writes: folded with
    /// the lock released.
    Fold(PendingFold),
}

/// How a write-open goes on after [`TreeActor::write_open_begin`]: its
/// view is set, or its open is on record and its view waits for the fold
/// of its client's pending saves ([`TreeActor::write_open_view`]).
#[must_use = "a write-open's view must be set from its fold"]
pub(crate) enum WriteOpenStart {
    /// The open, its view set: it may be given to its client.
    Ready(u64),
    /// The open, live with a placeholder view (the saved map, no base), and
    /// the fold its view is set from: not to be given to its client before
    /// [`TreeActor::write_open_view`].
    Fold { handle: u64, fold: PendingFold },
}

/// How a close goes on after [`TreeActor::close_begin`]: saved under the
/// lock already, to be decided with it released, or queued behind an
/// earlier-begun save of its file, its snapshot taken at its turn
/// ([`TreeActor::close_resume`]).
pub(in crate::tree_actor) enum CloseStart {
    Done(Saved),
    Decide(CloseWork),
    Queued,
}

impl CloseWork {
    /// The close's outcome, from the snapshots alone: reads no actor state.
    /// The view itself when the saved map did not move and the file has no
    /// page writes, compared with the saved map only where their sources
    /// differ; else the open's calls, read back from the journal, and the
    /// file's chosen page-write pieces replayed on the saved map.
    ///
    /// # Errors
    ///
    /// A failed read of the journal or of the bytes compared; a call that
    /// does not apply.
    pub(in crate::tree_actor) fn decide(self) -> Result<CloseDecision> {
        if self.reader.is_none() && self.pages.is_none() {
            let ranges = self.view.differs_from(&self.saved);
            let (outcome, map) = judge(&self.saved, self.view.clone(), &ranges, || {
                Ok(Layout::of_view(&self.view, RecordNo::new(self.handle)))
            })?;
            return Ok(CloseDecision {
                work: self,
                outcome,
                map,
                pieces: Vec::new(),
            });
        }
        let own = match &self.reader {
            Some(reader) => reader.calls(self.handle)?,
            None => Vec::new(),
        };
        let pieces = match &self.pages {
            Some((writes, through)) => {
                pages::close_pieces(self.saved.len(), &own, writes, *through)
            }
            None => Vec::new(),
        };
        let calls = pages::close_calls(own.iter(), pieces.clone());
        let (outcome, map) = replay_judge(&self.saved, calls)?;
        Ok(CloseDecision {
            work: self,
            outcome,
            map,
            pieces,
        })
    }
}

/// A close's outcome for `map`, the saved map with the close's calls
/// applied, which may differ from `saved` only over `ranges`: unchanged when
/// the length and those bytes are the same (finding 36; the owner,
/// 2026-11-07, "1. A"), else saved with its `layout`.
fn judge(
    saved: &FileMap,
    map: FileMap,
    ranges: &[Range<u64>],
    layout: impl FnOnce() -> Result<Layout>,
) -> Result<(SaveOutcome, Option<Decided>)> {
    if map.len() == saved.len() && map.same_as(saved, ranges)? {
        return Ok((SaveOutcome::Unchanged, None));
    }
    let layout = layout()?;
    Ok((layout.outcome(), Some(decided(saved, &layout, &map)?)))
}

/// What a save of `layout` records and serves: its chunk records, and the
/// layout laid on `saved`, each place resolved from `computed`, the map the
/// close's calls left.
///
/// # Errors
///
/// As [`Layout::lay`]: a layout its own close cannot lay is a bug.
fn decided(saved: &FileMap, layout: &Layout, computed: &FileMap) -> Result<Decided> {
    let known = Positions::of_map(computed);
    let laid = layout.lay(saved, &mut |place| Ok(known.find(place)))?;
    Ok((layout.to_chunks(), laid))
}

/// `calls` replayed on `saved` and judged: a close that applies nothing
/// changes nothing. "As it was" compares only the bytes the calls touched.
/// The merge is cloned once, before the replay, for [`Layout::of_close`]:
/// the open's calls are read twice where they are held, never copied.
fn replay_judge<'a, O>(
    saved: &FileMap,
    calls: CloseCalls<'a, FramePos, O>,
) -> Result<(SaveOutcome, Option<Decided>)>
where
    O: Iterator<Item = &'a (u64, Call, FramePos)> + Clone,
{
    let for_layout = calls.clone();
    let mut calls = calls.peekable();
    if calls.peek().is_none() {
        return Ok((SaveOutcome::Unchanged, None));
    }
    let mut map = saved.clone();
    let mut ranges = Vec::new();
    for call in calls {
        ranges.extend(call.call().touched(map.len()));
        map.apply(call.call(), call.at())?;
    }
    judge(saved, map, &ranges, || {
        Layout::of_close(saved.len(), for_layout)
    })
}

impl TreeActor {
    /// Saved bytes, whole, in memory: the library's read. An independent
    /// reader never sees an unfinished save. A view reads through
    /// [`TreeActor::file_map`] instead.
    pub fn read(&self, tree: TreeId, path: &str) -> Result<Vec<u8>> {
        Ok(self.file_map(tree, path)?.to_vec()?)
    }

    /// The saved file as a map: a clone of the actor's, O(1), which a reader
    /// reads with the actor unlocked and keeps across later saves and builds.
    pub fn file_map(&self, tree: TreeId, path: &str) -> Result<FileMap> {
        let tree = self.tree(tree)?;
        let object = tree.names.get(path).ok_or(Error::NotFound)?;
        Ok(tree.bytes.get(object).ok_or(Error::NotFound)?.clone())
    }

    /// A file at `path` whose save is being decided, or queued behind an
    /// earlier save of it: the session's refusal of a call that would
    /// delete or replace it (an unlink, a rename onto it, a whole-file
    /// replace, a path truncate). A failed save is no refusal: its retry
    /// saves first at the call's own step.
    pub(crate) fn pending_save(&self, tree: TreeId, path: &str) -> Option<SavePending> {
        let t = self.tree(tree).ok()?;
        let object = *t.names.get(path)?;
        self.save_in_flight(tree, object, None)
            .then(|| SavePending::new(tree, object))
    }

    /// Whether a close of `object` in `tree` is in flight (being decided or
    /// queued), its close record below `before` when given.
    pub(in crate::tree_actor) fn save_in_flight(
        &self,
        tree: TreeId,
        object: u64,
        before: Option<RecordNo>,
    ) -> bool {
        self.handles.values().any(|h| {
            h.closing
                && h.tree == tree
                && h.object == object
                && before.is_none_or(|b| h.close_order.is_some_and(|o| o < b))
        })
    }

    /// Whether a close of the volume is in flight (being decided or queued)
    /// whose close record is below `before`: what `Recording::settle` waits
    /// out, for the closes begun before it.
    pub(in crate::tree_actor) fn saves_in_flight(&self, before: RecordNo) -> bool {
        self.handles
            .values()
            .any(|h| h.closing && h.close_order.is_some_and(|o| o < before))
    }

    /// What an open of `path` by `by` reads, under the lock in O(segments +
    /// the file's page writes): the saved map when `by` has no pending save
    /// of it; one pending save's view whose base did not move, with no page
    /// write, as a clone; else a [`PendingFold`] of `by`'s pending saves in
    /// close order, for the caller to fold with the lock released
    /// (close-to-open, ledger R1.1: another client's pending save is not
    /// shown, nor any open's unsaved calls). Never reads the journal and
    /// never waits.
    ///
    /// # Errors
    ///
    /// As [`TreeActor::file_map`].
    pub(crate) fn open_content(
        &self,
        tree: TreeId,
        path: &str,
        by: &WriterAt,
    ) -> Result<OpenContent> {
        let object = *self.tree(tree)?.names.get(path).ok_or(Error::NotFound)?;
        match self.pending_content(tree, object, by)? {
            Some(content) => Ok(content),
            None => self.file_map(tree, path).map(OpenContent::Ready),
        }
    }

    /// A write-open's first half, under the lock: as
    /// [`TreeActor::write_open`] up to its view. With pending saves of the
    /// file by `by` that need a fold, the open is on record (its `Open`
    /// record appended, its handle live, its view the saved map with no
    /// base until set) and its fold is handed back; the handle is never
    /// given to a client before [`TreeActor::write_open_view`] sets its
    /// view. Never reads the journal.
    ///
    /// # Errors
    ///
    /// As [`TreeActor::write_open`].
    pub(crate) fn write_open_begin(
        &mut self,
        tree: TreeId,
        path: &str,
        by: &WriterAt,
        create: Option<Attrs>,
    ) -> Result<WriteOpenStart> {
        name(path)?;
        if create.is_some() && self.tree(tree)?.kind_at(path).is_none() {
            self.recorded_name(path, Kind::File)?;
        }
        let t = self.tree(tree)?;
        match t.kind_at(path) {
            Some(Kind::Dir) => return Err(Error::IsADirectory),
            Some(Kind::Symlink) => return Err(Error::Invalid),
            Some(Kind::File) | None => {}
        }
        let Some(object) = t.names.get(path).copied() else {
            let Some(attrs) = create else {
                return Err(Error::NotFound);
            };
            return self
                .create_step(tree, path, by, attrs)
                .map(WriteOpenStart::Ready);
        };
        // The writer's own pending saves are its view's start, taken in the
        // hold that records the open: such a view has no base, so its close
        // replays its calls on the saved head and never saves the view.
        let pending = self.pending_content(tree, object, by)?;
        let (handle, _) = self.journal_record(
            Entry::Open {
                tree: tree.to_string(),
                path: path.to_owned(),
                by: by.clone(),
                object,
            },
            &[],
        )?;
        let saved = self
            .tree(tree)?
            .bytes
            .get(&object)
            .cloned()
            .unwrap_or_default();
        match pending {
            Some(OpenContent::Ready(bytes)) => {
                self.handles
                    .insert(handle, Handle::new(tree, by, object, bytes, None));
                Ok(WriteOpenStart::Ready(handle))
            }
            Some(OpenContent::Fold(fold)) => {
                self.handles
                    .insert(handle, Handle::new(tree, by, object, saved, None));
                Ok(WriteOpenStart::Fold { handle, fold })
            }
            None => {
                let base = saved.step();
                self.handles
                    .insert(handle, Handle::new(tree, by, object, saved, base));
                self.pages_opened(tree, object);
                Ok(WriteOpenStart::Ready(handle))
            }
        }
    }

    /// A write-open's second half, under the lock: its view is `folded`,
    /// the fold its [`WriteOpenStart::Fold`] gave, with every page-write run
    /// of the file recorded after its `Open` record applied in record order,
    /// as each was applied to every open view of the file at its call. O(those
    /// runs); no journal read.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an open that is gone or whose close has begun
    /// (its client ended while the fold ran); [`Error::Stopped`].
    pub(crate) fn write_open_view(&mut self, handle: u64, mut folded: FileMap) -> Result<()> {
        if self.is_stopped() {
            return Err(Error::Stopped);
        }
        let h = self
            .handles
            .get(&handle)
            .filter(|h| h.close_order.is_none() && !h.closing)
            .ok_or(Error::NotFound)?;
        if let Some(state) = self.pages.get(&(h.tree, h.object)) {
            for (_, run) in state.writes.iter().filter(|(seq, _)| *seq > handle) {
                // Each run was checked against `i64` at its page write.
                let (call, pos) = run.call();
                let _ = folded.apply(&call, &pos);
            }
        }
        if let Some(h) = self.handles.get_mut(&handle) {
            h.bytes = folded;
        }
        Ok(())
    }

    /// What an open of `object` by `by` reads when `by` has pending saves
    /// of it, or None when it has none (see [`TreeActor::open_content`]):
    /// one pending save whose base did not move, with no page write of the
    /// file, gives its open's view, a clone; more, a moved base or page
    /// writes give a [`PendingFold`] of them in close order
    /// ([`close_rank`]).
    fn pending_content(
        &self,
        tree: TreeId,
        object: u64,
        by: &WriterAt,
    ) -> Result<Option<OpenContent>> {
        let mut mine: Vec<(u64, &Handle)> = self
            .handles
            .iter()
            .filter(|(handle, h)| {
                h.tree == tree
                    && h.object == object
                    && h.by.writer == by.writer
                    && (h.closing || self.pending_closes.contains(handle))
            })
            .map(|(handle, h)| (*handle, h))
            .collect();
        mine.sort_by_key(|(handle, h)| close_rank(*handle, h));
        let [(_, only), ..] = mine.as_slice() else {
            return Ok(None);
        };
        let saved_step = self
            .tree(tree)?
            .bytes
            .get(&object)
            .ok_or(Error::NotFound)?
            .step();
        if mine.len() == 1
            && only.base_step.is_some()
            && only.base_step == saved_step
            && self.pages_written(tree, object).is_none()
        {
            return Ok(Some(OpenContent::Ready(only.bytes.clone())));
        }
        let closes: Vec<u64> = mine.iter().map(|(handle, _)| *handle).collect();
        self.pending_fold(tree, object, &closes)
            .map(|fold| Some(OpenContent::Fold(fold)))
    }

    /// The snapshot a fold of `closes`, pending saves of `object` in
    /// `tree` in close order, reads: the saved map, each one's call count,
    /// the file's page writes and `saved_through`, and a reader of the
    /// journal as it stands. O(segments + the file's page writes); reads no
    /// journal.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when `tree` holds no such object or an open of
    /// `closes` is gone.
    fn pending_fold(&self, tree: TreeId, object: u64, closes: &[u64]) -> Result<PendingFold> {
        let saved = self
            .tree(tree)?
            .bytes
            .get(&object)
            .ok_or(Error::NotFound)?
            .clone();
        let closes = closes
            .iter()
            .map(|handle| {
                let h = self.handles.get(handle).ok_or(Error::NotFound)?;
                Ok((*handle, h.call_count))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(PendingFold {
            saved,
            closes,
            pages: self.pages_writes(tree, object),
            reader: self.journal.call_reader(),
        })
    }

    /// The saved length of `path`.
    pub fn file_len(&self, tree: TreeId, path: &str) -> Result<u64> {
        let tree = self.tree(tree)?;
        let object = tree.names.get(path).ok_or(Error::NotFound)?;
        Ok(tree.bytes.get(object).ok_or(Error::NotFound)?.len())
    }

    /// The entry at `path` (`""` the root directory) as a view presents it.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for no such entry.
    pub fn node(&self, tree: TreeId, path: &str) -> Result<Node> {
        let t = self.tree(tree)?;
        if let Some(state) = t.dirs.get(path) {
            return Ok(dir_node(path, state));
        }
        let object = *t.names.get(path).ok_or(Error::NotFound)?;
        Self::object_node(t, object)
    }

    /// The named file, link or directory numbered `number`, with its path
    /// (the first in path order, for a file).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for anything else: no such number, an anonymous
    /// or orphaned file ([`TreeActor::open_node`]).
    pub fn node_at(&self, tree: TreeId, number: u64) -> Result<(&str, Node)> {
        let t = self.tree(tree)?;
        if let Some(path) = t.dirs.path_of(number) {
            let state = t.dirs.get(path).ok_or(Error::Format)?;
            return Ok((path, dir_node(path, state)));
        }
        let path = t.names.of(number).next().ok_or(Error::NotFound)?;
        Ok((path, Self::object_node(t, number)?))
    }

    /// The file or link object `object` as a view presents it, named or
    /// not: an anonymous open's file, or one unlinked while open (no link).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an object the tree no longer has.
    pub fn open_node(&self, tree: TreeId, object: u64) -> Result<Node> {
        Self::object_node(self.tree(tree)?, object)
    }

    fn object_node(t: &SavedTree, object: u64) -> Result<Node> {
        let meta = *t.meta_of(object).ok_or(Error::NotFound)?;
        Ok(Node {
            number: Some(object),
            meta,
            times: t.times_of(object).copied().unwrap_or_default(),
            len: t.bytes.get(&object).map_or(0, FileMap::len),
            links: u32::try_from(t.names.of(object).count()).unwrap_or(u32::MAX),
        })
    }

    /// At most `count` entries of directory `dir` (`""` the root) in name
    /// order, from the `skip`th: one batch of a directory listing.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for no such entry, [`Error::NotADirectory`] for a
    /// file or link.
    pub fn children(
        &self,
        tree: TreeId,
        dir: &str,
        skip: usize,
        count: usize,
    ) -> Result<Vec<DirEntry>> {
        let t = self.tree(tree)?;
        match t.kind_at(dir) {
            Some(Kind::Dir) => {}
            Some(_) => return Err(Error::NotADirectory),
            None => return Err(Error::NotFound),
        }
        Ok(t.children(dir, skip, count)
            .into_iter()
            .map(|(name, number, kind)| DirEntry { name, number, kind })
            .collect())
    }

    /// A directory: a step. Its number is its record's, which it returns.
    /// In a set-group-ID directory it takes the directory's group and keeps
    /// the set-group-ID bit.
    ///
    /// # Errors
    ///
    /// [`Error::Exists`], [`Error::NotFound`] or [`Error::NotADirectory`] as
    /// `mkdir(2)` answers; [`Error::Name`], [`Error::NameTooLong`]; a failed
    /// record.
    pub fn mkdir(&mut self, tree: TreeId, path: &str, attrs: Attrs, by: &WriterAt) -> Result<u64> {
        name(path)?;
        if self.tree(tree)?.kind_at(path).is_none() {
            self.recorded_name(path, Kind::Dir)?;
        }
        let meta = self.tree(tree)?.new_meta(path, Kind::Dir, attrs);
        self.run_pending_closes(tree, None)?;
        let change = Change::Mkdir {
            tree: tree.to_string(),
            path: path.to_owned(),
            meta,
        };
        self.named_step(tree, by, change, (true, &[]))
    }

    /// An empty directory removed: a step.
    ///
    /// # Errors
    ///
    /// [`Error::NotEmpty`], [`Error::NotADirectory`] or [`Error::NotFound`]
    /// as `rmdir(2)` answers; a failed record.
    pub fn rmdir(&mut self, tree: TreeId, path: &str, by: &WriterAt) -> Result<()> {
        name(path)?;
        self.run_pending_closes(tree, None)?;
        let change = Change::Rmdir {
            tree: tree.to_string(),
            path: path.to_owned(),
        };
        self.named_step(tree, by, change, (true, &[])).map(drop)
    }

    /// A symbolic link to `target`, owned by `owner`, mode `0777`: a step.
    /// Its object is its record's, which it returns; its bytes are the
    /// target, recorded in the step's frame.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an empty target, [`Error::NameTooLong`] for
    /// one over 4095 bytes, [`Error::Invalid`] for one holding NUL; as
    /// [`TreeActor::mkdir`] for the path.
    pub fn symlink(
        &mut self,
        tree: TreeId,
        path: &str,
        target: &str,
        owner: FileOwner,
        by: &WriterAt,
    ) -> Result<u64> {
        name(path)?;
        if self.tree(tree)?.kind_at(path).is_none() {
            self.recorded_name(path, Kind::Symlink)?;
        }
        if target.is_empty() {
            return Err(Error::NotFound);
        }
        if target.len() > MAX_TARGET {
            return Err(Error::NameTooLong);
        }
        if target.contains('\0') {
            return Err(Error::Invalid);
        }
        let attrs = Attrs {
            perm: Perm::from_mode(0o777),
            owner,
        };
        let meta = self.tree(tree)?.new_meta(path, Kind::Symlink, attrs);
        self.run_pending_closes(tree, None)?;
        let change = Change::Symlink {
            tree: tree.to_string(),
            path: path.to_owned(),
            meta,
        };
        self.named_step(tree, by, change, (true, target.as_bytes()))
    }

    /// A link's target.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for an entry that is no link, [`Error::NotFound`]
    /// for none.
    pub fn readlink(&self, tree: TreeId, path: &str) -> Result<Vec<u8>> {
        if self.tree(tree)?.kind_at(path).ok_or(Error::NotFound)? != Kind::Symlink {
            return Err(Error::Invalid);
        }
        self.read(tree, path)
    }

    /// A chmod or chown of the entry at `path` (`""` the root directory):
    /// its kind kept, its mode and owner `attrs`'. A step, unless they are
    /// its metadata already. A link's mode stays `0777`.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for no such entry; a failed record.
    pub fn set_meta(
        &mut self,
        tree: TreeId,
        path: &str,
        attrs: Attrs,
        by: &WriterAt,
    ) -> Result<()> {
        if !path.is_empty() {
            name(path)?;
        }
        let kind = self.tree(tree)?.kind_at(path).ok_or(Error::NotFound)?;
        let meta = Meta::new(kind, attrs);
        if self.node(tree, path)?.meta == meta {
            return Ok(());
        }
        self.run_pending_closes(tree, None)?;
        let change = Change::SetMeta {
            tree: tree.to_string(),
            path: path.to_owned(),
            meta,
        };
        self.named_step(tree, by, change, (true, &[])).map(drop)
    }

    /// An explicit set of the entry's access and modification times
    /// (`utimensat(2)`; `""` the root directory): a step that builds no
    /// commit, since times are each tree's own. Its ctime moves too.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for no such entry; a failed record.
    pub fn set_times(
        &mut self,
        tree: TreeId,
        path: &str,
        atime: Option<SetTime>,
        mtime: Option<SetTime>,
        by: &WriterAt,
    ) -> Result<()> {
        if !path.is_empty() {
            name(path)?;
        }
        let change = Change::SetTimes {
            tree: tree.to_string(),
            path: path.to_owned(),
            atime,
            mtime,
        };
        self.named_step(tree, by, change, (false, &[])).map(drop)
    }

    /// Stable object IDs for a view's inode table (not merge identities).
    pub fn files(&self, tree: TreeId) -> Result<Vec<(String, u64, u64)>> {
        let tree = self.tree(tree)?;
        Ok(tree
            .names
            .iter()
            .map(|(p, o)| (p.to_string(), *o, tree.bytes.get(o).map_or(0, FileMap::len)))
            .collect())
    }

    /// The object `path` names: one lookup, as a view's LOOKUP needs.
    pub fn object_at(&self, tree: TreeId, path: &str) -> Result<u64> {
        self.tree(tree)?
            .names
            .get(path)
            .copied()
            .ok_or(Error::NotFound)
    }

    /// The first path, in path order, naming `object`: one lookup, as a view
    /// needs for an inode.
    pub fn path_of(&self, tree: TreeId, object: u64) -> Result<&str> {
        self.tree(tree)?
            .names
            .of(object)
            .next()
            .ok_or(Error::NotFound)
    }

    /// `open(O_TRUNC)`: a write-open and its truncate to zero, recorded in that
    /// order. When the truncate fails the kernel gets an error and sends no
    /// RELEASE, so the open closes here, or it would hold its file's takes until a
    /// restart (R9.10). A close that fails too stays pending, as any failed close
    /// does.
    pub fn write_open_truncating(
        &mut self,
        tree: TreeId,
        path: &str,
        by: &WriterAt,
        create: Option<Attrs>,
    ) -> Result<u64> {
        // Its truncate empties the view: no fold of pending saves is read.
        let handle = match self.write_open_begin(tree, path, by, create)? {
            WriteOpenStart::Ready(handle) => handle,
            WriteOpenStart::Fold { handle, fold } => {
                drop(fold);
                handle
            }
        };
        if let Err(error) = self.truncate(handle, 0) {
            if let Err(close) = self.close(handle) {
                eprintln!("tree close after a failed truncate: {close}");
            }
            return Err(error);
        }
        Ok(handle)
    }

    /// Open a private write buffer. With `create`, a missing file is made
    /// with those attributes (in a set-group-ID directory, its group): a
    /// recorded step. The returned number is persisted before it can be used
    /// and never repeats. The view starts from the file as `by`'s pending
    /// saves leave it; their fold, when one is needed, runs here under the
    /// lock in one hold (`Hold::PageImage`): the CLI's and tests' way. A
    /// session's write-open folds with the lock released
    /// ([`crate::session::SessionGuard::write_open`]).
    ///
    /// # Errors
    ///
    /// [`Error::IsADirectory`] for a directory, [`Error::Invalid`] for a
    /// link, [`Error::NotFound`] for a missing file without `create`, and as
    /// [`TreeActor::mkdir`] for a create; a failed fold, which closes the
    /// open.
    pub fn write_open(
        &mut self,
        tree: TreeId,
        path: &str,
        by: &WriterAt,
        create: Option<Attrs>,
    ) -> Result<u64> {
        match self.write_open_begin(tree, path, by, create)? {
            WriteOpenStart::Ready(handle) => Ok(handle),
            WriteOpenStart::Fold { handle, fold } => {
                let folded = {
                    let _kind = section(Hold::PageImage);
                    fold.fold()
                };
                self.write_open_folded(handle, folded)
            }
        }
    }

    /// A write-open's view set from its fold's outcome
    /// ([`TreeActor::write_open_view`]); a failed fold closes the open, with
    /// no calls, and is the answer.
    ///
    /// # Errors
    ///
    /// The fold's error; as [`TreeActor::write_open_view`].
    pub(crate) fn write_open_folded(
        &mut self,
        handle: u64,
        folded: Result<FileMap>,
    ) -> Result<u64> {
        match folded {
            Ok(map) => self.write_open_view(handle, map).map(|()| handle),
            Err(error) => {
                if let Err(close) = self.close(handle) {
                    eprintln!("tree close after a failed fold: {close}");
                }
                Err(error)
            }
        }
    }

    /// Create an unnamed open object (`O_TMPFILE`) with these attributes.
    /// Its writes are journaled as usual.
    pub fn anonymous_open(&mut self, tree: TreeId, by: &WriterAt, attrs: Attrs) -> Result<u64> {
        self.tree(tree)?;
        let meta = Meta::new(Kind::File, attrs);
        let (handle, _) = self.journal_record(
            Entry::AnonymousOpen {
                tree: tree.to_string(),
                by: by.clone(),
                meta,
            },
            &[],
        )?;
        let at_ns = self.last_at_ns;
        self.trees
            .get_mut(&tree)
            .ok_or(Error::NotFound)?
            .add_anonymous(handle, meta, at_ns);
        let base = self.tree(tree)?.bytes.get(&handle).and_then(FileMap::step);
        self.handles.insert(
            handle,
            Handle::new(tree, by, handle, FileMap::empty(), base),
        );
        self.pages_opened(tree, handle);
        Ok(handle)
    }

    /// Link an anonymous object for the first time, preserving its identity.
    pub fn first_link(
        &mut self,
        tree: TreeId,
        object: u64,
        path: &str,
        by: &WriterAt,
    ) -> Result<()> {
        name(path)?;
        if self.tree(tree)?.kind_at(path).is_none() {
            self.recorded_name(path, Kind::File)?;
        }
        let change = Change::Link {
            tree: tree.to_string(),
            object,
            path: path.to_owned(),
        };
        self.tree(tree)?.check(&change)?;
        self.run_pending_closes(tree, None)?;
        self.named_step(tree, by, change, (true, &[])).map(drop)
    }

    /// Whether this object has never had a name in this tree.
    pub fn is_anonymous(&self, tree: TreeId, object: u64) -> Result<bool> {
        Ok(self.tree(tree)?.anonymous.contains(&object))
    }

    /// The tree and writer of an open: what a session checks before it
    /// reaches the handle (`crate::session`).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for no such open.
    pub fn open_by(&self, handle: u64) -> Result<(TreeId, &WriterLabel)> {
        let h = self.handles.get(&handle).ok_or(Error::NotFound)?;
        Ok((h.tree, &h.by.writer))
    }

    /// Every open and pending close of `writer` in `tree` closes as a save,
    /// in handle order, after the tree's closes already pending (git-model
    /// §6): what an ended client's opens become. Each open is pending
    /// first, then each gets its close record in handle order, all before
    /// any save, so the end's order survives a restart. Answers each of its
    /// saves, in that order. An open of a file with a save in flight begun
    /// before the end (another client's close, or its own) is saved when
    /// that save lands, not ahead of it, and is not answered here. A failed
    /// append or save stops here: it and the rest stay pending, counted in
    /// `failed_saves`, and saved (an open with no close record appending
    /// its record first) at the tree's next step or at restart. The
    /// session's alone (clippy.toml).
    ///
    /// # Errors
    ///
    /// A failed close-record append or save, as [`TreeActor::close`].
    pub fn close_writer(&mut self, tree: TreeId, writer: &WriterLabel) -> Result<Vec<Saved>> {
        if self.is_stopped() {
            return Err(Error::Stopped);
        }
        let mine: Vec<u64> = self
            .handles
            .iter()
            .filter(|(_, h)| h.tree == tree && h.by.writer == *writer && !h.closing)
            .map(|(handle, _)| *handle)
            .collect();
        let now = std::time::Instant::now();
        for handle in &mine {
            if let Some(h) = self.handles.get_mut(handle) {
                h.pending_since.get_or_insert(now);
            }
            if !self.pending_closes.contains(handle) {
                self.pending_closes.push(*handle);
            }
        }
        for handle in &mine {
            if let Err(e) = self.record_close_begun(*handle) {
                return Err(self.close_failed(*handle, e));
            }
        }
        let saved = self.save_pending(tree)?;
        Ok(mine
            .iter()
            .filter_map(|handle| {
                saved
                    .iter()
                    .find(|(h, _)| h == handle)
                    .map(|(_, outcome)| *outcome)
            })
            .collect())
    }

    /// An open's stable object number, including before its first link.
    pub fn open_object(&self, handle: u64) -> Result<u64> {
        Ok(self.handles.get(&handle).ok_or(Error::NotFound)?.object)
    }

    /// A write-open's view: its saved map at open, its own calls and the
    /// file's page writes. A view clones it under the lock and reads it after.
    pub fn read_open(&self, handle: u64) -> Result<&FileMap> {
        Ok(&self.handles.get(&handle).ok_or(Error::NotFound)?.bytes)
    }

    pub fn write(&mut self, handle: u64, offset: u64, bytes: &[u8], append: bool) -> Result<()> {
        let len = bytes.len() as u64;
        let call = if append {
            Call::Append { len }
        } else {
            Call::Write { offset, len }
        };
        self.call(handle, call, bytes)
    }

    pub fn truncate(&mut self, handle: u64, size: u64) -> Result<()> {
        self.call(handle, Call::Truncate(size), &[])
    }

    /// Checks the call on the writer's view and on the saved head with every
    /// earlier call, journals it with its bytes `tail`, then applies it to the
    /// view as one piece of its frame. Both checks are on lengths, which are
    /// all `apply`'s errors depend on, and the head's is the open's
    /// [`LenRule`] with this call composed in: a call costs its own bytes,
    /// not the file's or its open's. An open being closed takes no call
    /// ([`Error::NotFound`], as a second close is): its close is decided with
    /// the lock released, on the calls it had, and would leave a later one
    /// out of the save.
    pub(in crate::tree_actor) fn call(
        &mut self,
        handle: u64,
        call: Call,
        tail: &[u8],
    ) -> Result<()> {
        let h = self
            .handles
            .get(&handle)
            .filter(|h| !h.closing)
            .ok_or(Error::NotFound)?;
        call.len_after(h.bytes.len())?;
        let lengths = h.lengths.then(&call)?;
        let saved = self
            .tree(h.tree)?
            .bytes
            .get(&h.object)
            .map_or(0, FileMap::len);
        lengths.on(saved)?;
        let (_, pos) = self.journal_record(
            Entry::Call {
                handle,
                call: call.clone(),
            },
            tail,
        )?;
        let h = self.handles.get_mut(&handle).ok_or(Error::NotFound)?;
        h.bytes.apply(&call, &pos)?;
        h.lengths = lengths;
        h.call_count += 1;
        let (tree, object) = (h.tree, h.object);
        self.pages_note_call(tree, object, &call, &pos);
        Ok(())
    }

    /// The journal call for a page write: only `pages` calls it (ledger R1.4).
    /// Returns its journal number and where its runs' bytes are, in order.
    pub(in crate::tree_actor) fn record_page_write(
        &mut self,
        tree: TreeId,
        object: u64,
        runs: &[pages::Changed],
    ) -> Result<(u64, FramePos)> {
        let heads = runs
            .iter()
            .map(|run| RunHead {
                offset: run.offset,
                len: run.bytes.len() as u64,
            })
            .collect();
        let tail = runs.iter().fold(Vec::new(), |mut tail, run| {
            tail.extend_from_slice(&run.bytes);
            tail
        });
        self.journal_record(
            Entry::PageWrite {
                tree: tree.to_string(),
                object,
                runs: heads,
            },
            &tail,
        )
    }

    /// RELEASE publishes calls against this tree's head, never the stale open
    /// base. A failed save changes nothing and answers its errno: the close
    /// stays pending, its handle still counting as open, and saves first at the
    /// next step in its tree and at restart. [`TreeActor::close_begin`],
    /// [`CloseWork::decide`] and [`TreeActor::close_finish`] in one hold of
    /// the lock: `Recording::close_with` releases it for the decision.
    /// A close of a file whose earlier-begun save is in flight cannot wait
    /// in one hold: it answers [`Error::Busy`] and stays pending, saved
    /// after that save. A session reaches this only for a file with none.
    pub fn close(&mut self, handle: u64) -> Result<Saved> {
        let _kind = section(Hold::Close);
        match self.close_begin(handle)? {
            CloseStart::Done(saved) => Ok(saved),
            CloseStart::Decide(work) => match work.decide() {
                Ok(decision) => self.close_finish(decision),
                Err(e) => Err(self.close_failed(handle, e)),
            },
            CloseStart::Queued => Err(self.close_failed(handle, Error::Busy)),
        }
    }

    /// A close's first half, under the lock, O(1) plus the file's page
    /// writes. First its close record ([`TreeActor::record_close_begun`]):
    /// a failed append is a failed close, pending and counted, its retry
    /// appending the record first. Saved here: an anonymous or orphaned file's, any close in a
    /// tree with failed closes pending (after them, in order), an open with
    /// no call and no page write (unchanged), and an open whose saved map
    /// has not moved, whose file has no page writes and whose length changed,
    /// with few pieces: its view is the save. Else the snapshots for
    /// [`CloseWork::decide`], the handle marked closing. First of all, a
    /// close of a file with an earlier-begun save in flight is marked
    /// closing with no snapshot ([`CloseStart::Queued`]): it is never saved
    /// ahead of that save.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`], [`Error::NotFound`] (no such open, or one being
    /// closed); a failed close-record append or save, which stays pending.
    pub(in crate::tree_actor) fn close_begin(&mut self, handle: u64) -> Result<CloseStart> {
        let _kind = section(Hold::Close);
        if self.is_stopped() {
            return Err(Error::Stopped);
        }
        let h = self.handles.get(&handle).ok_or(Error::NotFound)?;
        if h.closing {
            return Err(Error::NotFound);
        }
        let (tree, object) = (h.tree, h.object);
        if let Err(e) = self.record_close_begun(handle) {
            return Err(self.close_failed(handle, e));
        }
        let queued = self.save_in_flight(tree, object, None);
        if let Some(h) = self.handles.get_mut(&handle) {
            h.pending_since.get_or_insert_with(std::time::Instant::now);
            if queued {
                h.closing = true;
                return Ok(CloseStart::Queued);
            }
        }
        self.close_start(handle)
    }

    /// A queued close's turn, under the lock: still [`CloseStart::Queued`]
    /// while an earlier-begun save of its file is in flight, else its close
    /// as [`TreeActor::close_begin`] goes on past that check.
    ///
    /// # Errors
    ///
    /// As [`TreeActor::close_begin`]; [`Error::NotFound`] for an open not
    /// queued.
    pub(in crate::tree_actor) fn close_resume(&mut self, handle: u64) -> Result<CloseStart> {
        let _kind = section(Hold::Close);
        if self.is_stopped() {
            return Err(Error::Stopped);
        }
        let h = self
            .handles
            .get(&handle)
            .filter(|h| h.closing)
            .ok_or(Error::NotFound)?;
        if self.save_in_flight(h.tree, h.object, h.close_order) {
            return Ok(CloseStart::Queued);
        }
        if let Some(h) = self.handles.get_mut(&handle) {
            h.closing = false;
        }
        self.close_start(handle)
    }

    /// [`TreeActor::close_begin`] past its same-file check.
    fn close_start(&mut self, handle: u64) -> Result<CloseStart> {
        let h = self.handles.get(&handle).ok_or(Error::NotFound)?;
        let (tree, object) = (h.tree, h.object);
        let t = self.tree(tree)?;
        let pending = self
            .pending_closes
            .iter()
            .any(|p| self.handles.get(p).is_some_and(|o| o.tree == tree));
        if pending || t.anonymous.contains(&object) || !t.is_named(object) {
            if !self.pending_closes.contains(&handle) {
                self.pending_closes.push(handle);
            }
            return self
                .run_pending_closes(tree, Some(handle))
                .map(CloseStart::Done);
        }
        let saved = t.bytes.get(&object).ok_or(Error::NotFound)?;
        let moved = h.base_step.is_none() || saved.step() != h.base_step;
        let pages = self.pages_writes(tree, object);
        let quick = if h.call_count == 0 && pages.is_none() {
            Some((SaveOutcome::Unchanged, None))
        } else if !moved
            && pages.is_none()
            && h.bytes.len() != saved.len()
            && h.bytes.piece_count() <= file_map::SMALL_PIECES
        {
            let layout = Layout::of_view(&h.bytes, RecordNo::new(handle));
            match decided(saved, &layout, &h.bytes) {
                Ok(decided) => Some((layout.outcome(), Some(decided))),
                Err(e) => return Err(self.close_failed(handle, e)),
            }
        } else {
            None
        };
        if let Some((outcome, map)) = quick {
            return match self.save_close(handle, outcome, map, Vec::new()) {
                Ok(saved) => Ok(CloseStart::Done(saved)),
                Err(e) => Err(self.close_failed(handle, e)),
            };
        }
        let reader =
            (h.call_count > 0 && (moved || pages.is_some())).then(|| self.journal.call_reader());
        let work = CloseWork {
            handle,
            tree,
            object,
            saved: saved.clone(),
            saved_step: saved.step(),
            view: h.bytes.clone(),
            pages,
            reader,
        };
        if let Some(h) = self.handles.get_mut(&handle) {
            h.closing = true;
        }
        Ok(CloseStart::Decide(work))
    }

    /// A close's second half, under the lock: the decision lands, recorded,
    /// when what it read is still current (its open, the file named and not
    /// anonymous, the saved map's step, the file's page writes); else the
    /// close is redone once here, its calls read back.
    ///
    /// # Errors
    ///
    /// [`Error::Stopped`]; [`Error::NotFound`] for an open not being
    /// closed; a failed save, which stays pending.
    pub(in crate::tree_actor) fn close_finish(&mut self, decision: CloseDecision) -> Result<Saved> {
        let _kind = section(Hold::Close);
        let CloseDecision {
            work,
            outcome,
            map,
            pieces,
        } = decision;
        let handle = work.handle;
        if self.is_stopped() {
            return Err(Error::Stopped);
        }
        let h = self
            .handles
            .get_mut(&handle)
            .filter(|h| h.closing)
            .ok_or(Error::NotFound)?;
        h.closing = false;
        let current = self.tree(work.tree).is_ok_and(|t| {
            !t.anonymous.contains(&work.object)
                && t.is_named(work.object)
                && t.bytes.get(&work.object).map(FileMap::step) == Some(work.saved_step)
        }) && self.pages_written(work.tree, work.object)
            == work.pages.as_ref().map(|(w, through)| (w.len(), *through));
        let saved = if current {
            self.save_close(handle, outcome, map, pieces)
        } else {
            // Something moved while it was decided: today's close, under
            // the lock, after the tree's pending closes.
            if !self.pending_closes.contains(&handle) {
                self.pending_closes.push(handle);
            }
            self.run_pending_closes(work.tree, Some(handle))
        };
        let saved = saved.map_err(|e| self.close_failed(handle, e))?;
        // The landing is the tree's step: the closes it held back (an ended
        // client's open of the file) are saved after it.
        if let Err(e) = self.save_pending(work.tree) {
            eprintln!("tree SaveFailed: tree={}: {e}; it stays pending", work.tree);
        }
        Ok(saved)
    }

    /// A close that could not save: it stays pending, its handle still
    /// counting as open, and saves at its tree's next step or at restart.
    /// Counted in `failed_saves`.
    pub(in crate::tree_actor) fn close_failed(&mut self, handle: u64, error: Error) -> Error {
        if let Some(h) = self.handles.get_mut(&handle) {
            h.closing = false;
            h.pending_since.get_or_insert_with(std::time::Instant::now);
            if !self.pending_closes.contains(&handle) {
                self.pending_closes.push(handle);
            }
            self.metrics.failed_saves = self.metrics.failed_saves.saturating_add(1);
        }
        error
    }

    /// The open's close record: [`Entry::CloseBegun`] appended, with no
    /// fsync, unless the open has one, and its number set as the open's
    /// `close_order`. One `write(2)` of about 100 bytes: in the kernel when
    /// the close is answered, so it survives a kill, and durable at the
    /// next fsync, FSYNCDIR, `O_SYNC` write or builder batch. Called by
    /// [`TreeActor::close_begin`] before anything else it does, by
    /// [`TreeActor::close_writer`] for each open it closes (all appended
    /// before any save), by recovery through [`TreeActor::close`], and by
    /// the one writer of a save for a close that has none yet.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for no such open; a failed append, with the
    /// open unchanged.
    fn record_close_begun(&mut self, handle: u64) -> Result<RecordNo> {
        let h = self.handles.get(&handle).ok_or(Error::NotFound)?;
        if let Some(order) = h.close_order {
            return Ok(order);
        }
        let (seq, _) = self.journal_record(Entry::CloseBegun { handle }, &[])?;
        let order = RecordNo::new(seq);
        if let Some(h) = self.handles.get_mut(&handle) {
            h.close_order = Some(order);
        }
        Ok(order)
    }

    /// `rename(2)`: a step. A directory moves with every entry under it,
    /// each file keeping its object and identity; an open of one follows
    /// its object. A move onto itself changes nothing.
    ///
    /// # Errors
    ///
    /// As POSIX: [`Error::NotFound`], [`Error::NotADirectory`] (a directory
    /// onto a file), [`Error::IsADirectory`] (a file onto a directory),
    /// [`Error::NotEmpty`] (onto a directory with entries),
    /// [`Error::Invalid`] (a directory into its own subtree), and
    /// [`Error::Exists`] for [`RenameMode::NoReplace`] onto an entry.
    pub fn rename(
        &mut self,
        tree: TreeId,
        from: &str,
        to: &str,
        mode: RenameMode,
        by: &WriterAt,
    ) -> Result<()> {
        name(from)?;
        name(to)?;
        let t = self.tree(tree)?;
        if let (Some(kind), None) = (t.kind_at(from), t.kind_at(to)) {
            self.recorded_name(to, kind)?;
        }
        let change = Change::Rename {
            tree: tree.to_string(),
            from: from.to_owned(),
            to: to.to_owned(),
        };
        let t = self.tree(tree)?;
        t.check(&change)?;
        if mode == RenameMode::NoReplace && t.kind_at(to).is_some() {
            return Err(Error::Exists);
        }
        if from != to {
            self.run_pending_closes(tree, None)?;
            self.named_step(tree, by, change, (true, &[]))?;
        }
        Ok(())
    }

    /// `unlink(2)`: a step.
    ///
    /// # Errors
    ///
    /// [`Error::IsADirectory`] for a directory, [`Error::NotFound`] for no
    /// such entry; a failed record.
    pub fn unlink(&mut self, tree: TreeId, path: &str, by: &WriterAt) -> Result<()> {
        name(path)?;
        let change = Change::Unlink {
            tree: tree.to_string(),
            path: path.to_owned(),
        };
        self.tree(tree)?.check(&change)?;
        self.run_pending_closes(tree, None)?;
        self.named_step(tree, by, change, (true, &[])).map(drop)
    }

    /// One step: afterwards `path` holds exactly the `len` bytes of `src`,
    /// with `attrs`'s mode and owner: the atomic save a program makes by
    /// renaming a finished file over `path`.
    ///
    /// An existing file keeps its object and identity, and its unchanged
    /// lines keep theirs (aligned against its saved lines as a close's save
    /// is); a missing file is created as a create step would create it.
    ///
    /// The bytes go to the journal in chunk records just before the step; a
    /// crash, or an error from `src`, before the step's record leaves `path`
    /// as it was, and chunk records not followed by their step never reach a
    /// later step.
    ///
    /// Its commit is a direct save's (no `Agentfs-Open` trailer) and is
    /// queued for the upstream as every save is. Opens of the file keep
    /// their own copies and save over it at their close, as with any save.
    ///
    /// Same bytes and same metadata: [`Saved::Unchanged`] and no commit.
    ///
    /// # Errors
    ///
    /// [`Error::IsADirectory`] (a directory at `path`), [`Error::Invalid`] (a
    /// link at `path`), [`Error::NotFound`] (no parent directory),
    /// [`Error::Untracked`] (a missing path the rules place in the untracked
    /// area), [`Error::Io`] from `src` (`UnexpectedEof` when it ends before
    /// `len`), and a failed record.
    pub fn replace_file(
        &mut self,
        tree: TreeId,
        path: &str,
        attrs: Attrs,
        src: &mut dyn std::io::Read,
        len: u64,
        by: &WriterAt,
    ) -> Result<Saved> {
        name(path)?;
        let t = self.tree(tree)?;
        let existing = t
            .names
            .get(path)
            .copied()
            .filter(|_| t.kind_at(path) == Some(Kind::File));
        if t.kind_at(path).is_none() {
            self.recorded_name(path, Kind::File)?;
        }
        let t = self.tree(tree)?;
        let meta = match existing {
            Some(_) => Meta::new(Kind::File, attrs),
            None => t.new_meta(path, Kind::File, attrs),
        };
        let change = Change::Replace {
            tree: tree.to_string(),
            path: path.to_owned(),
            meta,
            len,
            chunks: TakeRecord::chunks_for(len),
        };
        t.check(&change)?;
        self.run_pending_closes(tree, None)?;
        if self.is_stopped() {
            return Err(Error::Stopped);
        }
        let started = std::time::Instant::now();
        // The chunks, then the step: no compaction comes between.
        self.before_record();
        let mut chunks = Vec::new();
        let mut done = 0;
        while done < len {
            let n = (len - done).min(TAKE_CHUNK);
            let mut buf = vec![0; usize::try_from(n).map_err(|_| Error::TooLarge)?];
            src.read_exact(&mut buf)?;
            let (_, pos) = self.append_record(Entry::Bytes, &buf)?;
            chunks.push(Tail { pos, len: n });
            done += n;
        }
        let seq = self.next;
        let map = chunk_map(&chunks, len, RecordNo::new(seq))?;
        if let Some(object) = existing {
            let t = self.tree(tree)?;
            let saved = t.bytes.get(&object).ok_or(Error::NotFound)?;
            let whole = 0..len;
            if t.meta_of(object) == Some(&meta)
                && map.same_as(saved, std::slice::from_ref(&whole))?
            {
                return Ok(Saved::Unchanged);
            }
        }
        crate::failpoints::hit("tree.replace_before_step")?;
        let pointers = self.publish(tree, seq);
        let (recorded, _, _) = self.record_with(
            by.clone(),
            change.clone(),
            pointers,
            (&mut std::iter::empty(), &[]),
            Vec::new(),
            false,
        )?;
        if recorded != seq {
            return Err(Error::Format);
        }
        let at_ns = self.last_at_ns;
        let t = self.trees.get_mut(&tree).ok_or(Error::NotFound)?;
        let keys = Self::apply_shape(&mut self.stopped, t, &change, seq, at_ns);
        let object = existing.unwrap_or(seq);
        t.bytes.insert(object, map);
        self.touch(tree, seq, keys);
        if existing.is_some() {
            // The view drops the file's pages, as after a take.
            self.changed.push((tree, object));
        }
        self.pages_saved(tree, object, seq);
        self.count_save(started);
        self.after_step();
        Ok(Saved::Commit(RecordNo::new(seq)))
    }

    /// The guard on every call that makes a name: a name the rules place in
    /// the untracked area never enters a tree ([`Error::Untracked`]), judged
    /// before anything is journaled (docs/trees.md, "Rules and the untracked
    /// area"). Rules decide only for new names: callers ask only when no
    /// entry is at `path`. A name that is not git-safe (a `.git` component
    /// among them) is refused first, as [`Error::Name`].
    fn recorded_name(&self, path: &str, kind: Kind) -> Result<()> {
        let kind = match kind {
            Kind::File => agentfs_rules::NewKind::File,
            Kind::Dir => agentfs_rules::NewKind::Dir,
            Kind::Symlink => agentfs_rules::NewKind::Symlink,
        };
        match self
            .rules
            .place(path, kind, agentfs_rules::PathFacts::default())
        {
            agentfs_rules::Placement::Tracked => Ok(()),
            agentfs_rules::Placement::Untracked(reason) => Err(Error::Untracked {
                path: path.to_owned(),
                reason,
            }),
        }
    }

    /// Saves this tree's pending closes in close order, answering for `wanted`.
    pub(in crate::tree_actor) fn run_pending_closes(
        &mut self,
        tree: TreeId,
        wanted: Option<u64>,
    ) -> Result<Saved> {
        Ok(self
            .save_pending(tree)?
            .into_iter()
            .find(|(handle, _)| wanted == Some(*handle))
            .map_or(Saved::Unchanged, |(_, saved)| saved))
    }

    /// Saves this tree's pending closes in close order ([`close_rank`]),
    /// not the order they failed in; answers each. A pending close of a
    /// file with a save in flight that began before it (any such save, for
    /// an open with no close record) stays pending until that save lands:
    /// a file's saves land in the order their closes began.
    fn save_pending(&mut self, tree: TreeId) -> Result<Vec<(u64, Saved)>> {
        let mut due: Vec<u64> = self
            .pending_closes
            .iter()
            .copied()
            .filter(|handle| {
                self.handles.get(handle).is_some_and(|h| {
                    h.tree == tree && !self.save_in_flight(tree, h.object, h.close_order)
                })
            })
            .collect();
        due.sort_by_key(|handle| self.handles.get(handle).map(|h| close_rank(*handle, h)));
        let mut saved = Vec::with_capacity(due.len());
        for handle in due {
            let _kind = section(Hold::Close);
            saved.push((handle, self.close_step(handle)?));
        }
        Ok(saved)
    }

    /// A create: the step that names a new object and opens it.
    fn create_step(
        &mut self,
        tree: TreeId,
        path: &str,
        by: &WriterAt,
        attrs: Attrs,
    ) -> Result<u64> {
        let meta = self.tree(tree)?.new_meta(path, Kind::File, attrs);
        let change = Change::Create {
            tree: tree.to_string(),
            path: path.to_owned(),
            meta,
        };
        self.tree(tree)?.check(&change)?;
        self.run_pending_closes(tree, None)?;
        let seq = self.next;
        let pointers = self.publish(tree, seq);
        let started = std::time::Instant::now();
        self.record_step(
            by.clone(),
            change.clone(),
            pointers,
            &mut std::iter::empty(),
        )?;
        let at_ns = self.last_at_ns;
        let t = self.trees.get_mut(&tree).ok_or(Error::NotFound)?;
        let keys = Self::apply_shape(&mut self.stopped, t, &change, seq, at_ns);
        let mut empty = FileMap::empty();
        empty.saved_at(RecordNo::new(seq));
        t.bytes.insert(seq, empty);
        self.touch(tree, seq, keys);
        self.handles.insert(
            seq,
            Handle::new(tree, by, seq, FileMap::empty(), Some(RecordNo::new(seq))),
        );
        self.pages_opened(tree, seq);
        self.count_save(started);
        self.after_step();
        Ok(seq)
    }

    /// A step that changes names, directories, metadata or times, checked
    /// before its record and applied after it; with `publish`, its commit's
    /// pointer changes (none for times), and `tail` its record's own bytes (a
    /// link's target, whose saved map reads them there). Its number.
    fn named_step(
        &mut self,
        tree: TreeId,
        by: &WriterAt,
        change: Change,
        (publish, tail): (bool, &[u8]),
    ) -> Result<u64> {
        self.tree(tree)?.check(&change)?;
        let seq = self.next;
        let pointers = if publish {
            self.publish(tree, seq)
        } else {
            Vec::new()
        };
        let started = std::time::Instant::now();
        let (recorded, _, pos) = self.record(
            by.clone(),
            change.clone(),
            pointers,
            (&mut std::iter::empty(), tail),
            Vec::new(),
        )?;
        if recorded != seq {
            return Err(Error::Format);
        }
        let at_ns = self.last_at_ns;
        let t = self.trees.get_mut(&tree).ok_or(Error::NotFound)?;
        let keys = Self::apply_shape(&mut self.stopped, t, &change, seq, at_ns);
        if let Change::Symlink { .. } = change {
            let mut map = FileMap::empty();
            map.apply(
                &Call::Write {
                    offset: 0,
                    len: tail.len() as u64,
                },
                &pos,
            )?;
            map.saved_at(RecordNo::new(seq));
            t.bytes.insert(seq, map);
        }
        self.touch(tree, seq, keys);
        if publish {
            self.count_save(started);
        }
        self.after_step();
        Ok(seq)
    }

    /// Applies a recorded step's shape transition, which was checked before
    /// its record: a failure now is a bug, and stops the actor.
    fn apply_shape(
        stopped: &mut bool,
        tree: &mut SavedTree,
        change: &Change,
        seq: u64,
        at_ns: u64,
    ) -> BTreeSet<Key> {
        tree.shape
            .apply(change, EventNumber::new(seq), at_ns)
            .unwrap_or_else(|e| {
                eprintln!("tree SaveFailed: a recorded step does not apply: {e}");
                *stopped = true;
                BTreeSet::new()
            })
    }

    fn count_save(&mut self, started: std::time::Instant) {
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.metrics.saves = self.metrics.saves.saturating_add(1);
        self.metrics.save_nanos = self.metrics.save_nanos.saturating_add(nanos);
        self.metrics.max_save_nanos = self.metrics.max_save_nanos.max(nanos);
    }

    /// A close decided under the lock: its calls read back from the journal
    /// and replayed, with the file's chosen page-write pieces, on the saved
    /// map; then saved. The builder replays the same calls on the built map.
    fn close_step(&mut self, handle: u64) -> Result<Saved> {
        let h = self.handles.get(&handle).ok_or(Error::NotFound)?;
        let (tree, object) = (h.tree, h.object);
        let t = self.tree(tree)?;
        if t.anonymous.contains(&object) {
            return self.save_close(handle, SaveOutcome::AnonymousDropped, None, Vec::new());
        }
        if !t.is_named(object) {
            return self.save_close(handle, SaveOutcome::OrphanDropped, None, Vec::new());
        }
        let own = self.own_calls(handle)?;
        let pieces = self.pages_close_pieces(h, &own);
        let calls = pages::close_calls(own.iter(), pieces.clone());
        let saved = t.bytes.get(&object).ok_or(Error::NotFound)?;
        let (outcome, map) = replay_judge(saved, calls)?;
        self.save_close(handle, outcome, map, pieces)
    }

    /// The calls of the open `handle`, read back from the journal: none
    /// without a read when it made none.
    pub(in crate::tree_actor) fn own_calls(
        &self,
        handle: u64,
    ) -> Result<Vec<(u64, Call, FramePos)>> {
        match self.handles.get(&handle) {
            Some(h) if h.call_count > 0 => self.journal.call_reader().calls(handle),
            _ => Ok(Vec::new()),
        }
    }

    /// A close's save, decided: its records appended (the commit point), then
    /// applied. A changed file's save records exactly its layout's outcome,
    /// its layout in chunk records just before the step, and the page-write
    /// `pieces` it applied; the saved map is then the layout laid on the
    /// saved map ([`decided`]): what a replay of the records lays. The one
    /// writer of a Saved close, so the one place that makes sure the open
    /// has its close record before its save (a restart refuses a save with
    /// none): a failed close whose record could not be appended at its
    /// start appends it here. The open's keys go and its handle with them.
    fn save_close(
        &mut self,
        handle: u64,
        outcome: SaveOutcome,
        saved_bytes: Option<Decided>,
        pieces: Vec<(u64, pages::Run)>,
    ) -> Result<Saved> {
        self.record_close_begun(handle)?;
        let h = self.handles.get(&handle).ok_or(Error::NotFound)?;
        let (tree, object, by) = (h.tree, h.object, h.by.clone());
        let (chunks, saved_bytes) = match saved_bytes {
            Some((chunks, laid)) => (chunks, Some(laid)),
            None => (Vec::new(), None),
        };
        let seq = self.next + chunks.len() as u64;
        let pointers = if saved_bytes.is_some() {
            self.publish(tree, seq)
        } else {
            Vec::new()
        };
        let change = Change::Save {
            tree: tree.to_string(),
            handle,
            pages: pieces
                .iter()
                .map(|(record, run)| PagePiece {
                    record: RecordNo::new(*record),
                    offset: run.offset,
                    len: run.len,
                })
                .collect(),
            outcome,
        };
        let started = std::time::Instant::now();
        // Every close lets its open's held keys go, a changed file's or not.
        let keys = self.tree(tree)?.shape.keys(object);
        let (recorded, _) =
            self.record_step(by, change, pointers, &mut chunks.into_iter().map(Ok))?;
        debug_assert_eq!(
            recorded, seq,
            "the layout's chunks come just before the step"
        );
        self.touch(tree, seq, keys);
        self.pending_closes.retain(|pending| *pending != handle);
        self.handles.remove(&handle);
        let at_ns = self.last_at_ns;
        let t = self.trees.get_mut(&tree).ok_or(Error::NotFound)?;
        t.shape.save(object, outcome, at_ns);
        match (outcome, saved_bytes) {
            (SaveOutcome::AnonymousDropped, _) => t.drop_object(object),
            (_, Some(mut map)) => {
                map.saved_at(RecordNo::new(seq));
                t.bytes.insert(object, map);
            }
            _ => {}
        }
        self.pages_saved(tree, object, seq);
        let saved = match outcome {
            SaveOutcome::Saved { .. } => {
                self.count_save(started);
                Saved::Commit(RecordNo::new(seq))
            }
            SaveOutcome::OrphanDropped => Saved::OrphanDropped,
            SaveOutcome::Unchanged | SaveOutcome::AnonymousDropped => Saved::Unchanged,
        };
        self.after_step();
        Ok(saved)
    }
}

/// The map `calls` leave on `saved`, or none if they leave it as it was: a save
/// that changes nothing commits nothing (finding 36; the owner, 2026-11-07, "1. A").
/// "As it was" compares only the bytes the calls touched, never the whole file.
/// The close as it was decided before the off-lock close: the property
/// tests' oracle.
#[cfg(test)]
pub(in crate::tree_actor) fn changed(
    saved: &FileMap,
    calls: &[(Call, FramePos)],
) -> Result<Option<FileMap>> {
    let mut map = saved.clone();
    let mut ranges = Vec::new();
    for (call, pos) in calls {
        ranges.extend(call.touched(map.len()));
        map.apply(call, pos)?;
    }
    if calls.is_empty() || (map.len() == saved.len() && map.same_as(saved, &ranges)?) {
        return Ok(None);
    }
    Ok(Some(map))
}

/// A path a tree may name: a git-safe relative path ([`RepoPath`]'s rules,
/// `/` between components), each component at most 255 bytes.
pub(in crate::tree_actor) fn name(path: &str) -> Result<()> {
    RepoPath::new(path).map_err(|_| Error::Name)?;
    if path.split('/').any(|component| component.len() > 255) {
        return Err(Error::NameTooLong);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree_actor::journal::segment::Scratch;

    /// `call` checks on lengths alone: `len_after` must give `apply`'s length
    /// and fail where it fails.
    #[test]
    fn len_after_is_the_length_apply_leaves() {
        let calls = [
            Call::Write { offset: 3, len: 3 },
            Call::Write { offset: 40, len: 0 },
            Call::Write { offset: 0, len: 1 },
            Call::Append { len: 4 },
            Call::Truncate(2),
            Call::Truncate(9),
        ];
        let mut frames = Scratch::new();
        for len in [0, 1, 5, 12] {
            for call in &calls {
                let mut map = FileMap::empty();
                map.apply(
                    &Call::Append { len },
                    &frames.add(&vec![b'.'; len as usize]),
                )
                .unwrap();
                let pos = frames.add(&vec![b'x'; call.carried() as usize]);
                map.apply(call, &pos).unwrap();
                assert_eq!(call.len_after(len).unwrap(), map.len(), "{call:?} on {len}");
            }
        }
        let too_far = Call::Write {
            offset: u64::MAX,
            len: 1,
        };
        assert!(too_far.len_after(0).is_err());
        assert!(FileMap::empty().apply(&too_far, &frames.add(b"a")).is_err());
    }

    #[test]
    fn a_close_that_rewrites_the_same_bytes_changes_nothing() {
        let mut frames = Scratch::new();
        let mut saved = FileMap::empty();
        saved
            .apply(&Call::Append { len: 6 }, &frames.add(b"abcdef"))
            .unwrap();
        let same = [
            (Call::Truncate(0), frames.add(b"")),
            (Call::Write { offset: 0, len: 6 }, frames.add(b"abcdef")),
        ];
        assert!(changed(&saved, &same).unwrap().is_none());
        let other = [(Call::Write { offset: 2, len: 1 }, frames.add(b"C"))];
        let map = changed(&saved, &other).unwrap().unwrap();
        assert_eq!(map.to_vec().unwrap(), b"abCdef");
        assert!(changed(&saved, &[]).unwrap().is_none());
    }

    /// A small deterministic generator: the properties need no crate.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
        }

        fn next(&mut self, below: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % below.max(1)
        }

        /// A length near zero or near `u64::MAX`, where folds overflow.
        fn edge(&mut self) -> u64 {
            match self.next(4) {
                0 => self.next(16),
                1 => u64::MAX - self.next(16),
                2 => u64::MAX / 2 + self.next(16),
                _ => self.next(1 << 40),
            }
        }
    }

    /// A2: the rule composed per call gives the fold's length from any saved
    /// length, and fails exactly where the fold fails.
    #[test]
    fn a_length_rule_is_the_fold_of_its_calls() {
        for seed in 1..2_000_u64 {
            let mut rng = Rng::new(seed);
            let calls: Vec<Call> = (0..1 + rng.next(6))
                .map(|_| match rng.next(3) {
                    0 => Call::Write {
                        offset: rng.edge(),
                        len: rng.edge() % 3,
                    },
                    1 => Call::Append { len: rng.edge() },
                    _ => Call::Truncate(rng.edge()),
                })
                .collect();
            let rule = calls
                .iter()
                .try_fold(LenRule::NONE, |rule, call| rule.then(call));
            for _ in 0..8 {
                let saved = rng.edge();
                let fold = calls
                    .iter()
                    .try_fold(saved, |len, call| call.len_after(len));
                match &rule {
                    Ok(rule) => assert_eq!(
                        rule.on(saved).ok(),
                        fold.ok(),
                        "seed {seed}: {calls:?} on {saved}: {rule:?}"
                    ),
                    // A rule fails only where every saved length does.
                    Err(_) => assert!(fold.is_err(), "seed {seed}: {calls:?} on {saved}"),
                }
            }
        }
        // The erratum's case: an append that overflows only from a longer
        // saved length, then a truncate.
        let calls = [Call::Append { len: u64::MAX }, Call::Truncate(0)];
        let rule = calls
            .iter()
            .try_fold(LenRule::NONE, |rule, call| rule.then(call))
            .unwrap();
        assert_eq!(rule.on(0).ok(), Some(0));
        assert!(rule.on(1).is_err());
    }

    use crate::tree_actor::journal::{Journal, fold};
    use crate::tree_actor::test_support::{ATTRS, new_actor, writer};
    use crate::tree_actor::{JOURNAL, TreeId};

    const PATH: &str = "f";

    /// The Save record the journal holds for `handle`: what its close decided.
    fn recorded_outcome(root: &std::path::Path, handle: u64) -> SaveOutcome {
        let (_, frames) = Journal::open(&root.join(JOURNAL), RecordNo::ZERO).unwrap();
        frames
            .iter()
            .find_map(|(record, _)| match &record.entry {
                Entry::Step {
                    change:
                        Change::Save {
                            handle: of,
                            outcome,
                            ..
                        },
                    ..
                } if *of == handle => Some(*outcome),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no save of {handle} in the journal"))
    }

    /// The oracle: today's close as it was before the off-lock close, on
    /// the actor's state now, with the open's calls from a fold of the
    /// journal (not the read-back the close uses): the outcome and the bytes
    /// it saves.
    fn oracle(v: &TreeActor, root: &std::path::Path, handle: u64) -> (SaveOutcome, Vec<u8>) {
        let (_, frames) = Journal::open(&root.join(JOURNAL), RecordNo::ZERO).unwrap();
        let folded = fold(&frames).unwrap();
        let own = folded
            .opens
            .get(&handle)
            .map(|open| open.calls.clone())
            .unwrap_or_default();
        let h = &v.handles[&handle];
        let saved = v.tree(h.tree).unwrap().bytes[&h.object].clone();
        let pieces = match v.pages_writes(h.tree, h.object) {
            Some((writes, through)) => pages::close_pieces(saved.len(), &own, &writes, through),
            None => Vec::new(),
        };
        let calls = pages::close_calls(own.iter(), pieces);
        let collected: Vec<(Call, FramePos)> = calls
            .clone()
            .map(|call| (call.call().clone(), call.at().clone()))
            .collect();
        match changed(&saved, &collected).unwrap() {
            Some(map) => (
                Layout::of_close(saved.len(), calls).unwrap().outcome(),
                map.to_vec().unwrap(),
            ),
            None => (SaveOutcome::Unchanged, saved.to_vec().unwrap()),
        }
    }

    /// Runs one random history of two opens of one file, closing each
    /// against the oracle, and builds it: every build must succeed, so the
    /// builder's own check of each save agrees with the actor's: no commit
    /// is flagged. `pause` waits 3 ms before each close, which lets the
    /// builder's batches run, and rebase the saved map, at other points.
    /// `closed` gets each close's outcome.
    fn two_opens(seed: u64, pause: bool, closed: &mut Vec<SaveOutcome>) {
        let mut rng = Rng::new(seed);
        let tmp = tempfile::tempdir().unwrap();
        let mut v = new_actor(tmp.path());
        let w = writer("w");
        let first = v.write_open(TreeId::Root, PATH, &w, Some(ATTRS)).unwrap();
        let seed_bytes: Vec<u8> = (0..rng.next(200)).map(|_| rng.next(256) as u8).collect();
        v.write(first, 0, &seed_bytes, false).unwrap();
        v.close(first).unwrap();
        let mut open: [Option<u64>; 2] = [None, None];
        for step in 0..80 {
            let which = rng.next(2) as usize;
            let Some(h) = open[which] else {
                if rng.next(2) == 0 {
                    open[which] = Some(v.write_open(TreeId::Root, PATH, &w, None).unwrap());
                }
                continue;
            };
            let len = v.read_open(h).unwrap().len();
            let data: Vec<u8> = (0..1 + rng.next(40)).map(|_| rng.next(256) as u8).collect();
            match rng.next(16) {
                0..=4 => v.write(h, rng.next(len + 30), &data, false).unwrap(),
                5 => v.write(h, 0, &data, true).unwrap(),
                6 => v.truncate(h, rng.next(len + 20)).unwrap(),
                7 => {
                    // The same bytes again, where they are.
                    let at = rng.next(len.max(1));
                    let mut same = vec![0; data.len()];
                    let n = v.read_open(h).unwrap().read(at, &mut same).unwrap();
                    v.write(h, at, &same[..n], false).unwrap();
                }
                8 | 9 => v.page_writeback(h, rng.next(len + 30), &data).unwrap(),
                10 => {
                    // Built, so the next record rebases the saved map.
                    if let Err(e) = v.settle() {
                        panic!("seed {seed} step {step}: {e}");
                    }
                }
                _ => {
                    if pause {
                        std::thread::sleep(std::time::Duration::from_millis(3));
                    }
                    let (outcome, bytes) = oracle(&v, tmp.path(), h);
                    v.close(h).unwrap();
                    open[which] = None;
                    assert_eq!(
                        recorded_outcome(tmp.path(), h),
                        outcome,
                        "seed {seed} step {step}"
                    );
                    assert_eq!(
                        v.read(TreeId::Root, PATH).unwrap(),
                        bytes,
                        "seed {seed} step {step}"
                    );
                    closed.push(outcome);
                }
            }
        }
        if let Err(e) = v.settle() {
            panic!("seed {seed} at the end: {e}");
        }
        assert_eq!(
            v.verification().counts(),
            crate::tree_actor::UnverifiedCounts::default(),
            "seed {seed}: the builder's replay of every save agrees with its record"
        );
    }

    /// A1: seeds 1..64 of two opens of one file (writes, appends, truncates,
    /// the same bytes rewritten, page writes, the other open's save while
    /// one is open, builds that rebase the saved map): each close records
    /// the outcome and the digest the oracle gives, and saves its bytes; the
    /// builder replays every save to the same digest.
    #[test]
    fn a_close_decides_as_the_oracle_does() {
        let mut closed = Vec::new();
        for seed in 1..64 {
            two_opens(seed, false, &mut closed);
        }
        // The histories reach every outcome a named file's close has.
        assert!(
            closed
                .iter()
                .any(|o| matches!(o, SaveOutcome::Saved { .. }))
        );
        assert!(closed.contains(&SaveOutcome::Unchanged));
    }

    /// The histories where the builder's check of a save once disagreed
    /// with the actor's: a close that applies part of a page write whose
    /// rest an earlier close saved, with the saved map on its frames on one
    /// side and rebased onto the checkout on the other.
    #[test]
    fn seed_11_builds_every_save() {
        two_opens(11, false, &mut Vec::new());
    }

    #[test]
    fn seed_19_builds_every_save() {
        two_opens(19, false, &mut Vec::new());
    }

    #[test]
    fn seed_26_builds_every_save_with_a_pause_before_each_close() {
        two_opens(26, true, &mut Vec::new());
    }

    #[test]
    fn seeds_11_and_19_build_every_save_with_a_pause_before_each_close() {
        two_opens(11, true, &mut Vec::new());
        two_opens(19, true, &mut Vec::new());
    }

    use crate::tree_actor::journal::readback::READERS;

    fn readers() -> u64 {
        READERS.with(std::cell::Cell::get)
    }

    /// A4 and A6: an open whose saved map did not move and whose length
    /// changed closes in one step under the lock, without the journal: no
    /// reader, however many calls it made (100,000 overwrites of a small
    /// range and one length change: few pieces).
    #[test]
    fn a_close_whose_base_did_not_move_reads_no_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let mut v = new_actor(tmp.path());
        let w = writer("w");
        let h = v.write_open(TreeId::Root, PATH, &w, Some(ATTRS)).unwrap();
        v.write(h, 0, b"0123456789", false).unwrap();
        v.close(h).unwrap();
        let h = v.write_open(TreeId::Root, PATH, &w, None).unwrap();
        for n in 0..100_000_u64 {
            v.write(h, n % 8, &[n as u8], false).unwrap();
        }
        v.write(h, 0, b"longer", true).unwrap();
        let before = readers();
        let saved = v.close(h).unwrap();
        assert!(matches!(saved, Saved::Commit(_)), "{saved:?}");
        assert_eq!(readers(), before, "a reader taken");
        assert_eq!(v.read(TreeId::Root, PATH).unwrap().len(), 16);
        // The same file with its saved map moved meanwhile reads its calls back.
        let a = v.write_open(TreeId::Root, PATH, &w, None).unwrap();
        let b = v.write_open(TreeId::Root, PATH, &w, None).unwrap();
        v.write(a, 0, b"a", true).unwrap();
        v.write(b, 0, b"b", true).unwrap();
        v.close(b).unwrap();
        let before = readers();
        v.close(a).unwrap();
        assert_eq!(readers(), before + 1);
        assert_eq!(v.read(TreeId::Root, PATH).unwrap().len(), 18);
    }
}
