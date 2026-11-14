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
//! dates and take decisions are in the records, and a replay that disagrees
//! with its record (`Disagrees`) stops building with the alarm.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentfs_contracts::pod::{PodContext, WriterAt};
use agentfs_contracts::volume::VolumeIdentity;
use git2::Oid;

use super::built::BuiltObjects;
use super::built::{BuildError, History};
use super::built_log::{BuiltEntry, BuiltLog, BuiltValue};
use super::feed::{BuildRef, CommitSpec, Feed, Parent};
use super::format::{self, BlobRef};
use super::message::{self, Action, Message};
use super::upstreams::Upstreams;
use super::{Objects, checkout, objects_of, refs};
use crate::tree_actor::journal::durable::{Durability, RecordNo};
use crate::tree_actor::journal::segment::{FramePos, Relocations, Segment};
use crate::tree_actor::journal::{
    At, Change, Entry, Frame, Record, RowRecord, SaveOutcome, Tail, TakeHead, TakeRecord, Target,
    frame_at, runs_from_frame, take_chunks,
};
use crate::tree_actor::pointers::Pointers;
use crate::tree_actor::queue::Key;
use crate::tree_actor::recording::file_map::BaseFile;
use crate::tree_actor::recording::{Call, FileMap, own_keys, pages};
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
    calls: Vec<(u64, Call, FramePos)>,
}

/// What the builder has built: the trees with line maps and name histories,
/// the pointers as built (a batch's own commits as marks until it finishes),
/// the live opens' calls and page writes.
#[derive(Clone)]
pub(in crate::tree_actor) struct BuildState {
    trees: BTreeMap<TreeId, Tree>,
    pointers: Pointers<Parent>,
    upstreams: Upstreams,
    opens: BTreeMap<u64, BuildOpen>,
    pages: BTreeMap<(TreeId, u64), Vec<(u64, pages::Run)>>,
    owners: BTreeMap<EventNumber, Parent>,
    /// Each step's head, for a later record naming it (`Target::Step`).
    heads: BTreeMap<RecordNo, Parent>,
    /// Trees whose head's tree is not known to match the built tree (after a
    /// fast-forward): the next commit lists the whole tree.
    whole: BTreeSet<TreeId>,
    /// Each tree's object numbers as built, as History publishes them.
    objects: BTreeMap<TreeId, Arc<Objects>>,
    /// Chunk records no take has taken yet.
    bytes: Vec<(u64, Tail)>,
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
    /// A fork made or dropped: `config`'s branch merges change.
    forked: bool,
    /// Trees whose names or objects a step moved.
    moved: BTreeSet<TreeId>,
    /// Files whose map a step replaced: History hands the new maps to the actor.
    files: BTreeSet<(TreeId, u64)>,
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

impl BuildState {
    /// Moves every position the state holds (live opens' calls, held page
    /// writes, take chunks, file maps) to where compaction carried its
    /// frame, letting go of the dropped segments.
    fn relocate(&mut self, moved: &Relocations) {
        for open in self.opens.values_mut() {
            for (_, _, pos) in &mut open.calls {
                pos.relocate(moved);
            }
        }
        for runs in self.pages.values_mut() {
            pages::relocate_runs(runs, moved);
        }
        for (_, tail) in &mut self.bytes {
            tail.pos.relocate(moved);
        }
        for tree in self.trees.values_mut() {
            for bytes in tree.bytes.values_mut() {
                if !bytes.on_frames() {
                    continue;
                }
                let mut map = (**bytes).clone();
                if map.relocate(moved) {
                    *bytes = Arc::new(bytes.with_map(map));
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
        owners: &BTreeMap<EventNumber, Oid>,
        heads: BTreeMap<RecordNo, Oid>,
    ) -> Result<Self> {
        let mut trees = trees;
        let objects = trees
            .iter()
            .map(|(id, tree)| Ok((*id, Arc::new(objects_of(&tree.shape)?))))
            .collect::<Result<_>>()?;
        for folded in opens.values() {
            if folded.open.anonymous {
                trees
                    .get_mut(&folded.open.tree)
                    .ok_or(Error::Format)?
                    .add_anonymous(folded.open.object);
            }
        }
        Ok(Self {
            trees,
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
                            calls: folded.calls.clone(),
                        },
                    )
                })
                .collect(),
            pages,
            owners: owners
                .iter()
                .map(|(event, oid)| (*event, Parent::Built(*oid)))
                .collect(),
            heads: heads
                .into_iter()
                .map(|(step, oid)| (step, Parent::Built(oid)))
                .collect(),
            whole: BTreeSet::new(),
            objects,
            bytes,
        })
    }

