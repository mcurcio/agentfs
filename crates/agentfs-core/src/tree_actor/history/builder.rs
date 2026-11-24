//! The builder: makes each durable step's commits behind the call path, on
//! its own thread (docs/trees.md, "History trails the files"; git-model §12).
//!
//! It reads the journal's durable records from its own cursor, replays each
//! step on the built trees with the transitions the actor ran at the call
//! ([`Shape::apply`], [`pages::close_calls`], [`Call::apply`]), feeds the
//! commits to one `git fast-import` per batch, and installs the batch in this
//! order: pack, then index (git's own), the pack files and directory fsynced,
//! then the built-log entry fsynced. Only then are refs, `config` and the
//! checkouts copied and the built event sent. It decides nothing: outcomes,
//! dates and take decisions are in the records. A save is built from its
//! recorded layout: when the builder's own replay of it does not match, the
//! commit is flagged and building goes on ([`super::verify`]); only a layout
//! that cannot be laid (`Disagrees`) stops building with the alarm.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Bound;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentfs_contracts::pod::{PodContext, WriterAt};
use agentfs_contracts::volume::VolumeIdentity;
use git2::Oid;

use super::built::{BuildError, BuiltObjects, History, Owners};
use super::built_log::{BuiltEntry, BuiltLog, BuiltValue};
use super::feed::{BuildRef, CommitSpec, Feed, Parent};
use super::format::{self, BlobRef};
use super::message::{self, Action, Message};
use super::upstreams::Upstreams;
use super::verify::{self, Evidence, Flag, Inputs, Verification};
use super::{Objects, TreeMoved, checkout, objects_after, objects_of, refs};
use crate::tree_actor::journal::durable::{Durability, RecordNo};
use crate::tree_actor::journal::readback::CallReader;
use crate::tree_actor::journal::segment::{FramePos, Relocations, Segment};
use crate::tree_actor::journal::{
    At, Change, Entry, Frame, Record, RowRecord, SaveOutcome, Tail, TakeHead, TakeRecord, Target,
    chunk_map, frame_at, link_map, runs_from_frame, take_chunks,
};
use crate::tree_actor::pointers::Pointers;
use crate::tree_actor::queue::Key;
use crate::tree_actor::recording::file_map::BaseFile;
use crate::tree_actor::recording::layout::{Layout, Positions};
use crate::tree_actor::recording::{Call, FileMap, pages};
use crate::tree_actor::stages::{Timer, Which};
use crate::tree_actor::tree::{
    EventNumber, FileBytes, Lines, MergeFileIdentity, NameHistory, Tree, Versions,
};
use crate::tree_actor::{Error, Result, TreeId};
use crate::tree_git::{RefUpdate, TreeGit};
use crate::tree_reconcile::{Window, histogram_files};

/// The most steps one batch builds: a batch's built event wakes the lag gate,
/// so a long backlog is built in slices that each end in one.
const MAX_BATCH_STEPS: usize = 256;

/// Where a reader stands: a segment and a byte in it.
type ReadAt = (Arc<Segment>, u64);

/// The builder's own reader of the journal: frames after its cursor, through
/// the durable mark, along the segments in order. A segment it holds stays
/// readable after compaction drops it; carried frames and checkpoints number
/// at or below the cursor and are passed over.
pub(in crate::tree_actor) struct JournalReader {
    durability: Arc<Durability>,
    salt: u32,
    segment: Arc<Segment>,
    offset: u64,
    cursor: RecordNo,
}

impl JournalReader {
    /// A reader of the journal whose cursor is `cursor`: the next read starts
    /// after it.
    pub fn open(durability: Arc<Durability>, salt: u32, cursor: RecordNo) -> Result<Self> {
        let segment = durability
            .segments()
            .first()
            .cloned()
            .ok_or(Error::CorruptJournal(0))?;
        let mut reader = Self {
            durability,
            salt,
            segment,
            offset: 0,
            cursor,
        };
        // Past every frame at or below the cursor.
        loop {
            match frame_at(&reader.segment, reader.offset, salt, cursor)? {
                At::Frame(_, len) => reader.offset += len,
                At::Later => break,
                At::End => {
                    if !reader.next_segment() {
                        break;
                    }
                }
            }
        }
        Ok(reader)
    }

    /// Moves to the segment after the one read, if the journal has one.
    fn next_segment(&mut self) -> bool {
        let first = self.segment.first();
        match self
            .durability
            .segments()
            .into_iter()
            .find(|s| s.first() > first)
        {
            Some(next) => {
                self.segment = next;
                self.offset = 0;
                true
            }
            None => false,
        }
    }

    /// Frames after the cursor through `through`, never past the durable
    /// mark, stopping after `max_steps` steps, and where they end. The cursor
    /// moves only at [`JournalReader::advance`].
    pub fn read(&mut self, through: RecordNo, max_steps: usize) -> Result<(Vec<Frame>, ReadAt)> {
        // A record past the mark may yet be torn: nothing is built from it.
        let through = through.min(self.durability.mark());
        let mut frames = Vec::new();
        let mut steps = 0;
        let (start, start_offset) = (Arc::clone(&self.segment), self.offset);
        while steps < max_steps {
            match frame_at(&self.segment, self.offset, self.salt, through)? {
                At::Frame(frame, len) => {
                    self.offset += len;
                    if frame.0.seq > self.cursor {
                        steps += usize::from(matches!(frame.0.entry, Entry::Step { .. }));
                        frames.push(*frame);
                    }
                }
                At::Later => break,
                At::End => {
                    if !self.next_segment() {
                        break;
                    }
                }
            }
        }
        let end = (Arc::clone(&self.segment), self.offset);
        // Nothing moves until the batch is built.
        self.segment = start;
        self.offset = start_offset;
        Ok((frames, end))
    }

    /// The batch read up to `end`, through `through`, is built.
    pub fn advance(&mut self, end: ReadAt, through: RecordNo) {
        (self.segment, self.offset) = end;
        self.cursor = self.cursor.max(through);
    }
}

/// A write-open as the builder knows it.
#[derive(Clone)]
struct BuildOpen {
    tree: TreeId,
    object: u64,
    calls: imbl::Vector<(u64, Call, FramePos)>,
}

/// What the builder has built: the trees with line maps and name histories,
/// the pointers as built (a batch's own commits as marks until it finishes),
/// the live opens' calls and page writes.
///
/// Every map that grows with the volume is persistent, so a batch's copy
/// (`next = state.clone()`) is O(1) and shares every node it does not
/// change: a batch allocates in proportion to its own steps, and a failed
/// batch is still dropped whole.
#[derive(Clone)]
pub(in crate::tree_actor) struct BuildState {
    trees: imbl::OrdMap<TreeId, Tree>,
    /// O(refs), copied as it is.
    pointers: Pointers<Parent>,
    upstreams: Upstreams,
    opens: imbl::OrdMap<u64, BuildOpen>,
    pages: imbl::OrdMap<(TreeId, u64), imbl::Vector<(u64, pages::Run)>>,
    /// Each direct save's commit, as built: History's snapshot, shared.
    owners: Owners,
    /// Each step's head, for a later record naming it (`Target::Step`).
    heads: imbl::OrdMap<RecordNo, Parent>,
    /// Trees whose head's tree is not known to match the built tree (after a
    /// fast-forward): the next commit lists the whole tree.
    whole: imbl::OrdSet<TreeId>,
    /// Each tree's object numbers as built, as History publishes them.
    objects: BuiltObjects,
    /// Chunk records no take has taken yet.
    bytes: imbl::Vector<(u64, Tail)>,
    /// Files whose built map still reads journal frames: what rebase visits.
    framed: imbl::OrdSet<(TreeId, u64)>,
}

/// A map for a file the builder's state just took: a save, a create, a take.
fn saved_map(map: FileMap, step: u64) -> Arc<FileBytes> {
    let mut map = map;
    map.saved_at(RecordNo::new(step));
    Arc::new(map.into())
}

