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
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentfs_contracts::pod::{PodContext, WriterAt};
use agentfs_contracts::volume::VolumeIdentity;
use git2::Oid;

use super::built::{BuildError, History};
use super::built_log::{BuiltEntry, BuiltLog, BuiltValue};
use super::feed::{CommitSpec, Feed, Parent};
use super::format::{self, BlobRef};
use super::message::{self, Action, Message};
use super::upstreams::Upstreams;
use super::{checkout, object_table, refs};
use crate::tree_actor::journal::durable::{Durability, RecordNo};
use crate::tree_actor::journal::{
    Change, Entry, Frame, Record, SaveOutcome, TakeHead, TakeRecord, Target, frame_at,
    runs_from_frame, salt_of,
};
use crate::tree_actor::pointers::Pointers;
use crate::tree_actor::recording::{Call, changed_bytes, pages};
use crate::tree_actor::stages::{Timer, Which};
use crate::tree_actor::tree::{EventNumber, FileBytes, NameHistory, Tree, Versions};
use crate::tree_actor::{Error, Result, TreeId, referee};
use crate::tree_git::{RefUpdate, TreeGit};

/// The builder's own reader of the journal file: frames after its cursor,
/// through the durable mark. It reopens the file after a compaction replaced
/// it, skipping to the cursor.
pub(in crate::tree_actor) struct JournalReader {
    path: PathBuf,
    file: File,
    salt: u32,
    offset: u64,
    cursor: RecordNo,
    generation: u64,
}

impl JournalReader {
    /// A reader of the journal at `path` whose cursor is `cursor`: the next
    /// read starts after it.
    pub fn open(path: &Path, cursor: RecordNo, generation: u64) -> Result<Self> {
        let mut reader = Self {
            path: path.to_owned(),
            file: File::open(path)?,
            salt: 0,
            offset: 0,
            cursor,
            generation,
        };
        reader.reopen(generation)?;
        Ok(reader)
    }

    fn reopen(&mut self, generation: u64) -> Result<()> {
        self.file = File::open(&self.path)?;
        self.salt = salt_of(&self.file)?;
        self.offset = 0;
        self.generation = generation;
        while let Some(((record, _), len)) =
            frame_at(&self.file, self.offset, self.salt, self.cursor)?
        {
            let _ = record;
            self.offset += len;
        }
        Ok(())
    }

    /// Frames after the cursor through `through` (at most the durable mark),
    /// and where they end. The cursor moves only at [`JournalReader::advance`].
    pub fn read(&mut self, through: RecordNo, generation: u64) -> Result<(Vec<Frame>, u64)> {
        if generation != self.generation {
            self.reopen(generation)?;
        }
        let mut frames = Vec::new();
        let mut at = self.offset;
        while let Some((frame, len)) = frame_at(&self.file, at, self.salt, through)? {
            at += len;
            if frame.0.seq > self.cursor {
                frames.push(frame);
            }
        }
        Ok((frames, at))
    }

    /// The batch read up to `offset`, through `through`, is built.
    pub fn advance(&mut self, offset: u64, through: RecordNo) {
        self.offset = offset;
        self.cursor = self.cursor.max(through);
    }
}