    fn tree(&self, tree: TreeId) -> Result<&Tree> {
        self.trees.get(&tree).ok_or(Error::Format)
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
            Entry::AnonymousOpen { tree, .. } => {
                let tree = tree.parse::<TreeId>()?;
                self.trees
                    .get_mut(&tree)
                    .ok_or(Error::Format)?
                    .add_anonymous(seq);
                self.opened(seq, tree, seq);
            }
            Entry::Call { handle, call } => {
                call.check_tail(tail.len)?;
                self.opens
                    .get_mut(handle)
                    .ok_or(Error::Format)?
                    .calls
                    .push((seq, call.clone(), tail.pos.clone()));
            }
            Entry::PageWrite { tree, object, runs } => self
                .pages
                .entry((tree.parse::<TreeId>()?, *object))
                .or_default()
                .extend(runs_from_frame(runs, tail)?.into_iter().map(|r| (seq, r))),
            Entry::Bytes => self.bytes.push((seq, tail.clone())),
            Entry::Step {
                by,
                change,
                pointers,
                rows,
            } => self.step(record, by, (change, rows), pointers, ctx, batch)?,
        }
        Ok(())
    }

    fn opened(&mut self, handle: u64, tree: TreeId, object: u64) {
        self.opens.insert(
            handle,
            BuildOpen {
                tree,
                object,
                calls: Vec::new(),
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
        let whole = self.whole.remove(&tree);
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
            }
            .render(ctx.identity)
        };
        // The commit this step makes or moves its tree's head to.
        let mut head: Option<Parent> = None;
        let mut owner = false;
        match change {
            Change::Create { tree, path } => {
                let id = tree.parse::<TreeId>()?;
                let t = self.tree_mut(id)?;
                let keys = t.shape.apply(change, event)?;
                let object = event.object();
                t.bytes.insert(object, saved_map(FileMap::empty(), seq));
                t.lines.insert(object, Arc::default());
                batch.files.insert((id, object));
                let identity = *t.identities.get(&object).ok_or(Error::Format)?;
                t.namespace
                    .insert(identity, NameHistory::created(event, path.clone()));
                let message = direct(Action::Create, id, None)?;
                head = Some(self.commit_direct(id, &keys, &message, record.at_ns, batch)?);
                self.opened(seq, id, object);
                owner = true;
                batch.moved.insert(id);
            }
            Change::Link { tree, object, path } => {
                let id = tree.parse::<TreeId>()?;
                let t = self.tree_mut(id)?;
                let keys = t.shape.apply(change, event)?;
                let identity = *t.identities.get(object).ok_or_else(|| disagrees(seq))?;
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
                let keys = t.shape.save(open.object, *outcome);
                match outcome {
                    SaveOutcome::AnonymousDropped => {
                        t.bytes.remove(&open.object);
                        t.lines.remove(&open.object);
                    }
                    SaveOutcome::Unchanged | SaveOutcome::OrphanDropped => {}
                    SaveOutcome::Saved {
                        len,
                        pieces: digest,
                    } => {
                        let writes = self.pages.get(&(id, open.object));
                        let (map, calls) = pages::replay_close(
                            t.bytes.get(&open.object).ok_or_else(|| disagrees(seq))?,
                            &open.calls,
                            writes.map_or(&[][..], Vec::as_slice),
                            pieces,
                        )?
                        .ok_or_else(|| disagrees(seq))?;
                        if map.len() != *len || map.digest(&own_keys(&calls)) != *digest {
                            return Err(disagrees(seq));
                        }
                        let mut versions = Versions::of(event);
                        let base = t.bytes.get(&open.object).ok_or_else(|| disagrees(seq))?;
                        let old = t.lines.get(&open.object).ok_or_else(|| disagrees(seq))?;
                        let lines = if calls
                            .iter()
                            .all(|(call, _)| matches!(call, Call::Append { .. }))
                        {
                            let added = Window::new(&map, base.len(), map.len() - base.len());
                            old.append(&added, |n| versions.take(n))?
                        } else {
                            align(ctx.root, old, base, &map, &mut versions)?
                        };
                        t.lines.insert(open.object, Arc::new(lines));
                        t.bytes.insert(open.object, saved_map(map, seq));
                        batch.files.insert((id, open.object));
                        let message = direct(Action::Direct, id, Some(*handle))?;
                        head =
                            Some(self.commit_direct(id, &keys, &message, record.at_ns, batch)?);
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
            Change::Rename { tree, from, to } => {
                let id = tree.parse::<TreeId>()?;
                let t = self.tree_mut(id)?;
                let object = *t.names.get(from).ok_or_else(|| disagrees(seq))?;
                let source = *t.identities.get(&object).ok_or_else(|| disagrees(seq))?;
                match t.names.get(to).copied() {
                    Some(replaced) if t.fresh_names.contains(&object) => {
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
                            let replaced = *t.identities.get(&replaced).ok_or(Error::Format)?;
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
                let keys = t.shape.apply(change, event)?;
                let message = direct(Action::Rename, id, None)?;
                head = Some(self.commit_direct(id, &keys, &message, record.at_ns, batch)?);
                owner = true;
                batch.moved.insert(id);
            }
            Change::Unlink { tree, path } => {
                let id = tree.parse::<TreeId>()?;
                let t = self.tree_mut(id)?;
                let object = *t.names.get(path).ok_or_else(|| disagrees(seq))?;
                let identity = *t.identities.get(&object).ok_or(Error::Format)?;
                t.namespace
                    .get_mut(&identity)
                    .ok_or(Error::Format)?
                    .advance(event, None, None);
                let keys = t.shape.apply(change, event)?;
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
                head = Some(self.head(upstream)?);
                batch.forked = true;
                batch.moved.insert(fork);
            }
            Change::Take { tree, take } => {
                let id = tree.parse::<TreeId>()?;
                head = self.take(id, seq, take, change, ctx, record.at_ns, batch)?;
                batch.moved.insert(id);
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
                self.pages.retain(|(tree, _), _| *tree != fork);
                batch.moved.remove(&fork);
                batch.files.retain(|(tree, _)| *tree != fork);
                batch.forked = true;
            }
        }
        if let Some(head) = head {
            self.heads.insert(record.seq, head);
            if owner {
                self.owners.insert(event, head);
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
        let chunks = take_chunks(&mut self.bytes, seq, take.chunks)?;
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
        // The actor ran the shape's own transition: the two must agree.
        let mut shape = current.shape.clone();
        shape.apply(change, event)?;
        if *shape.names != *t.shape.names || shape.fresh_names != t.shape.fresh_names {
            return Err(disagrees(seq));
        }
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
                    .identities
                    .get(object)
                    .is_some_and(|i| identities.contains(i))
                {
                    changed.insert(Key::Name(path.clone()));
                }
            }
        }
        changed.extend(identities.into_iter().map(Key::File));
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

    /// Every mark replaced by its object, once the batch's pack is installed.
    fn resolve(&mut self, marks: &BTreeMap<u64, Oid>) -> Result<()> {
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
        for value in self.owners.values_mut() {
            resolve(value)?;
        }
        for value in self.heads.values_mut() {
            resolve(value)?;
        }
        Ok(())
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

    fn built_owners(&self) -> BTreeMap<EventNumber, Oid> {
        self.owners
            .iter()
            .filter_map(|(event, value)| match value {
                Parent::Built(oid) => Some((*event, *oid)),
                Parent::Mark(_) => None,
            })
            .collect()
    }
}

/// What every step's build reads and never changes.
pub(in crate::tree_actor) struct Context<'a> {
    pub identity: &'a VolumeIdentity,
    pub service: &'a PodContext,
    pub repo: &'a TreeGit,
    /// The state area: where an alignment writes the files `git diff` reads.
    pub root: &'a Path,
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
    /// The built log's entries since its last rewrite.
    pub log: Vec<BuiltEntry>,
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
        let started = std::time::Instant::now();
        let checkpointed = self.history.checkpointed_through();
        if self
            .log
            .first()
            .is_some_and(|entry| entry.through <= checkpointed)
        {
            self.log.retain(|entry| entry.through > checkpointed);
            self.built_log.rewrite(&self.log)?;
        }
        let through = self.durability.appended();
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
        // No record after the checkpoint names a step it covers
        // (`TreeActor::resolve_compacted`): those heads are never read again.
        self.state.heads.retain(|step, _| *step > checkpointed);
        self.state.relocate(self.durability.relocations());
        let mut next = self.state.clone();
        let marks_path = self.root.join("build.marks");
        let repo = self.history.repo().clone();
        let ctx = Context {
            identity: &self.identity,
            service: &self.service,
            repo: &repo,
            root: &self.root,
        };
        let mut batch = Batch {
            repo: &repo,
            marks: &marks_path,
            feed: None,
            blobs: HashMap::new(),
            values: Vec::new(),
            scratch: BTreeMap::new(),
            forked: false,
            moved: BTreeSet::new(),
            files: BTreeSet::new(),
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
        next.resolve(&marks).map_err(to_build_error)?;
        for tree in &batch.moved {
            let objects = objects_of(&next.tree(*tree).map_err(to_build_error)?.shape)
                .map_err(to_build_error)?;
            next.objects.insert(*tree, Arc::new(objects));
        }
        let objects: Arc<BuiltObjects> = Arc::new(next.objects.clone());
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
                oid: oid.to_string(),
            });
        }
        crate::failpoints::hit("tree.build_after_pack")?;
        let entry = BuiltEntry { through, values };
        self.built_log.append(&entry)?;
        self.log.push(entry);
        self.reader.advance(offset, through);
        let built = next.built_pointers().map_err(to_build_error)?;
        let owners = next.built_owners();
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
        self.history
            .built_event(through, built, owners, objects, started.elapsed());
        Ok(())
    }

    /// The files of each tree checked out through `through` read from the
    /// checkout as built, letting go of their frames; returns those maps and
    /// every map in `files`, for the actor to take where it saved the same
    /// step. A file that cannot be opened keeps its frames.
    fn rebase(
        &mut self,
        through: RecordNo,
        checked_out: &[TreeId],
        files: BTreeSet<(TreeId, u64)>,
    ) -> Vec<(TreeId, u64, FileMap)> {
        let mut changed = files;
        for tree in checked_out {
            let Some(t) = self.state.trees.get_mut(tree) else {
                continue;
            };
            let named: Vec<(String, u64)> = t
                .names
                .iter()
                .map(|(path, object)| (path.clone(), *object))
                .collect();
            for (path, object) in named {
                let Some(bytes) = t.bytes.get(&object) else {
                    continue;
                };
                if !bytes.on_frames() {
                    continue;
                }
                let base = match BaseFile::open(
                    &self.root.join(super::BASES),
                    &super::checkout_path(&self.root, *tree, &path),
                    through,
                    bytes.known_blob(),
                ) {
                    Ok(base) => base,
                    Err(e) => {
                        eprintln!("tree BaseFile: {tree} {path}: {e}");
                        continue;
                    }
                };
                let mut map = (**bytes).clone();
                map.rebase(base);
                let rebased = Arc::new(bytes.with_map(map));
                t.bytes.insert(object, rebased);
                changed.insert((*tree, object));
            }
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