/// One batch's packer and what it fed.
struct Batch<'a> {
    repo: &'a TreeGit,
    marks: &'a Path,
    feed: Option<Feed>,
    /// Blobs fed this batch, by the bytes they hold.
    blobs: HashMap<*const FileBytes, (Arc<FileBytes>, u64)>,
    /// Every `Target::Step` the batch's records name, resolved.
    values: Vec<(RecordNo, String, Parent)>,
    /// The last commit each scratch ref got.
    scratch: BTreeMap<BuildRef, u64>,
    /// This batch's direct saves, by their commit's mark: History's owners
    /// once the pack is installed.
    owners: BTreeMap<EventNumber, u64>,
    /// A fork made or dropped: `config`'s branch merges change.
    forked: bool,
    /// Trees whose names or objects a step moved.
    moved: BTreeSet<TreeId>,
    /// Files whose map a step replaced: History hands the new maps to the actor.
    files: BTreeSet<(TreeId, u64)>,
    /// This batch's flagged saves, by their commit's mark.
    flags: Vec<Flagged>,
}

/// A save the batch flagged: its commit's mark, what its log line and its
/// evidence say, and when it was recorded.
struct Flagged {
    mark: u64,
    step: RecordNo,
    tree: TreeId,
    path: String,
    flag: Flag,
    evidence: Evidence,
    at_ns: u64,
}

impl Batch<'_> {
    fn feed(&mut self) -> Result<&mut Feed> {
        if self.feed.is_none() {
            self.feed = Some(Feed::start(self.repo, self.marks).map_err(build_error)?);
        }
        self.feed
            .as_mut()
            .ok_or_else(|| Error::Io(std::io::Error::other("no feed")))
    }

    fn blob(&mut self, bytes: &Arc<FileBytes>) -> Result<BlobRef> {
        if let Some(oid) = bytes.known_blob() {
            return Ok(BlobRef::Stored(oid));
        }
        let key = Arc::as_ptr(bytes);
        if let Some((_, mark)) = self.blobs.get(&key) {
            return Ok(BlobRef::Mark(*mark));
        }
        let mark = self
            .feed()?
            .blob_from(bytes.len(), bytes.reader())
            .map_err(build_error)?;
        self.blobs.insert(key, (Arc::clone(bytes), mark));
        Ok(BlobRef::Mark(mark))
    }
}

/// A build failure as the actor's error, carried out of the batch.
fn build_error(e: BuildError) -> Error {
    Error::Io(std::io::Error::other(BuildFailure(e)))
}

#[derive(Debug)]
struct BuildFailure(BuildError);

impl std::fmt::Display for BuildFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for BuildFailure {}

/// The build error an actor error stands for.
fn to_build_error(e: Error) -> BuildError {
    if let Error::Io(io) = &e
        && let Some(inner) = io.get_ref()
        && let Some(BuildFailure(error)) = inner.downcast_ref::<BuildFailure>()
    {
        return error.clone();
    }
    e.into()
}

fn disagrees(step: u64) -> Error {
    build_error(BuildError::Disagrees(RecordNo::new(step)))
}

/// A recorded layout that could not be read or laid: [`disagrees`]; any
/// other failure as it is.
fn unlaid(e: Error, step: u64) -> Error {
    match e {
        Error::Format => disagrees(step),
        e => e,
    }
}

impl BuildState {
    /// Moves every position the state holds (live opens' calls, held page
    /// writes, take chunks, file maps) to where compaction carried its
    /// frame, letting go of the dropped segments.
    ///
    /// Only a file in `framed` reads frames, so only those maps are visited,
    /// and only what moved is changed (a persistent map copies what changes).
    fn relocate(&mut self, moved: &Relocations) {
        let carried = |pos: &FramePos| pos.relocated(moved).is_some();
        let opens: Vec<u64> = self
            .opens
            .iter()
            .filter(|(_, open)| open.calls.iter().any(|(_, _, pos)| carried(pos)))
            .map(|(handle, _)| *handle)
            .collect();
        for handle in opens {
            if let Some(open) = self.opens.get_mut(&handle) {
                for (_, _, pos) in open.calls.iter_mut() {
                    pos.relocate(moved);
                }
            }
        }
        let held: Vec<(TreeId, u64)> = self
            .pages
            .iter()
            .filter(|(_, runs)| runs.iter().any(|(_, run)| carried(&run.at)))
            .map(|(key, _)| *key)
            .collect();
        for key in held {
            if let Some(runs) = self.pages.get_mut(&key) {
                for (_, run) in runs.iter_mut() {
                    run.at.relocate(moved);
                }
            }
        }
        if self.bytes.iter().any(|(_, tail)| carried(&tail.pos)) {
            for (_, tail) in self.bytes.iter_mut() {
                tail.pos.relocate(moved);
            }
        }
        let framed: Vec<(TreeId, u64)> = self.framed.iter().copied().collect();
        for (tree, object) in framed {
            let Some(t) = self.trees.get(&tree) else {
                continue;
            };
            let Some(bytes) = t.bytes.get(&object) else {
                continue;
            };
            let mut map = (**bytes).clone();
            if map.relocate(moved) {
                let relocated = Arc::new(bytes.with_map(map));
                if let Some(t) = self.trees.get_mut(&tree) {
                    t.bytes.insert(object, relocated);
                }
            }
        }
    }