/// A write-open as the builder knows it.
#[derive(Clone)]
struct BuildOpen {
    tree: TreeId,
    object: u64,
    calls: Vec<(u64, Call)>,
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
    /// fast-forward, or at start): the next commit lists the whole tree.
    whole: BTreeSet<TreeId>,
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
    /// The last commit each `refs/build/<tree>` got.
    scratch: BTreeMap<TreeId, u64>,
    forked: bool,
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
        let mark = self.feed()?.blob(bytes).map_err(build_error)?;
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
    /// The state at a built point: the trees its pointers' heads hold, the
    /// opens, calls and page writes live then.
    pub(in crate::tree_actor) fn at(
        trees: BTreeMap<TreeId, Tree>,
        pointers: &Pointers,
        upstreams: Upstreams,
        opens: &BTreeMap<u64, crate::tree_actor::journal::FoldedOpen>,
        pages: BTreeMap<(TreeId, u64), Vec<(u64, pages::Run)>>,
        owners: &BTreeMap<EventNumber, Oid>,
        heads: BTreeMap<RecordNo, Oid>,
    ) -> Result<Self> {
        let mut trees = trees;
        for folded in opens.values() {
            if folded.open.anonymous {
                trees
                    .get_mut(&folded.open.tree)
                    .ok_or(Error::Format)?
                    .add_anonymous(folded.open.object);
            }
        }
        let whole = trees.keys().copied().collect();
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
            whole,
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
        tail: &[u8],
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
            Entry::Call { handle, call } => self
                .opens
                .get_mut(handle)
                .ok_or(Error::Format)?
                .calls
                .push((seq, Call::from_frame(call, tail)?)),
            Entry::PageWrite { tree, object, runs } => self
                .pages
                .entry((tree.parse::<TreeId>()?, *object))
                .or_default()
                .extend(runs_from_frame(runs, tail)?.into_iter().map(|r| (seq, r))),
            Entry::Step {
                by,
                change,
                pointers,
            } => self.step(record, by, change, pointers, ctx, batch)?,
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

    /// A direct save's commit of `after` over the tree's head.
    fn commit_direct(
        &mut self,
        tree: TreeId,
        after: &Tree,
        message: &str,
        at_ns: u64,
        batch: &mut Batch<'_>,
    ) -> Result<Parent> {
        let parent = self.head(tree)?;
        let before = if self.whole.remove(&tree) {
            None
        } else {
            Some(self.tree(tree)?)
        };
        let edits = format::edits(before, after, |bytes| batch.blob(bytes))?;
        let mark = batch
            .feed()?
            .commit(&CommitSpec {
                tree,
                parents: &[parent],
                at_ns,
                message,
                edits: &edits,
            })
            .map_err(build_error)?;
        batch.scratch.insert(tree, mark);
        Ok(Parent::Mark(mark))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one arm per kind of step, as the record has"
    )]
    fn step(
        &mut self,
        record: &Record,
        by: &WriterAt,
        change: &Change,
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
        let mut target_tree: Option<TreeId> = None;
        match change {
            Change::Create { tree, path } => {
                let id = tree.parse::<TreeId>()?;
                let mut t = self.tree(id)?.clone();
                t.shape.apply(change, event)?;
                let object = event.object();
                t.bytes.insert(object, Arc::default());
                t.lines.insert(object, Arc::default());
                let identity = t.identities[&object];
                t.namespace
                    .insert(identity, NameHistory::created(event, path.clone()));
                let message = direct(Action::Create, id, None)?;
                head = Some(self.commit_direct(id, &t, &message, record.at_ns, batch)?);
                self.trees.insert(id, t);
                self.opened(seq, id, object);
                owner = true;
                target_tree = Some(id);
            }
            Change::Link { tree, object, path } => {
                let id = tree.parse::<TreeId>()?;
                let mut t = self.tree(id)?.clone();
                t.shape.apply(change, event)?;
                let identity = *t.identities.get(object).ok_or_else(|| disagrees(seq))?;
                t.namespace
                    .insert(identity, NameHistory::created(event, path.clone()));
                let message = direct(Action::Create, id, None)?;
                head = Some(self.commit_direct(id, &t, &message, record.at_ns, batch)?);
                self.trees.insert(id, t);
                owner = true;
                target_tree = Some(id);
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
                let mut t = self.tree(id)?.clone();
                t.shape.save(open.object, *outcome);
                match outcome {
                    SaveOutcome::AnonymousDropped => {
                        t.bytes.remove(&open.object);
                        t.lines.remove(&open.object);
                    }
                    SaveOutcome::Unchanged | SaveOutcome::OrphanDropped => {}
                    SaveOutcome::Saved { len, crc32 } => {
                        let writes = self.pages.get(&(id, open.object));
                        let mut resolved = Vec::with_capacity(pieces.len());
                        for piece in pieces {
                            let runs: Vec<pages::Run> = writes
                                .into_iter()
                                .flatten()
                                .filter(|(n, _)| *n == piece.record.get())
                                .map(|(_, run)| run.clone())
                                .collect();
                            let run = pages::resolve_piece(&runs, piece.offset, piece.len)
                                .ok_or_else(|| disagrees(seq))?;
                            resolved.push((piece.record.get(), run));
                        }
                        let calls = pages::close_calls(&open.calls, &resolved);
                        let saved = t.bytes.get(&open.object).ok_or_else(|| disagrees(seq))?;
                        let bytes = changed_bytes(saved, &calls)?.ok_or_else(|| disagrees(seq))?;
                        if bytes.len() as u64 != *len || crc32fast::hash(&bytes) != *crc32 {
                            return Err(disagrees(seq));
                        }
                        let mut versions = Versions::of(event);
                        let mut next_id = || versions.next();
                        let old = t.lines.get(&open.object).ok_or_else(|| disagrees(seq))?;
                        let lines = if calls.iter().all(|call| matches!(call, Call::Append(_))) {
                            let suffix: Vec<u8> = calls
                                .iter()
                                .flat_map(|call| match call {
                                    Call::Append(bytes) => bytes.as_slice(),
                                    _ => &[],
                                })
                                .copied()
                                .collect();
                            old.append(&suffix, &mut next_id)?
                        } else {
                            let _timer = Timer::start(Which::Align);
                            old.align(&bytes, &mut next_id)?
                        };
                        t.lines.insert(open.object, Arc::new(lines));
                        t.bytes.insert(open.object, Arc::new(bytes.into()));
                        let message = direct(Action::Direct, id, Some(*handle))?;
                        head = Some(self.commit_direct(id, &t, &message, record.at_ns, batch)?);
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
                self.trees.insert(id, t);
                target_tree = Some(id);
            }
            Change::Rename { tree, from, to } => {
                let id = tree.parse::<TreeId>()?;
                let mut t = self.tree(id)?.clone();
                let object = *t.names.get(from).ok_or_else(|| disagrees(seq))?;
                let source = *t.identities.get(&object).ok_or_else(|| disagrees(seq))?;
                match t.names.get(to).copied() {
                    Some(replaced) if t.fresh_names.contains(&object) => {
                        t.namespace
                            .get_mut(&source)
                            .ok_or(Error::Format)?
                            .advance(event, None, None);
                        let mut versions = Versions::of(event);
                        let bytes = Arc::clone(t.bytes.get(&object).ok_or(Error::Format)?);
                        let lines = t
                            .lines
                            .get(&replaced)
                            .ok_or(Error::Format)?
                            .align(&bytes, || versions.next())?;
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
                t.shape.apply(change, event)?;
                let message = direct(Action::Rename, id, None)?;
                head = Some(self.commit_direct(id, &t, &message, record.at_ns, batch)?);
                self.trees.insert(id, t);
                owner = true;
                target_tree = Some(id);
            }
            Change::Unlink { tree, path } => {
                let id = tree.parse::<TreeId>()?;
                let mut t = self.tree(id)?.clone();
                let object = *t.names.get(path).ok_or_else(|| disagrees(seq))?;
                let identity = *t.identities.get(&object).ok_or(Error::Format)?;
                t.namespace
                    .get_mut(&identity)
                    .ok_or(Error::Format)?
                    .advance(event, None, None);
                t.shape.apply(change, event)?;
                let message = direct(Action::Unlink, id, None)?;
                head = Some(self.commit_direct(id, &t, &message, record.at_ns, batch)?);
                self.trees.insert(id, t);
                owner = true;
                target_tree = Some(id);
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
            }
            Change::Take { tree, take } => {
                let id = tree.parse::<TreeId>()?;
                head = self.take(id, seq, take, change, ctx, record.at_ns, batch)?;
                target_tree = Some(id);
            }
        }
        if let Some(head) = head {
            self.heads.insert(record.seq, head);
            if owner {
                self.owners.insert(event, head);
            }
        }
        for pointer in changes {
            let value = match &pointer.new {
                None => None,
                Some(Target::Oid(hex)) => Some(Parent::Built(
                    Oid::from_str(hex).map_err(|_| Error::Format)?,
                )),
                Some(Target::Step(step)) if *step == record.seq => {
                    let value = match target_tree {
                        Some(tree) if pointer.name == refs::object_table(tree) => {
                            let blob = object_table(self.tree(tree)?)?;
                            Parent::Mark(batch.feed()?.blob(&blob).map_err(build_error)?)
                        }
                        _ => head.ok_or_else(|| disagrees(seq))?,
                    };
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
        t.install(snapshot)?;
        // The actor ran the shape's own transition: the two must agree.
        let mut shape = current.shape.clone();
        shape.apply(change, event)?;
        if *shape.names != *t.shape.names || shape.fresh_names != t.shape.fresh_names {
            return Err(disagrees(seq));
        }
        let parent = self.head(id)?;
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
                Some(self.commit_whole(
                    id,
                    &t,
                    &[parent, Parent::Built(incoming)],
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
                Some(self.commit_whole(
                    id,
                    &t,
                    &[parent, Parent::Built(target)],
                    &message,
                    at_ns,
                    batch,
                )?)
            }
        };
        self.trees.insert(id, t);
        Ok(head)
    }

    /// A merge's commit: the whole tree over its first parent's.
    fn commit_whole(
        &mut self,
        tree: TreeId,
        after: &Tree,
        parents: &[Parent],
        message: &str,
        at_ns: u64,
        batch: &mut Batch<'_>,
    ) -> Result<Parent> {
        self.whole.remove(&tree);
        let edits = format::edits(None, after, |bytes| batch.blob(bytes))?;
        let mark = batch
            .feed()?
            .commit(&CommitSpec {
                tree,
                parents,
                at_ns,
                message,
                edits: &edits,
            })
            .map_err(build_error)?;
        batch.scratch.insert(tree, mark);
        Ok(Parent::Mark(mark))
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
        let checkpointed = self.history.state().checkpointed;
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
        let generation = self.durability.generation();
        let (frames, offset) = self
            .reader
            .read(through, generation)
            .map_err(to_build_error)?;
        let mut next = self.state.clone();
        let marks_path = self.root.join("build.marks");
        let repo = self.history.repo().clone();
        let ctx = Context {
            identity: &self.identity,
            service: &self.service,
            repo: &repo,
        };
        let mut batch = Batch {
            repo: &repo,
            marks: &marks_path,
            feed: None,
            blobs: HashMap::new(),
            values: Vec::new(),
            scratch: BTreeMap::new(),
            forked: false,
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
        let scratch: Vec<(TreeId, Oid)> = batch
            .scratch
            .iter()
            .filter_map(|(tree, mark)| Some((*tree, *marks.get(mark)?)))
            .collect();
        let forked = batch.forked;
        self.state = next;
        if let Err(e) = self.copy(&built, &scratch, forked) {
            self.stale = true;
            eprintln!("tree CopyStale: {e}");
        }
        self.history
            .built_event(through, built, owners, started.elapsed());
        Ok(())
    }

    /// Git's refs, `config` and the checkouts: copies of the built state.
    fn copy(&mut self, built: &Pointers, scratch: &[(TreeId, Oid)], forked: bool) -> Result<()> {
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
                changes.extend(scratch.iter().map(|(tree, oid)| RefUpdate {
                    refname: format!("refs/build/{tree}"),
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
        for (tree, head) in refs::branches(built)? {
            if !self.stale && old.get(&refs::branch(tree)) == Some(head) {
                continue;
            }
            let previous = self.checked_out.remove(&tree);
            let files = checkout(&repo, &self.root, tree, head, previous)?;
            self.checked_out.insert(tree, files);
        }
        self.stale = false;
        Ok(())
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
    let mut checked_out = BTreeMap::new();
    for (tree, head) in refs::branches(pointers)? {
        checked_out.insert(tree, checkout(repo, root, tree, head, None)?);
    }
    Ok(checked_out)
}

/// A take's changed files and names, as its record holds them.
pub(in crate::tree_actor) type TakeDiff = (
    Vec<(
        crate::tree_actor::tree::MergeFileIdentity,
        Option<referee::File>,
    )>,
    Vec<(
        crate::tree_actor::tree::MergeFileIdentity,
        Option<NameHistory>,
    )>,
);

/// A take's decision on a built tree as a record: the files and names that
/// differ from `mine`.
pub(in crate::tree_actor) fn take_diff(
    mine: &referee::Snapshot,
    decided: &referee::Snapshot,
) -> TakeDiff {
    let mut files = Vec::new();
    for (id, file) in &decided.files {
        if mine.files.get(id) != Some(file) {
            files.push((*id, Some(file.clone())));
        }
    }
    for id in mine.files.keys() {
        if !decided.files.contains_key(id) {
            files.push((*id, None));
        }
    }
    let mut names = Vec::new();
    for (id, history) in &decided.names {
        if mine.names.get(id) != Some(history) {
            names.push((*id, Some(history.clone())));
        }
    }
    for id in mine.names.keys() {
        if !decided.names.contains_key(id) {
            names.push((*id, None));
        }
    }
    (files, names)
}