    /// The state at a built point: the trees its pointers' heads hold, the
    /// opens, calls, page writes and take chunks live then.
    #[allow(
        clippy::too_many_arguments,
        reason = "what a start reads, passed through"
    )]
    pub(in crate::tree_actor) fn at(
        trees: BTreeMap<TreeId, Tree>,
        pointers: &Pointers,
        upstreams: Upstreams,
        opens: &BTreeMap<u64, crate::tree_actor::journal::FoldedOpen>,
        pages: BTreeMap<(TreeId, u64), Vec<(u64, pages::Run)>>,
        bytes: Vec<(u64, Tail)>,
        owners: &Owners,
        heads: BTreeMap<RecordNo, Oid>,
    ) -> Result<Self> {
        let mut trees = trees;
        let objects = trees
            .iter()
            .map(|(id, tree)| Ok((*id, objects_of(&tree.shape)?)))
            .collect::<Result<_>>()?;
        for folded in opens.values() {
            if folded.open.anonymous {
                trees
                    .get_mut(&folded.open.tree)
                    .ok_or(Error::Format)?
                    .add_anonymous(
                        folded.open.object,
                        folded.open.meta.ok_or(Error::Format)?,
                        0,
                    );
            }
        }
        let framed = trees
            .iter()
            .flat_map(|(id, tree)| {
                tree.bytes
                    .iter()
                    .filter(|(_, bytes)| bytes.on_frames())
                    .map(|(object, _)| (*id, *object))
            })
            .collect();
        Ok(Self {
            trees: trees.into_iter().collect(),
            pointers: Pointers::new(
                pointers
                    .iter()
                    .map(|(name, oid)| (name.clone(), Parent::Built(*oid)))
                    .collect(),
            ),
            upstreams,
            opens: opens
                .iter()
                .map(|(handle, folded)| {
                    (
                        *handle,
                        BuildOpen {
                            tree: folded.open.tree,
                            object: folded.open.object,
                            calls: folded.calls.iter().cloned().collect(),
                        },
                    )
                })
                .collect(),
            pages: pages
                .into_iter()
                .map(|(key, runs)| (key, imbl::Vector::from(runs)))
                .collect(),
            owners: owners.clone(),
            heads: heads
                .into_iter()
                .map(|(step, oid)| (step, Parent::Built(oid)))
                .collect(),
            whole: imbl::OrdSet::new(),
            objects,
            bytes: bytes.into_iter().collect(),
            framed,
        })
    }

    fn tree(&self, tree: TreeId) -> Result<&Tree> {
        self.trees.get(&tree).ok_or(Error::Format)
    }

    /// Each tree's objects as built: the published map, else (a tree no
    /// batch has moved yet) its shape's.
    fn objects_of_tree(&self, tree: TreeId, t: &Tree) -> Result<Objects> {
        match self.objects.get(&tree) {
            Some(objects) => Ok(objects.clone()),
            None => objects_of(&t.shape),
        }
    }

    /// What `next`, this state after a batch, moved of each tree's object
    /// numbers and locals: the built-log entry's `moved`. Visits the diffs of
    /// the persistent maps only (O(trees) plus what moved).
    fn moved_to(&self, next: &Self) -> Result<Vec<TreeMoved>> {
        let trees: BTreeSet<TreeId> = self
            .trees
            .keys()
            .chain(next.trees.keys())
            .copied()
            .collect();
        let mut moved = Vec::new();
        for tree in trees {
            let before = match self.trees.get(&tree) {
                Some(t) => Some((t, self.objects_of_tree(tree, t)?)),
                None => None,
            };
            let after = match next.trees.get(&tree) {
                Some(t) => Some((t, next.objects_of_tree(tree, t)?)),
                None => None,
            };
            let change = TreeMoved::between(
                tree,
                before.as_ref().map(|(t, o)| (&t.shape, o)),
                after.as_ref().map(|(t, o)| (&t.shape, o)),
            )?;
            // The unit tests' check, at every batch: the moves carry the
            // before-state's objects and locals to the after-state's.
            #[cfg(test)]
            {
                let (mut objects, mut locals) = (BTreeMap::new(), BTreeMap::new());
                if let Some((t, _)) = &before {
                    objects.insert(tree, objects_of(&t.shape)?);
                    locals.insert(tree, t.shape.locals());
                }
                if let Some(change) = &change {
                    change.apply(&mut objects, &mut locals)?;
                }
                let (mut want_objects, mut want_locals) = (BTreeMap::new(), BTreeMap::new());
                if let Some((t, _)) = &after {
                    want_objects.insert(tree, objects_of(&t.shape)?);
                    want_locals.insert(tree, t.shape.locals());
                }
                assert_eq!(
                    (objects, locals),
                    (want_objects, want_locals),
                    "{tree}: a batch's moves disagree with its trees"
                );
            }
            moved.extend(change);
        }
        Ok(moved)
    }

    fn head(&self, tree: TreeId) -> Result<Parent> {
        self.pointers.get(&refs::branch(tree)).ok_or(Error::Format)
    }

    /// One record of a batch.
    fn apply(
        &mut self,
        record: &Record,
        tail: &Tail,
        ctx: &Context<'_>,
        batch: &mut Batch<'_>,
    ) -> Result<()> {
        let seq = record.seq.get();
        match &record.entry {
            Entry::Checkpoint { .. } => return Err(Error::Format),
            Entry::Open { tree, object, .. } => {
                let tree = tree.parse::<TreeId>()?;
                self.opened(seq, tree, *object);
            }
            Entry::AnonymousOpen { tree, meta, .. } => {
                let tree = tree.parse::<TreeId>()?;
                self.trees
                    .get_mut(&tree)
                    .ok_or(Error::Format)?
                    .add_anonymous(seq, *meta, record.at_ns);
                self.opened(seq, tree, seq);
            }
            Entry::Call { handle, call } => {
                call.check_tail(tail.len)?;
                self.opens
                    .get_mut(handle)
                    .ok_or(Error::Format)?
                    .calls
                    .push_back((seq, call.clone(), tail.pos.clone()));
            }
            // A close's start builds nothing: its save step does.
            Entry::CloseBegun { .. } => {}
            Entry::PageWrite { tree, object, runs } => self
                .pages
                .entry((tree.parse::<TreeId>()?, *object))
                .or_default()
                .extend(runs_from_frame(runs, tail)?.into_iter().map(|r| (seq, r))),
            Entry::Bytes => self.bytes.push_back((seq, tail.clone())),
            Entry::Step {
                by,
                change,
                pointers,
                rows,
            } => self.step(record, tail, by, (change, rows), pointers, ctx, batch)?,
            // The builder's pointers are built values already.
            Entry::Switch { .. } => {}
        }
        Ok(())
    }

    fn opened(&mut self, handle: u64, tree: TreeId, object: u64) {
        self.opens.insert(
            handle,
            BuildOpen {
                tree,
                object,
                calls: imbl::Vector::new(),
            },
        );
        self.pages.entry((tree, object)).or_default();
    }

    /// A commit of the tree as it now is over `parents`: the edits of the
    /// paths and identities in `changed`, or the whole tree after a move the
    /// built tree is not known to match.
    #[allow(
        clippy::too_many_arguments,
        reason = "the commit's parts, passed through"
    )]
    fn commit(
        &mut self,
        tree: TreeId,
        parents: &[Parent],
        changed: &BTreeSet<Key>,
        message: &str,
        at_ns: u64,
        batch: &mut Batch<'_>,
    ) -> Result<Parent> {
        let whole = self.whole.remove(&tree).is_some();
        let after = self.trees.get(&tree).ok_or(Error::Format)?;
        let edits = format::edits(after, (!whole).then_some(changed), |bytes| {
            batch.blob(bytes)
        })?;
        let mark = batch
            .feed()?
            .commit(&CommitSpec {
                on: BuildRef::Tree(tree),
                parents,
                at_ns,
                message,
                edits: &edits,
            })
            .map_err(build_error)?;
        batch.scratch.insert(BuildRef::Tree(tree), mark);
        Ok(Parent::Mark(mark))
    }

    /// Each row of a step, in order, as an event commit over its chain's
    /// tip in the batch (none: a root commit), dated by the step's record.
    /// Returns each chain's last commit in this step.
    fn commit_rows(
        &mut self,
        record: &Record,
        rows: &[RowRecord],
        ctx: &Context<'_>,
        batch: &mut Batch<'_>,
    ) -> Result<BTreeMap<String, Parent>> {
        let mut tips = BTreeMap::new();
        for row in rows {
            let chain = refs::chain_of_row(&row.chain).ok_or(Error::Format)?;
            let message = Message::Row {
                row,
                chain,
                event: EventNumber::new(record.seq.get()),
            }
            .render(ctx.identity)?;
            let parents: Vec<Parent> = self.pointers.get(&row.chain).into_iter().collect();
            let mark = batch
                .feed()?
                .commit(&CommitSpec {
                    on: BuildRef::Rows,
                    parents: &parents,
                    at_ns: record.at_ns,
                    message: &message,
                    edits: &format::row_edits(&row.row),
                })
                .map_err(build_error)?;
            batch.scratch.insert(BuildRef::Rows, mark);
            self.pointers.set(&row.chain, Some(Parent::Mark(mark)));
            tips.insert(row.chain.clone(), Parent::Mark(mark));
        }
        Ok(tips)
    }

    /// A direct step's commit over the tree's head.
    fn commit_direct(
        &mut self,
        tree: TreeId,
        changed: &BTreeSet<Key>,
        message: &str,
        at_ns: u64,
        batch: &mut Batch<'_>,
    ) -> Result<Parent> {
        let parent = self.head(tree)?;
        self.commit(tree, &[parent], changed, message, at_ns, batch)
    }

    fn tree_mut(&mut self, tree: TreeId) -> Result<&mut Tree> {
        self.trees.get_mut(&tree).ok_or(Error::Format)
    }

    /// One step, applied to the built trees in place: a failed batch drops
    /// the copy it built on.
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per kind of step, as the record has"
    )]
    #[allow(
        clippy::too_many_arguments,
        reason = "the record's parts, passed through"
    )]
    fn step(
        &mut self,
        record: &Record,
        tail: &Tail,
        by: &WriterAt,
        (change, rows): (&Change, &[RowRecord]),
        changes: &[crate::tree_actor::journal::PointerChange],
        ctx: &Context<'_>,
        batch: &mut Batch<'_>,
    ) -> Result<()> {
        let seq = record.seq.get();
        let event = EventNumber::new(seq);
        let direct = |action: Action, tree: TreeId, open: Option<u64>| {
            Message::Direct {
                action,
                by,
                tree,
                event,
                open,
                unverified: None,
            }
            .render(ctx.identity)
        };
        // The commit this step makes or moves its tree's head to.
        let mut head: Option<Parent> = None;
        let mut owner = false;
        let at_ns = record.at_ns;
        let dir_rename = match change {
            Change::Rename { tree, from, to } if from != to => self
                .tree(tree.parse::<TreeId>()?)?
                .dirs
                .contains_key(from.as_str()),
            _ => false,
        };
        match change {
            Change::Create { tree, path, .. } => {
                let id = tree.parse::<TreeId>()?;
                let t = self.tree_mut(id)?;
                let keys = t.shape.apply(change, event, at_ns)?;
                let object = event.object();
                t.bytes.insert(object, saved_map(FileMap::empty(), seq));
                t.lines.insert(object, Arc::default());
                batch.files.insert((id, object));
                let identity = t.identity_of(object).ok_or(Error::Format)?;
                t.namespace
                    .insert(identity, NameHistory::created(event, path.clone()));
                let message = direct(Action::Create, id, None)?;
                head = Some(self.commit_direct(id, &keys, &message, record.at_ns, batch)?);
                self.opened(seq, id, object);
                owner = true;
                batch.moved.insert(id);
            }
            Change::Symlink { tree, path, .. } => {
                let id = tree.parse::<TreeId>()?;
                let t = self.tree_mut(id)?;
                let keys = t.shape.apply(change, event, at_ns)?;
                let object = event.object();
                let map = link_map(tail, seq)?;
                let mut versions = Versions::of(event);
                let lines = Lines::default()
                    .append(&Window::new(&map, 0, map.len()), |n| versions.take(n))?;
                t.bytes.insert(object, Arc::new(map.into()));
                t.lines.insert(object, Arc::new(lines));
                batch.files.insert((id, object));
                let identity = t.identity_of(object).ok_or(Error::Format)?;
                t.namespace
                    .insert(identity, NameHistory::created(event, path.clone()));
                let message = direct(Action::Symlink, id, None)?;
                head = Some(self.commit_direct(id, &keys, &message, at_ns, batch)?);
                owner = true;
                batch.moved.insert(id);
            }
            Change::Mkdir { tree, .. }
            | Change::Rmdir { tree, .. }
            | Change::SetMeta { tree, .. } => {
                let id = tree.parse::<TreeId>()?;
                let keys = self.tree_mut(id)?.shape.apply(change, event, at_ns)?;
                let action = match change {
                    Change::Mkdir { .. } => Action::Mkdir,
                    Change::Rmdir { .. } => Action::Rmdir,
                    _ => Action::Meta,
                };
                let message = direct(action, id, None)?;
                head = Some(self.commit_direct(id, &keys, &message, at_ns, batch)?);
                owner = true;
                batch.moved.insert(id);
            }
            // Times are each tree's own: no commit.
            Change::SetTimes { tree, .. } => {
                let id = tree.parse::<TreeId>()?;
                self.tree_mut(id)?.shape.apply(change, event, at_ns)?;
            }
            Change::Link { tree, object, path } => {
                let id = tree.parse::<TreeId>()?;
                let t = self.tree_mut(id)?;
                let keys = t.shape.apply(change, event, at_ns)?;
                let identity = t.identity_of(*object).ok_or_else(|| disagrees(seq))?;
                t.namespace
                    .insert(identity, NameHistory::created(event, path.clone()));
                let message = direct(Action::Create, id, None)?;
                head = Some(self.commit_direct(id, &keys, &message, record.at_ns, batch)?);
                owner = true;
                batch.moved.insert(id);
            }
            Change::Save {
                tree,
                handle,
                pages: pieces,
                outcome,
            } => {
                let id = tree.parse::<TreeId>()?;
                let open = self.opens.remove(handle).ok_or_else(|| disagrees(seq))?;
                if open.tree != id {
                    return Err(disagrees(seq));
                }
                let t = self.trees.get_mut(&id).ok_or(Error::Format)?;
                let keys = t.shape.save(open.object, *outcome, at_ns);
                match outcome {
                    SaveOutcome::AnonymousDropped => {
                        t.bytes.remove(&open.object);
                        t.lines.remove(&open.object);
                        self.framed.remove(&(id, open.object));
                    }
                    SaveOutcome::Unchanged | SaveOutcome::OrphanDropped => {}
                    SaveOutcome::Saved { len, chunks } => {
                        let chunks = self.take_chunks(seq, *chunks)?;
                        // The record wins: a layout that cannot be read or
                        // laid is the only disagreement that stops building.
                        let recorded = Layout::read(*len, &chunks).map_err(|e| unlaid(e, seq))?;
                        let writes: Vec<(u64, pages::Run)> = self
                            .pages
                            .get(&(id, open.object))
                            .map(|runs| runs.iter().cloned().collect())
                            .unwrap_or_default();
                        let t = self.trees.get_mut(&id).ok_or(Error::Format)?;
                        let saved = t.bytes.get(&open.object).ok_or_else(|| disagrees(seq))?;
                        // The open's calls are read where they are held.
                        let replayed =
                            Layout::replay(saved.len(), open.calls.iter(), &writes, pieces)?;
                        // Each place from the calls and runs tracked here,
                        // else from the journal by its record.
                        let mut known = Positions::default();
                        for (_, _, pos) in open.calls.iter() {
                            known.add(pos);
                        }
                        for (_, run) in &writes {
                            known.add(&run.at);
                        }
                        let mut journal: Option<CallReader> = None;
                        let map = recorded
                            .lay(saved, &mut |place| {
                                if let Some(pos) = known.find(place) {
                                    return Ok(Some(pos));
                                }
                                let reader = match &mut journal {
                                    Some(reader) => reader,
                                    empty => empty.insert(CallReader::of_segments(
                                        ctx.durability.segments(),
                                        ctx.salt,
                                    )?),
                                };
                                Ok(reader
                                    .record(place.record)?
                                    .map(|(_, tail)| tail.pos.at(place.at)))
                            })
                            .map_err(|e| unlaid(e, seq))?;
                        let base = t.bytes.get(&open.object).ok_or_else(|| disagrees(seq))?;
                        let old = t.lines.get(&open.object).ok_or_else(|| disagrees(seq))?;
                        let appended = recorded.only_extends(base.len());
                        let before_len = base.len();
                        let lines = saved_lines(ctx.root, old, base, &map, appended, event)?;
                        t.lines.insert(open.object, Arc::new(lines));
                        t.bytes.insert(open.object, saved_map(map, seq));
                        batch.files.insert((id, open.object));
                        let flag = (replayed.as_ref() != Some(&recorded)).then(|| Flag {
                            recorded: recorded.fingerprint(),
                            replayed: replayed.as_ref().map(Layout::fingerprint),
                            builder: verify::release_or_unknown(),
                        });
                        let message = Message::Direct {
                            action: Action::Direct,
                            by,
                            tree: id,
                            event,
                            open: Some(*handle),
                            unverified: flag.as_ref(),
                        }
                        .render(ctx.identity)?;
                        let commit =
                            self.commit_direct(id, &keys, &message, record.at_ns, batch)?;
                        if let (Some(flag), Parent::Mark(mark)) = (flag, &commit) {
                            let path = self
                                .tree(id)?
                                .names
                                .of(open.object)
                                .next()
                                .unwrap_or_default()
                                .to_owned();
                            let evidence = Evidence::of(&Inputs {
                                step: record.seq,
                                handle: *handle,
                                tree: id,
                                object: open.object,
                                path: &path,
                                before_len,
                                own: &open.calls,
                                writes: &writes,
                                pieces,
                                recorded: &recorded,
                                replayed: replayed.as_ref(),
                            });
                            batch.flags.push(Flagged {
                                mark: *mark,
                                step: record.seq,
                                tree: id,
                                path,
                                flag,
                                evidence,
                                at_ns: record.at_ns,
                            });
                        }
                        head = Some(commit);
                        owner = true;
                    }
                }
                if !self
                    .opens
                    .values()
                    .any(|o| o.tree == id && o.object == open.object)
                {
                    self.pages.remove(&(id, open.object));
                }
            }
            Change::Rename { tree, from, to } if dir_rename => {
                // A directory's rename: each file under it gets a new name version.
                let id = tree.parse::<TreeId>()?;
                let t = self.tree_mut(id)?;
                let prefix = format!("{from}/");
                let moved: Vec<(String, u64)> = t
                    .names
                    .range::<_, str>((Bound::Included(prefix.as_str()), Bound::Unbounded))
                    .take_while(|(path, _)| path.starts_with(&prefix))
                    .map(|(path, object)| (path.to_string(), *object))
                    .collect();
                for (path, object) in moved {
                    let identity = t.identity_of(object).ok_or(Error::Format)?;
                    t.namespace
                        .get_mut(&identity)
                        .ok_or(Error::Format)?
                        .advance(event, Some(format!("{to}{}", &path[from.len()..])), None);
                }
                let keys = t.shape.apply(change, event, at_ns)?;
                let message = direct(Action::Rename, id, None)?;
                head = Some(self.commit_direct(id, &keys, &message, at_ns, batch)?);
                owner = true;
                batch.moved.insert(id);
            }
            Change::Rename { tree, from, to } => {
                let id = tree.parse::<TreeId>()?;
                let t = self.tree_mut(id)?;
                let object = *t.names.get(from.as_str()).ok_or_else(|| disagrees(seq))?;
                let source = t.identity_of(object).ok_or_else(|| disagrees(seq))?;
                match t.names.get(to.as_str()).copied() {
                    Some(replaced) if t.is_fresh(object) => {
                        t.namespace
                            .get_mut(&source)
                            .ok_or(Error::Format)?
                            .advance(event, None, None);
                        let mut versions = Versions::of(event);
                        let lines = align(
                            ctx.root,
                            t.lines.get(&replaced).ok_or(Error::Format)?,
                            t.bytes.get(&replaced).ok_or(Error::Format)?,
                            t.bytes.get(&object).ok_or(Error::Format)?,
                            &mut versions,
                        )?;
                        t.lines.insert(object, Arc::new(lines));
                    }
                    replaced => {
                        if let Some(replaced) = replaced {
                            let replaced = t.identity_of(replaced).ok_or(Error::Format)?;
                            t.namespace
                                .get_mut(&replaced)
                                .ok_or(Error::Format)?
                                .advance(event, None, Some(source));
                        }
                        t.namespace.get_mut(&source).ok_or(Error::Format)?.advance(
                            event,
                            Some(to.clone()),
                            None,
                        );
                    }
                }
                let keys = t.shape.apply(change, event, at_ns)?;
                let message = direct(Action::Rename, id, None)?;
                head = Some(self.commit_direct(id, &keys, &message, record.at_ns, batch)?);
                owner = true;
                batch.moved.insert(id);
            }
            Change::Unlink { tree, path } => {
                let id = tree.parse::<TreeId>()?;
                let t = self.tree_mut(id)?;
                let object = *t.names.get(path.as_str()).ok_or_else(|| disagrees(seq))?;
                let identity = t.identity_of(object).ok_or(Error::Format)?;
                t.namespace
                    .get_mut(&identity)
                    .ok_or(Error::Format)?
                    .advance(event, None, None);
                let keys = t.shape.apply(change, event, at_ns)?;
                let message = direct(Action::Unlink, id, None)?;
                head = Some(self.commit_direct(id, &keys, &message, record.at_ns, batch)?);
                owner = true;
                batch.moved.insert(id);
            }
            Change::Fork { upstream } => {
                let upstream = upstream.parse::<TreeId>()?;
                let fork = TreeId::Fork(seq);
                let mut t = self.tree(upstream)?.clone();
                for object in std::mem::take(&mut t.shape.anonymous) {
                    t.drop_object(object);
                }
                self.trees.insert(fork, t);
                self.upstreams.add(seq, upstream);
                if self.whole.contains(&upstream) {
                    self.whole.insert(fork);
                }
                // The fork's maps are the upstream's: so are those that read
                // frames, noted or replaced earlier in this batch.
                let fork_tree = self.tree(fork)?;
                let framed: Vec<u64> = self
                    .framed
                    .range((upstream, 0)..=(upstream, u64::MAX))
                    .chain(batch.files.range((upstream, 0)..=(upstream, u64::MAX)))
                    .map(|(_, object)| *object)
                    .filter(|object| {
                        fork_tree
                            .bytes
                            .get(object)
                            .is_some_and(|bytes| bytes.on_frames())
                    })
                    .collect();
                for object in framed {
                    self.framed.insert((fork, object));
                }
                head = Some(self.head(upstream)?);
                batch.forked = true;
                batch.moved.insert(fork);
            }
            Change::Take { tree, take } => {
                let id = tree.parse::<TreeId>()?;
                head = self.take(id, seq, take, change, ctx, record.at_ns, batch)?;
                batch.moved.insert(id);
            }
            // A save of the file there, or a create and a save: built as both.
            Change::Replace {
                tree,
                path,
                len,
                chunks,
                ..
            } => {
                let id = tree.parse::<TreeId>()?;
                let chunks = self.take_chunks(seq, *chunks)?;
                let map = chunk_map(&chunks, *len, record.seq)?;
                let t = self.tree_mut(id)?;
                let existing = t.names.get(path.as_str()).copied();
                let keys = t.shape.apply(change, event, at_ns)?;
                let object = existing.unwrap_or(event.object());
                let lines = match existing {
                    Some(object) => saved_lines(
                        ctx.root,
                        t.lines.get(&object).ok_or_else(|| disagrees(seq))?,
                        t.bytes.get(&object).ok_or_else(|| disagrees(seq))?,
                        &map,
                        false,
                        event,
                    )?,
                    None => {
                        let identity = t.identity_of(object).ok_or(Error::Format)?;
                        t.namespace
                            .insert(identity, NameHistory::created(event, path.clone()));
                        batch.moved.insert(id);
                        saved_lines(
                            ctx.root,
                            &Lines::default(),
                            &FileMap::empty(),
                            &map,
                            false,
                            event,
                        )?
                    }
                };
                t.lines.insert(object, Arc::new(lines));
                t.bytes.insert(object, Arc::new(map.into()));
                batch.files.insert((id, object));
                let message = direct(Action::Direct, id, None)?;
                head = Some(self.commit_direct(id, &keys, &message, at_ns, batch)?);
                owner = true;
            }
            Change::Rows => {}
            Change::Drop { fork } => {
                // Its branch and queue entries go with the step's pointer
                // changes; its checkout at the copy.
                let fork = fork.parse::<TreeId>()?;
                if self.opens.values().any(|o| o.tree == fork) {
                    return Err(disagrees(seq));
                }
                self.trees.remove(&fork).ok_or_else(|| disagrees(seq))?;
                self.upstreams.remove(fork);
                self.objects.remove(&fork);
                self.whole.remove(&fork);
                let held: Vec<(TreeId, u64)> = self
                    .pages
                    .range((fork, 0)..=(fork, u64::MAX))
                    .map(|(key, _)| *key)
                    .collect();
                for key in held {
                    self.pages.remove(&key);
                }
                let framed: Vec<(TreeId, u64)> = self
                    .framed
                    .range((fork, 0)..=(fork, u64::MAX))
                    .copied()
                    .collect();
                for key in framed {
                    self.framed.remove(&key);
                }
                batch.moved.remove(&fork);
                batch.files.retain(|(tree, _)| *tree != fork);
                batch.forked = true;
            }
        }
        if let Some(head) = head {
            self.heads.insert(record.seq, head);
            if owner {
                match head {
                    Parent::Mark(mark) => {
                        batch.owners.insert(event, mark);
                    }
                    Parent::Built(oid) => {
                        self.owners.insert(event, oid);
                    }
                }
            }
        }
        // After the change, its rows: a row chain's `Step(n)` is step `n`'s
        // last commit on that chain, never the step's head.
        let row_tips = self.commit_rows(record, rows, ctx, batch)?;
        for pointer in changes {
            let value = match &pointer.new {
                None => None,
                Some(Target::Oid(hex)) => Some(Parent::Built(
                    Oid::from_str(hex).map_err(|_| Error::Format)?,
                )),
                Some(Target::Step(step)) if refs::chain_of_row(&pointer.name).is_some() => {
                    // The actor names a chain's own step only (`record_step`).
                    let value = *row_tips
                        .get(&pointer.name)
                        .filter(|_| *step == record.seq)
                        .ok_or_else(|| disagrees(seq))?;
                    batch.values.push((*step, pointer.name.clone(), value));
                    Some(value)
                }
                Some(Target::Step(step)) if *step == record.seq => {
                    let value = head.ok_or_else(|| disagrees(seq))?;
                    batch.values.push((*step, pointer.name.clone(), value));
                    Some(value)
                }
                Some(Target::Step(step)) => {
                    let value = *self.heads.get(step).ok_or_else(|| disagrees(seq))?;
                    batch.values.push((*step, pointer.name.clone(), value));
                    Some(value)
                }
            };
            self.pointers.set(&pointer.name, value);
        }
        Ok(())
    }

    /// A take: its decided files and names laid over the tree, and its head.
    #[allow(
        clippy::too_many_arguments,
        reason = "the step's parts, passed through"
    )]
    fn take(
        &mut self,
        id: TreeId,
        seq: u64,
        take: &TakeRecord,
        change: &Change,
        ctx: &Context<'_>,
        at_ns: u64,
        batch: &mut Batch<'_>,
    ) -> Result<Option<Parent>> {
        let event = EventNumber::new(seq);
        let chunks = self.take_chunks(seq, take.chunks)?;
        let current = self.tree(id)?;
        let mut snapshot = current.snapshot();
        for (identity, file) in &take.files {
            match file {
                Some(file) => snapshot.files.insert(*identity, file.clone()),
                None => snapshot.files.remove(identity),
            };
        }
        for (identity, history) in &take.names {
            match history {
                Some(history) => snapshot.names.insert(*identity, history.clone()),
                None => snapshot.names.remove(identity),
            };
        }
        for (path, meta) in &take.dirs {
            match meta {
                Some(meta) => snapshot.dirs.insert(path.clone(), *meta),
                None => snapshot.dirs.remove(path),
            };
        }
        let mut t = current.clone();
        // A file the take leaves reads its bytes from the record's tail; every
        // other file keeps its map.
        let mut maps: BTreeMap<u64, FileMap> = take
            .maps(&chunks, RecordNo::new(seq))?
            .into_iter()
            .collect();
        let mut taken = Vec::new();
        t.install(snapshot, |_, object, _| match maps.remove(&object) {
            Some(map) => {
                taken.push(object);
                Ok(Arc::new(map.into()))
            }
            None => current
                .bytes
                .get(&object)
                .cloned()
                .ok_or_else(|| disagrees(seq)),
        })?;
        batch
            .files
            .extend(taken.into_iter().map(|object| (id, object)));
        // The actor ran the shape's own transition: the two must agree. The
        // tree takes the transition's shape, with its directories' numbers
        // and its times.
        let mut shape = current.shape.clone();
        shape.apply(change, event, at_ns)?;
        if *shape.names != *t.shape.names
            || shape.fresh_objects().ne(t.shape.fresh_objects())
            || shape.dirs.keys().ne(t.shape.dirs.keys())
        {
            return Err(disagrees(seq));
        }
        t.shape = shape;
        // What the commit rewrites: each identity the take changed, and the
        // paths it had and has.
        let identities: BTreeSet<MergeFileIdentity> = take
            .files
            .iter()
            .map(|(id, _)| *id)
            .chain(take.names.iter().map(|(id, _)| *id))
            .collect();
        let mut changed = BTreeSet::new();
        for tree in [current, &t] {
            for (path, object) in tree.names.iter() {
                if tree
                    .identity_of(*object)
                    .is_some_and(|i| identities.contains(&i))
                {
                    changed.insert(Key::Name(path.to_string()));
                }
            }
        }
        changed.extend(identities.into_iter().map(Key::File));
        changed.extend(take.dirs.iter().map(|(path, _)| Key::Name(path.clone())));
        let parent = self.head(id)?;
        self.trees.insert(id, t);
        let head = match &take.head {
            TakeHead::Stay => None,
            TakeHead::FastForward { to } => {
                self.whole.insert(id);
                Some(Parent::Built(Oid::from_str(to).map_err(|_| Error::Format)?))
            }
            TakeHead::Landing { incoming } => {
                let incoming = Oid::from_str(incoming).map_err(|_| Error::Format)?;
                let commit = ctx.repo.read_commit(incoming)?;
                let landed = message::writer_at(&commit.message)?.ok_or(Error::Format)?;
                let message = Message::Landing {
                    landed: &landed,
                    tree: id,
                    event,
                    collisions: &take.collisions,
                }
                .render(ctx.identity)?;
                Some(self.commit(
                    id,
                    &[parent, Parent::Built(incoming)],
                    &changed,
                    &message,
                    at_ns,
                    batch,
                )?)
            }
            TakeHead::Sync { target } => {
                let target = Oid::from_str(target).map_err(|_| Error::Format)?;
                let message = Message::Sync {
                    service: ctx.service,
                    tree: id,
                    event,
                }
                .render(ctx.identity)?;
                Some(self.commit(
                    id,
                    &[parent, Parent::Built(target)],
                    &changed,
                    &message,
                    at_ns,
                    batch,
                )?)
            }
        };
        Ok(head)
    }

    /// Every mark replaced by its object, once the batch's pack is installed:
    /// the pointers (O(refs)), the heads of the batch's steps (`from` on) and
    /// the batch's direct saves (`owners`, by mark).
    fn resolve(
        &mut self,
        marks: &BTreeMap<u64, Oid>,
        from: RecordNo,
        owners: &BTreeMap<EventNumber, u64>,
    ) -> Result<()> {
        let resolve = |value: &mut Parent| -> Result<()> {
            if let Parent::Mark(mark) = *value {
                *value = Parent::Built(*marks.get(&mark).ok_or(Error::Format)?);
            }
            Ok(())
        };
        let mut pointers = BTreeMap::new();
        for (name, value) in self.pointers.iter() {
            let mut value = *value;
            resolve(&mut value)?;
            pointers.insert(name.clone(), value);
        }
        self.pointers = Pointers::new(pointers);
        for (event, mark) in owners {
            self.owners
                .insert(*event, *marks.get(mark).ok_or(Error::Format)?);
        }
        let marked: Vec<RecordNo> = self
            .heads
            .range(from..)
            .filter(|(_, value)| matches!(value, Parent::Mark(_)))
            .map(|(step, _)| *step)
            .collect();
        for step in marked {
            if let Some(value) = self.heads.get_mut(&step) {
                resolve(value)?;
            }
        }
        Ok(())
    }

    /// A take's or replace's chunks ([`take_chunks`]) from the records no
    /// take has taken yet: few, and none at rest.
    fn take_chunks(&mut self, seq: u64, n: u64) -> Result<Vec<Tail>> {
        let mut pending: Vec<(u64, Tail)> = self.bytes.iter().cloned().collect();
        let chunks = take_chunks(&mut pending, seq, n)?;
        self.bytes = pending.into_iter().collect();
        Ok(chunks)
    }

    /// The unit tests' check, at every batch: `framed` holds every built map
    /// that reads frames, so rebase and relocation visit them all.
    #[cfg(test)]
    fn check_framed(&self) {
        for (id, tree) in &self.trees {
            for (object, bytes) in &tree.bytes {
                assert!(
                    !bytes.on_frames() || self.framed.contains(&(*id, *object)),
                    "{id} {object} reads frames but is not noted"
                );
            }
        }
    }

    /// Notes whether each of `files`, whose maps a batch replaced, still
    /// reads frames.
    fn note_framed(&mut self, files: &BTreeSet<(TreeId, u64)>) {
        for (tree, object) in files {
            let framed = self
                .trees
                .get(tree)
                .and_then(|t| t.bytes.get(object))
                .is_some_and(|bytes| bytes.on_frames());
            if framed {
                self.framed.insert((*tree, *object));
            } else {
                self.framed.remove(&(*tree, *object));
            }
        }
    }

    /// The pointers as oids; [`Error::Format`] while a mark is unresolved.
    fn built_pointers(&self) -> Result<Pointers> {
        Ok(Pointers::new(
            self.pointers
                .iter()
                .map(|(name, value)| match value {
                    Parent::Built(oid) => Ok((name.clone(), *oid)),
                    Parent::Mark(_) => Err(Error::Format),
                })
                .collect::<Result<_>>()?,
        ))
    }
}

/// What every step's build reads and never changes.
pub(in crate::tree_actor) struct Context<'a> {
    pub identity: &'a VolumeIdentity,
    pub service: &'a PodContext,
    pub repo: &'a TreeGit,
    /// The state area: where an alignment writes the files `git diff` reads.
    pub root: &'a Path,
    /// The journal's segments and salt: where a save's bytes the builder
    /// does not track are read by record.
    pub durability: &'a Arc<Durability>,
    pub salt: u32,
}

/// The line map of `after`'s bytes over `old`, the map of `before`'s: one
/// histogram alignment, reading both maps a line at a time. Only when the two
/// differ and `before` has bytes are they written to files for `git diff`, in
/// the state area's `align/` (never memory or `/tmp`), and removed after.
fn align(
    root: &Path,
    old: &Lines,
    before: &FileMap,
    after: &FileMap,
    versions: &mut Versions,
) -> Result<Lines> {
    let _timer = Timer::start(Which::Align);
    let dir = root.join("align");
    let diff = || {
        std::fs::create_dir_all(&dir)?;
        let (a, b) = (dir.join("old"), dir.join("new"));
        let written = (|| {
            for (path, map) in [(&a, before), (&b, after)] {
                let mut file = std::fs::File::create(path)?;
                std::io::copy(&mut map.reader(), &mut file)?;
            }
            histogram_files(&a, &b)
        })();
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
        written
    };
    Ok(old.align(before, after, diff, |n| versions.take(n))?)
}

/// The line map a save of step `event` leaves: `after`'s bytes over `old`,
/// the map of `before`'s. Lines a close only appended (`appended`) take new
/// versions after the old ones; any other save is aligned ([`align`]). A
/// close's save and a replace both build their lines here.
fn saved_lines(
    root: &Path,
    old: &Lines,
    before: &FileMap,
    after: &FileMap,
    appended: bool,
    event: EventNumber,
) -> Result<Lines> {
    let mut versions = Versions::of(event);
    if appended {
        let added = Window::new(after, before.len(), after.len() - before.len());
        return Ok(old.append(&added, |n| versions.take(n))?);
    }
    align(root, old, before, after, &mut versions)
}

/// The builder thread's own state.
pub(in crate::tree_actor) struct Builder {
    pub state: BuildState,
    pub reader: JournalReader,
    pub history: History,
    pub durability: Arc<Durability>,
    pub built_log: BuiltLog,
    pub root: PathBuf,
    pub identity: VolumeIdentity,
    pub service: PodContext,
    /// The pointers Git's refs hold.
    pub copied: Pointers,
    /// The files tree each checkout holds.
    pub checked_out: BTreeMap<TreeId, Oid>,
    /// A copy failed: the next batch rebuilds refs, config and checkouts whole.
    pub stale: bool,
    /// The volume's flagged commits by state: each flag counted once its
    /// batch is built.
    pub verification: Verification,
}

impl Builder {
    /// Its own thread: builds each wanted record, retrying a failed batch at
    /// the next request; returns once History asks it to stop.
    pub fn run(mut self) {
        let mut seen = 0;
        while self.history.next_work(&mut seen).is_some() {
            if let Err(e) = self.batch() {
                self.history.failed_event(e, seen);
            }
        }
    }

    /// One batch: every durable record past the built point, built and
    /// installed, or nothing reachable changed.
    fn batch(&mut self) -> std::result::Result<(), BuildError> {
        crate::failpoints::hit("tree.build_before_batch")?;
        #[cfg(test)]
        self.history.batch_read(None);
        let started = std::time::Instant::now();
        let checkpointed = self.history.checkpointed_through();
        if self
            .built_log
            .first_through()
            .is_some_and(|through| through <= checkpointed)
        {
            let keep = self.built_log.entries_after(checkpointed)?;
            self.built_log.rewrite(&keep)?;
        }
        let through = self.durability.appended();
        #[cfg(test)]
        self.history.batch_read(Some(through));
        self.durability
            .make_durable(through)
            .map_err(|e| BuildError::Io(e.errno()))?;
        let (frames, offset) = self
            .reader
            .read(through, MAX_BATCH_STEPS)
            .map_err(to_build_error)?;
        // A batch cut at its step limit is built through its last record; the
        // next batch follows at once, so built events (and the gate) keep coming.
        let through = match frames.last() {
            Some((record, _))
                if frames
                    .iter()
                    .filter(|(r, _)| matches!(r.entry, Entry::Step { .. }))
                    .count()
                    >= MAX_BATCH_STEPS =>
            {
                record.seq
            }
            _ => through,
        };
        // `checkpointed` never passes the record before the checkpoint's
        // oldest step head, so every head a record after it may still name
        // (a fork's target, a save's `old`, a switch record's step) stays.
        if self
            .state
            .heads
            .get_min()
            .is_some_and(|(step, _)| *step <= checkpointed)
        {
            self.state.heads = self.state.heads.split(&checkpointed).1;
        }
        self.state.relocate(self.durability.relocations());
        let mut next = self.state.clone();
        let marks_path = self.root.join("build.marks");
        let repo = self.history.repo().clone();
        let ctx = Context {
            identity: &self.identity,
            service: &self.service,
            repo: &repo,
            root: &self.root,
            durability: &self.durability,
            salt: self.reader.salt,
        };
        let mut batch = Batch {
            repo: &repo,
            marks: &marks_path,
            feed: None,
            blobs: HashMap::new(),
            values: Vec::new(),
            scratch: BTreeMap::new(),
            owners: BTreeMap::new(),
            forked: false,
            moved: BTreeSet::new(),
            files: BTreeSet::new(),
            flags: Vec::new(),
        };
        for (record, tail) in &frames {
            next.apply(record, tail, &ctx, &mut batch)
                .map_err(to_build_error)?;
        }
        let marks = match batch.feed.take() {
            Some(feed) => {
                let _timer = Timer::start(Which::Objects);
                feed.finish()?
            }
            None => BTreeMap::new(),
        };
        let from = frames.first().map_or(through, |(record, _)| record.seq);
        next.resolve(&marks, from, &batch.owners)
            .map_err(to_build_error)?;
        next.note_framed(&batch.files);
        #[cfg(test)]
        next.check_framed();
        for tree in &batch.moved {
            let after = &next.tree(*tree).map_err(to_build_error)?.shape;
            // A tree this batch made (a fork) has none to start from.
            let objects = match (self.state.trees.get(tree), self.state.objects.get(tree)) {
                (Some(before), Some(objects)) => objects_after(&before.shape, objects, after),
                _ => objects_of(after),
            }
            .map_err(to_build_error)?;
            // The unit tests' check, at every batch: the objects moved are
            // all the tree's.
            #[cfg(test)]
            assert!(
                objects_of(after).is_ok_and(|all| all == objects),
                "{tree}: the objects a batch moved disagree with its tree's"
            );
            next.objects.insert(*tree, objects);
        }
        let objects = next.objects.clone();
        for (bytes, mark) in batch.blobs.values() {
            if let Some(oid) = marks.get(mark) {
                bytes.set_blob(*oid);
            }
        }
        let mut values = Vec::with_capacity(batch.values.len());
        for (step, name, value) in &batch.values {
            let oid = match value {
                Parent::Built(oid) => *oid,
                Parent::Mark(mark) => *marks
                    .get(mark)
                    .ok_or_else(|| BuildError::Packer("an unknown mark".into()))?,
            };
            values.push(BuiltValue {
                step: *step,
                name: name.clone(),
                oid,
            });
        }
        let moved = self.state.moved_to(&next).map_err(to_build_error)?;
        // Each flag's commit is in the pack: its evidence, best effort. The
        // flag itself is the commit's trailer, already durable.
        let mut flagged = Vec::with_capacity(batch.flags.len());
        for pending in std::mem::take(&mut batch.flags) {
            let commit = *marks
                .get(&pending.mark)
                .ok_or_else(|| BuildError::Packer("an unknown mark".into()))?;
            write_evidence(&repo, commit, &pending);
            flagged.push((commit, pending));
        }
        crate::failpoints::hit("tree.build_after_pack")?;
        let entry = BuiltEntry {
            through,
            values,
            moved,
        };
        self.built_log.append(&entry)?;
        // Built: each flag counts once, whatever builds the batch again.
        self.verification.add_unresolved(flagged.len() as u64);
        for (commit, pending) in &flagged {
            let replayed = pending
                .flag
                .replayed
                .map_or_else(|| "none".to_owned(), |f| f.to_string());
            eprintln!(
                "tree HistoryUnverified: step={} tree={} commit={commit} path={}: the builder's \
                 replay of this save does not match its recorded layout (recorded {}, replayed \
                 {replayed}); history holds the bytes the workload saved \
                 (docs/runbooks/recording-failures.md)",
                pending.step.get(),
                pending.tree,
                serde_json::to_string(&pending.path).unwrap_or_default(),
                pending.flag.recorded,
            );
        }
        self.reader.advance(offset, through);
        let built = next.built_pointers().map_err(to_build_error)?;
        let owners = next.owners.clone();
        let scratch: Vec<(BuildRef, Oid)> = batch
            .scratch
            .iter()
            .filter_map(|(on, mark)| Some((*on, *marks.get(mark)?)))
            .collect();
        let forked = batch.forked;
        let files = std::mem::take(&mut batch.files);
        self.state = next;
        let checked_out = match self.copy(&built, &scratch, forked) {
            Ok(trees) => trees,
            Err(e) => {
                self.stale = true;
                eprintln!("tree CopyStale: {e}");
                Vec::new()
            }
        };
        let maps = self.rebase(through, &checked_out, files);
        self.history.publish_maps(maps);
        let trees = self.state.trees.clone();
        self.history
            .built_event(through, built, (owners, objects, trees), started.elapsed());
        Ok(())
    }

    /// The files of each tree checked out through `through` that still read
    /// frames, read from the checkout as built, letting go of their frames;
    /// returns those maps and every map in `files`, for the actor to take
    /// where it saved the same step. A file that cannot be opened keeps its
    /// frames. Visits `framed` within `checked_out`, and `files`: O(batch),
    /// never every name.
    fn rebase(
        &mut self,
        through: RecordNo,
        checked_out: &[TreeId],
        files: BTreeSet<(TreeId, u64)>,
    ) -> Vec<(TreeId, u64, FileMap)> {
        let mut changed = files;
        let visit: Vec<(TreeId, u64)> = checked_out
            .iter()
            .flat_map(|tree| {
                self.state
                    .framed
                    .range((*tree, 0)..=(*tree, u64::MAX))
                    .copied()
            })
            .collect();
        for (tree, object) in visit {
            let state = &mut self.state;
            let Some(bytes) = state
                .trees
                .get(&tree)
                .and_then(|t| t.bytes.get(&object))
                .filter(|bytes| bytes.on_frames())
                .cloned()
            else {
                state.framed.remove(&(tree, object));
                continue;
            };
            // A file no name reaches has no checkout: it keeps its frames.
            let Some(path) = state
                .trees
                .get(&tree)
                .and_then(|t| t.names.of(object).next())
                .map(str::to_owned)
            else {
                continue;
            };
            let base = match BaseFile::open(
                &self.root.join(super::BASES),
                &super::checkout_path(&self.root, tree, &path),
                through,
                bytes.known_blob(),
            ) {
                Ok(base) => base,
                Err(e) => {
                    eprintln!("tree BaseFile: {tree} {path}: {e}");
                    continue;
                }
            };
            let mut map = (*bytes).clone();
            map.rebase(base);
            let rebased = Arc::new(bytes.with_map(map));
            if !rebased.on_frames() {
                state.framed.remove(&(tree, object));
            }
            if let Some(t) = state.trees.get_mut(&tree) {
                t.bytes.insert(object, rebased);
            }
            changed.insert((tree, object));
        }
        changed
            .into_iter()
            .filter_map(|(tree, object)| {
                let map = self.state.trees.get(&tree)?.bytes.get(&object)?;
                Some((tree, object, (**map).clone()))
            })
            .collect()
    }

    /// Git's refs, `config` and the checkouts: copies of the built state.
    /// Returns the trees whose checkout it rewrote.
    fn copy(
        &mut self,
        built: &Pointers,
        scratch: &[(BuildRef, Oid)],
        forked: bool,
    ) -> Result<Vec<TreeId>> {
        // The batch is built: from here a failure leaves only stale copies.
        crate::failpoints::hit("tree.build_after_built_log")?;
        let repo = self.history.repo().clone();
        repo.refresh()?;
        {
            let _timer = Timer::start(Which::RefCopy);
            if self.stale || forked {
                repo.write_config(&refs::head(), &self.state.upstreams.merges())?;
            }
            if self.stale {
                repo.rebuild_refs(&built.creates())?;
            } else {
                let mut changes = self.copied.changes_to(built);
                changes.extend(scratch.iter().map(|(on, oid)| RefUpdate {
                    refname: on.refname(),
                    new: None,
                    expected_old: Some(*oid),
                }));
                if !changes.is_empty() {
                    repo.ref_transaction(&changes)?;
                }
            }
        }
        let old = std::mem::replace(&mut self.copied, built.clone());
        crate::failpoints::hit("tree.build_after_ref_copy")?;
        // A dropped fork's checkout goes after its refs.
        let branches = refs::branches(built)?;
        let dropped: Vec<TreeId> = self
            .checked_out
            .keys()
            .copied()
            .filter(|tree| !branches.contains_key(tree))
            .collect();
        for tree in dropped {
            self.checked_out.remove(&tree);
            remove_checkout(&self.root, tree)?;
        }
        if self.stale {
            remove_stale_checkouts(&self.root, &branches)?;
        }
        let mut rewritten = Vec::new();
        for (tree, head) in branches {
            if !self.stale && old.get(&refs::branch(tree)) == Some(head) {
                continue;
            }
            let previous = self.checked_out.remove(&tree);
            let files = checkout(&repo, &self.root, tree, head, previous)?;
            self.checked_out.insert(tree, files);
            rewritten.push(tree);
        }
        self.stale = false;
        Ok(rewritten)
    }
}

/// A flagged save's evidence, under the notes ref, committed by the builder's
/// commit identity at the step's record time. Any failure is logged with the
/// evidence's text and never fails the batch: the flag is in the commit.
fn write_evidence(repo: &TreeGit, commit: Oid, pending: &Flagged) {
    let written = (|| -> Result<()> {
        let seconds = i64::try_from(pending.at_ns / 1_000_000_000).unwrap_or(i64::MAX);
        let committer =
            git2::Signature::new("agentfs", "agentfs@localhost", &git2::Time::new(seconds, 0))?;
        verify::write_evidence(repo, commit, &pending.evidence, &committer)
    })();
    if let Err(e) = written {
        let text = verify::render_evidence(&pending.evidence).unwrap_or_default();
        eprintln!(
            "tree HistoryEvidenceNotWritten: commit={commit}: {e}; the evidence was: {}",
            text.trim_end()
        );
    }
}

/// Copies at a start: `config`, every ref and every checkout, from the built
/// pointers. Returns what each checkout holds.
pub(in crate::tree_actor) fn copy_all(
    repo: &TreeGit,
    root: &Path,
    pointers: &Pointers,
    upstreams: &Upstreams,
) -> Result<BTreeMap<TreeId, Oid>> {
    repo.write_config(&refs::head(), &upstreams.merges())?;
    repo.rebuild_refs(&pointers.creates())?;
    crate::failpoints::hit("tree.after_ref_rebuild")?;
    let branches = refs::branches(pointers)?;
    remove_stale_checkouts(root, &branches)?;
    let mut checked_out = BTreeMap::new();
    for (tree, head) in branches {
        checked_out.insert(tree, checkout(repo, root, tree, head, None)?);
    }
    Ok(checked_out)
}

/// Deletes `tree`'s checkout, if it has one.
fn remove_checkout(root: &Path, tree: TreeId) -> Result<()> {
    match std::fs::remove_dir_all(root.join("trees").join(tree.to_string())) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Deletes each fork's checkout whose branch is gone: a dropped fork's, left
/// by a copy that failed or a process that died before it.
fn remove_stale_checkouts(root: &Path, branches: &BTreeMap<TreeId, Oid>) -> Result<()> {
    let forks = root.join("trees").join("fork");
    let entries = match std::fs::read_dir(&forks) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let name = entry?.file_name();
        let tree = format!("fork/{}", name.to_string_lossy()).parse::<TreeId>();
        match tree {
            Ok(tree) if branches.contains_key(&tree) => {}
            Ok(tree) => remove_checkout(root, tree)?,
            // Nothing a tree's checkout is named: never agentfs's to delete.
            Err(_) => {}
        }
    }
    Ok(())
}
