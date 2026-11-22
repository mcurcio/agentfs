//! What a journal's frames add up to: the fold recovery starts from, and each
//! tree's object numbers, whose one home is the journal (target git-model §3,
//! §12).

use std::collections::{BTreeMap, BTreeSet};

use agentfs_contracts::pod::WriterAt;
use git2::Oid;

use super::compaction::Checkpoint;
use super::durable::RecordNo;
use super::segment::FramePos;
use super::{
    Change, Entry, Frame, OpenRecord, PointerChange, Record, RunHead, SaveOutcome, Tail, Target,
    TreeLocals, chunk_map, take_chunks,
};
use crate::tree_actor::history::Objects;
use crate::tree_actor::history::upstreams::Upstreams;
use crate::tree_actor::meta::Meta;
use crate::tree_actor::pointers::Pointers;
use crate::tree_actor::recording::layout::{Layout, Positions};
use crate::tree_actor::recording::{Call, FileMap, pages};
use crate::tree_actor::tree::{EventNumber, SavedTree};
use crate::tree_actor::{Error, Result, TreeId};

/// A live write-open: a journal [`OpenRecord`] with its tree parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) struct LiveOpen {
    pub(in crate::tree_actor) handle: u64,
    pub(in crate::tree_actor) tree: TreeId,
    pub(in crate::tree_actor) by: WriterAt,
    pub(in crate::tree_actor) object: u64,
    /// Never linked: its object has no name and no saved state.
    pub(in crate::tree_actor) anonymous: bool,
    /// An anonymous open's metadata.
    pub(in crate::tree_actor) meta: Option<Meta>,
}

impl LiveOpen {
    pub(in crate::tree_actor) fn parse(record: &OpenRecord) -> Result<Self> {
        Ok(Self {
            handle: record.handle,
            tree: record.tree.parse::<TreeId>()?,
            by: record.by.clone(),
            object: record.object,
            anonymous: record.anonymous,
            meta: record.meta,
        })
    }
}

/// A live open and its calls, as the journal leaves them: each call with
/// its number and where its bytes are.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::tree_actor) struct FoldedOpen {
    pub(in crate::tree_actor) open: LiveOpen,
    pub(in crate::tree_actor) calls: Vec<(u64, Call, FramePos)>,
}

/// What a journal's frames add up to (git-model §12).
#[derive(Debug, PartialEq, Eq)]
pub(in crate::tree_actor) struct Folded {
    /// The pointers as the records name them: oids from the checkpoint (its
    /// step heads as their steps), and the steps after it as [`Target`]s.
    pub(in crate::tree_actor) pointers: Pointers<Target>,
    /// The journal's salt, the checkpoint's.
    pub(in crate::tree_actor) salt: u32,
    /// The checkpoint's redo start: its pointers' oids are as of this record.
    pub(in crate::tree_actor) built: RecordNo,
    /// Each tree's object numbers as of `built`, from the checkpoint.
    pub(in crate::tree_actor) objects: BTreeMap<TreeId, Objects>,
    /// Each tree's directory numbers and times and files' times as of
    /// `built`, from the checkpoint.
    pub(in crate::tree_actor) locals: BTreeMap<TreeId, TreeLocals>,
    /// Each fork's upstream. The root has none.
    pub(in crate::tree_actor) upstreams: Upstreams,
    pub(in crate::tree_actor) next_seq: u64,
    pub(in crate::tree_actor) opens: BTreeMap<u64, FoldedOpen>,
    /// A file's page writes while a write-open of it lives.
    pub(in crate::tree_actor) page_writes: BTreeMap<(TreeId, u64), Vec<(u64, pages::Run)>>,
    /// The last step that closed a write-open of a file, while one lives.
    pub(in crate::tree_actor) saved_through: BTreeMap<(TreeId, u64), u64>,
    /// The chunk records no take or replace step has taken yet
    /// ([`super::take_chunks`]).
    pub(in crate::tree_actor) bytes: Vec<(u64, Tail)>,
}

impl Folded {
    pub(in crate::tree_actor) fn add_upstream(&mut self, fork: &str, upstream: &str) -> Result<()> {
        let TreeId::Fork(fork) = fork.parse::<TreeId>()? else {
            return Err(Error::Format);
        };
        self.upstreams.add(fork, upstream.parse::<TreeId>()?);
        Ok(())
    }

    /// One step's part of the fold: its pointers, the open a create makes, the
    /// fork it makes, the open a close ends.
    pub(in crate::tree_actor) fn add_step(
        &mut self,
        seq: u64,
        by: &WriterAt,
        change: &Change,
    ) -> Result<()> {
        match change {
            Change::Create { tree, .. } => self.add_open(LiveOpen {
                handle: seq,
                tree: tree.parse::<TreeId>()?,
                by: by.clone(),
                object: seq,
                anonymous: false,
                meta: None,
            }),
            Change::Link { object, .. } => self
                .opens
                .values_mut()
                .filter(|o| o.open.object == *object)
                .for_each(|o| {
                    o.open.anonymous = false;
                    o.open.meta = None;
                }),
            Change::Save { handle, .. } => {
                let open = self.opens.remove(handle).ok_or(Error::Format)?.open;
                self.saved_through.insert((open.tree, open.object), seq);
            }
            Change::Fork { upstream } => {
                self.upstreams.add(seq, upstream.parse::<TreeId>()?);
            }
            Change::Drop { fork } => self.upstreams.remove(fork.parse::<TreeId>()?),
            Change::Rename { .. }
            | Change::Unlink { .. }
            | Change::Take { .. }
            | Change::Replace { .. }
            | Change::Rows
            | Change::Mkdir { .. }
            | Change::Rmdir { .. }
            | Change::Symlink { .. }
            | Change::SetMeta { .. }
            | Change::SetTimes { .. } => {}
        }
        Ok(())
    }

    pub(in crate::tree_actor) fn add_open(&mut self, open: LiveOpen) {
        self.opens.insert(
            open.handle,
            FoldedOpen {
                open,
                calls: Vec::new(),
            },
        );
    }
}

/// A write-open a replay follows: its tree, object and calls.
type ReplayOpen = (TreeId, u64, Vec<(u64, Call, FramePos)>);

/// The actor's saved trees replayed over records: each step's own
/// transition ([`Shape::apply`], [`Shape::save`], a fork's copy) and each
/// save's recorded layout laid on its saved map ([`Layout::lay`]), as the
/// actor laid it at the call. What a start serves from, never waiting
/// for history, and how it numbers objects at the built point: object numbers
/// are the journal's, never history's.
pub(in crate::tree_actor) struct Replay {
    pub(in crate::tree_actor) trees: BTreeMap<TreeId, SavedTree>,
    opens: BTreeMap<u64, ReplayOpen>,
    pages: BTreeMap<(TreeId, u64), Vec<(u64, pages::Run)>>,
    /// False: shapes only, each save's map left as it was.
    maps: bool,
    /// Chunk records no take or replace has taken yet.
    bytes: Vec<(u64, Tail)>,
}

impl Replay {
    /// From `trees` as of a point where `opens` were live with these calls
    /// and `page_writes` held: each anonymous open's object added.
    ///
    /// # Errors
    ///
    /// [`Error::Format`] for an open of a tree `trees` lacks.
    pub(in crate::tree_actor) fn new(
        mut trees: BTreeMap<TreeId, SavedTree>,
        opens: &BTreeMap<u64, FoldedOpen>,
        page_writes: BTreeMap<(TreeId, u64), Vec<(u64, pages::Run)>>,
        bytes: Vec<(u64, Tail)>,
    ) -> Result<Self> {
        let mut live = BTreeMap::new();
        for (handle, folded) in opens {
            let open = &folded.open;
            if open.anonymous {
                trees
                    .get_mut(&open.tree)
                    .ok_or(Error::Format)?
                    .add_anonymous(open.object, open.meta.ok_or(Error::Format)?, 0);
            }
            live.insert(*handle, (open.tree, open.object, folded.calls.clone()));
        }
        Ok(Self {
            trees,
            opens: live,
            pages: page_writes,
            maps: true,
            bytes,
        })
    }

    fn tree(&mut self, tree: &str) -> Result<(TreeId, &mut SavedTree)> {
        let id = tree.parse::<TreeId>()?;
        Ok((id, self.trees.get_mut(&id).ok_or(Error::Format)?))
    }

    fn opened(&mut self, handle: u64, tree: TreeId, object: u64) {
        self.opens.insert(handle, (tree, object, Vec::new()));
        self.pages.entry((tree, object)).or_default();
    }

    /// One record.
    ///
    /// # Errors
    ///
    /// [`Error::Format`] for a record naming a tree, open or run the replay
    /// lacks; a transition's own failure.
    pub(in crate::tree_actor) fn apply(&mut self, record: &Record, tail: &Tail) -> Result<()> {
        let seq = record.seq.get();
        match &record.entry {
            Entry::Checkpoint { .. } => return Err(Error::Format),
            Entry::Open { tree, object, .. } => {
                let tree = tree.parse::<TreeId>()?;
                self.opened(seq, tree, *object);
            }
            Entry::AnonymousOpen { tree, meta, .. } => {
                let (id, t) = self.tree(tree)?;
                t.add_anonymous(seq, *meta, record.at_ns);
                self.opened(seq, id, seq);
            }
            Entry::Call { handle, call } => {
                call.check_tail(tail.len)?;
                self.opens.get_mut(handle).ok_or(Error::Format)?.2.push((
                    seq,
                    call.clone(),
                    tail.pos.clone(),
                ));
            }
            Entry::PageWrite { tree, object, runs } => self
                .pages
                .entry((tree.parse::<TreeId>()?, *object))
                .or_default()
                .extend(runs_from_frame(runs, tail)?.into_iter().map(|r| (seq, r))),
            Entry::Bytes => self.bytes.push((seq, tail.clone())),
            Entry::Step { change, .. } => self.step(seq, record.at_ns, change, tail)?,
        }
        Ok(())
    }

    fn step(&mut self, seq: u64, at_ns: u64, change: &Change, tail: &Tail) -> Result<()> {
        let event = EventNumber::new(seq);
        match change {
            Change::Create { tree, .. } => {
                let (id, t) = self.tree(tree)?;
                t.shape.apply(change, event, at_ns)?;
                let mut empty = FileMap::empty();
                empty.saved_at(RecordNo::new(seq));
                t.bytes.insert(event.object(), empty);
                self.opened(seq, id, event.object());
            }
            Change::Symlink { tree, .. } => {
                let (_, t) = self.tree(tree)?;
                t.shape.apply(change, event, at_ns)?;
                t.bytes.insert(event.object(), link_map(tail, seq)?);
            }
            Change::Link { tree, .. }
            | Change::Rename { tree, .. }
            | Change::Unlink { tree, .. }
            | Change::Mkdir { tree, .. }
            | Change::Rmdir { tree, .. }
            | Change::SetMeta { tree, .. }
            | Change::SetTimes { tree, .. } => {
                self.tree(tree)?.1.shape.apply(change, event, at_ns)?;
            }
            Change::Save {
                tree,
                handle,
                outcome,
                ..
            } => {
                let (id, object, calls) = self.opens.remove(handle).ok_or(Error::Format)?;
                let saved = match outcome {
                    SaveOutcome::Saved { len, chunks } => {
                        let chunks = take_chunks(&mut self.bytes, seq, *chunks)?;
                        if self.maps {
                            // The recorded layout, laid on the saved map: the
                            // map the actor served at the call.
                            let layout = Layout::read(*len, &chunks)?;
                            let mut known = Positions::default();
                            for (_, _, pos) in &calls {
                                known.add(pos);
                            }
                            for (_, run) in self.pages.get(&(id, object)).into_iter().flatten() {
                                known.add(&run.at);
                            }
                            let t = self.trees.get(&id).ok_or(Error::Format)?;
                            let mut map = layout
                                .lay(t.bytes.get(&object).ok_or(Error::Format)?, &mut |place| {
                                    Ok(known.find(place))
                                })?;
                            map.saved_at(RecordNo::new(seq));
                            Some(map)
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                let (_, t) = self.tree(tree)?;
                t.shape.save(object, *outcome, at_ns);
                if matches!(outcome, SaveOutcome::AnonymousDropped) {
                    t.drop_object(object);
                }
                if let Some(map) = saved {
                    t.bytes.insert(object, map);
                }
                if !self.opens.values().any(|o| o.0 == id && o.1 == object) {
                    self.pages.remove(&(id, object));
                }
            }
            Change::Fork { upstream } => {
                let mut fork = self
                    .trees
                    .get(&upstream.parse::<TreeId>()?)
                    .ok_or(Error::Format)?
                    .clone();
                for object in fork.shape.anonymous.clone() {
                    fork.drop_object(object);
                }
                self.trees.insert(TreeId::Fork(seq), fork);
            }
            Change::Rows => {}
            Change::Drop { fork } => {
                let fork = fork.parse::<TreeId>()?;
                self.trees.remove(&fork).ok_or(Error::Format)?;
                self.pages.retain(|(tree, _), _| *tree != fork);
            }
            Change::Take { tree, take } => {
                let chunks = take_chunks(&mut self.bytes, seq, take.chunks)?;
                let maps = if self.maps {
                    take.maps(&chunks, RecordNo::new(seq))?
                } else {
                    Vec::new()
                };
                let (_, t) = self.tree(tree)?;
                t.shape.apply(change, event, at_ns)?;
                for (object, map) in maps {
                    t.bytes.insert(object, map);
                }
            }
            Change::Replace {
                tree,
                path,
                len,
                chunks,
                ..
            } => {
                let chunks = take_chunks(&mut self.bytes, seq, *chunks)?;
                let map = if self.maps {
                    Some(chunk_map(&chunks, *len, RecordNo::new(seq))?)
                } else {
                    None
                };
                let (_, t) = self.tree(tree)?;
                let object = t
                    .names
                    .get(path.as_str())
                    .copied()
                    .unwrap_or(event.object());
                t.shape.apply(change, event, at_ns)?;
                if let Some(map) = map {
                    t.bytes.insert(object, map);
                }
            }
        }
        Ok(())
    }
}

/// A link's saved map: its target, the bytes of its step's own tail.
///
/// # Errors
///
/// As [`FileMap::apply`].
pub(in crate::tree_actor) fn link_map(tail: &Tail, seq: u64) -> Result<FileMap> {
    let mut map = FileMap::empty();
    map.apply(
        &Call::Write {
            offset: 0,
            len: tail.len,
        },
        &tail.pos,
    )?;
    map.saved_at(RecordNo::new(seq));
    Ok(map)
}

pub(in crate::tree_actor) fn parse_oid(hex: &str) -> Result<Oid> {
    Oid::from_str(hex).map_err(|_| Error::Format)
}

/// A page write's runs, each where its bytes are in the frame's tail.
pub(in crate::tree_actor) fn runs_from_frame(
    heads: &[RunHead],
    tail: &Tail,
) -> Result<Vec<pages::Run>> {
    let mut at = 0_u64;
    let mut runs = Vec::with_capacity(heads.len());
    for head in heads {
        head.offset.checked_add(head.len).ok_or(Error::Format)?;
        runs.push(pages::Run {
            offset: head.offset,
            len: head.len,
            at: tail.pos.at(at),
        });
        at = at.checked_add(head.len).ok_or(Error::Format)?;
    }
    if at == tail.len {
        Ok(runs)
    } else {
        Err(Error::Format)
    }
}

/// Adds a journal's frames up. Pure. Frames that disagree with each other, or
/// a tree string that is no [`TreeId`], cannot come from a good journal:
/// [`Error::Format`], or [`Error::PointerMoved`] for a pointer.
pub(in crate::tree_actor) fn fold(frames: &[Frame]) -> Result<Folded> {
    let mut frames = frames.iter();
    let Some((
        Record {
            entry: Entry::Checkpoint(checkpoint),
            ..
        },
        _,
    )) = frames.next()
    else {
        return Err(Error::Format);
    };
    let Checkpoint {
        salt,
        built,
        pointers,
        upstreams,
        opens,
        page_saved,
        objects,
        locals,
        step_heads,
        ..
    } = checkpoint;
    let next = checkpoint.next();
    if step_heads.keys().any(|name| !pointers.contains_key(name)) {
        return Err(Error::Format);
    }
    let mut folded = Folded {
        // A pointer a record after the checkpoint may still name by its
        // step stands as that step (C4); its oid is the checkpoint's.
        pointers: Pointers::new(
            pointers
                .iter()
                .map(|(name, hex)| {
                    parse_oid(hex)?;
                    let target = match step_heads.get(name) {
                        Some(step) if step <= built => Target::Step(*step),
                        Some(_) => return Err(Error::Format),
                        None => Target::Oid(hex.clone()),
                    };
                    Ok((name.clone(), target))
                })
                .collect::<Result<_>>()?,
        ),
        salt: *salt,
        built: *built,
        objects: objects
            .iter()
            .map(|(tree, objects)| {
                let objects = objects.iter().map(|(id, object)| (*id, *object)).collect();
                Ok((tree.parse::<TreeId>()?, objects))
            })
            .collect::<Result<_>>()?,
        locals: locals
            .iter()
            .map(|(tree, locals)| Ok((tree.parse::<TreeId>()?, locals.clone())))
            .collect::<Result<_>>()?,
        upstreams: Upstreams::default(),
        next_seq: next.get(),
        opens: BTreeMap::new(),
        page_writes: BTreeMap::new(),
        saved_through: page_saved
            .iter()
            .map(|(tree, object, seq)| Ok(((tree.parse::<TreeId>()?, *object), *seq)))
            .collect::<Result<_>>()?,
        bytes: Vec::new(),
    };
    for (fork, upstream) in upstreams {
        folded.add_upstream(fork, upstream)?;
    }
    for open in opens {
        folded.add_open(LiveOpen::parse(open)?);
    }
    for (record, tail) in frames {
        let seq = record.seq.get();
        folded.next_seq = folded.next_seq.max(seq.saturating_add(1));
        match &record.entry {
            Entry::Checkpoint { .. } => return Err(Error::Format),
            Entry::Open {
                tree, by, object, ..
            } => folded.add_open(LiveOpen {
                handle: seq,
                tree: tree.parse::<TreeId>()?,
                by: by.clone(),
                object: *object,
                anonymous: false,
                meta: None,
            }),
            Entry::AnonymousOpen { tree, by, meta } => folded.add_open(LiveOpen {
                handle: seq,
                tree: tree.parse::<TreeId>()?,
                by: by.clone(),
                object: seq,
                anonymous: true,
                meta: Some(*meta),
            }),
            Entry::Call { handle, call } => {
                call.check_tail(tail.len)?;
                folded
                    .opens
                    .get_mut(handle)
                    .ok_or(Error::Format)?
                    .calls
                    .push((seq, call.clone(), tail.pos.clone()));
            }
            Entry::PageWrite { tree, object, runs } => folded
                .page_writes
                .entry((tree.parse::<TreeId>()?, *object))
                .or_default()
                .extend(runs_from_frame(runs, tail)?.into_iter().map(|r| (seq, r))),
            Entry::Bytes => folded.bytes.push((seq, tail.clone())),
            Entry::Step {
                by,
                change,
                pointers,
                ..
            } => {
                // EXPERIMENT (erratum 2026-11-22): a head's step and its oid
                // are the same value; the actor switched it with no record.
                let named: Vec<PointerChange> = pointers
                    .iter()
                    .map(|change| {
                        let mut change = change.clone();
                        if let (Some(Target::Oid(oid)), Some(Target::Step(step))) =
                            (&change.old, folded.pointers.get(&change.name))
                            && step_heads.get(&change.name) == Some(&step)
                            && checkpoint.pointers.get(&change.name) == Some(oid)
                        {
                            change.old = Some(Target::Step(step));
                        }
                        change
                    })
                    .collect();
                folded.pointers.check_targets(&named)?;

                folded.pointers.apply_targets(pointers);
                folded.add_step(seq, by, change)?;
                if matches!(
                    change,
                    Change::Take { .. }
                        | Change::Replace { .. }
                        | Change::Save {
                            outcome: SaveOutcome::Saved { .. },
                            ..
                        }
                ) {
                    folded.bytes.clear();
                }
            }
        }
    }
    // Page state lives from a file's write-open to the close of its last one.
    let live: BTreeSet<(TreeId, u64)> = folded
        .opens
        .values()
        .map(|o| (o.open.tree, o.open.object))
        .collect();
    folded.page_writes.retain(|key, _| live.contains(key));
    folded.saved_through.retain(|key, _| live.contains(key));
    Ok(folded)
}

#[cfg(all(test, feature = "failpoints"))]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::os::unix::fs::FileExt;
    use std::path::Path;

    use super::super::{HEADER, crc_body, header_bytes, read_frame, segment, u32_at};
    use crate::tree_actor::TreeId;
    use crate::tree_actor::test_support::{ATTRS, new_actor, reopen, writer};

    /// Rewrites the tail of the first chunk record ([`super::Entry::Bytes`])
    /// in the journal's one segment with `tail` (the same length), and fixes
    /// its checksum.
    fn rewrite_chunk(root: &Path, tail: &[u8]) {
        let listed = segment::list(&root.join("journal")).unwrap();
        let path = &listed[0].1;
        let bytes = fs::read(path).unwrap();
        let salt = u32_at(&bytes, 4);
        let mut at = 0;
        loop {
            let (header, head, old) = read_frame(&bytes[at..], salt).unwrap();
            if head.ends_with(b"\"entry\":\"Bytes\"}") {
                assert_eq!(old.len(), tail.len());
                let fixed = header_bytes(
                    salt,
                    header.record,
                    header.mark,
                    header.flags,
                    head.len() as u32,
                    tail.len() as u32,
                    crc_body(head, tail),
                );
                let file = OpenOptions::new().write(true).open(path).unwrap();
                file.write_all_at(&fixed, at as u64).unwrap();
                file.write_all_at(tail, (at + HEADER + head.len()) as u64)
                    .unwrap();
                file.sync_all().unwrap();
                return;
            }
            at += header.frame_len() as usize;
        }
    }

    /// A4: a start's replay serves a save as its recorded layout lays it on
    /// the saved map, never as its calls would: a layout crafted to differ
    /// from the calls (five zeros where the call wrote "bytes") is what a
    /// restart serves.
    #[test]
    fn a_replay_serves_the_recorded_layout_not_the_calls() {
        let tmp = tempfile::tempdir().unwrap();
        let _unbuilt = crate::failpoints::configure("tree.build_before_batch=error@*").unwrap();
        let mut v = new_actor(tmp.path());
        let h = v
            .write_open(TreeId::Root, "a", &writer("w"), Some(ATTRS))
            .unwrap();
        v.write(h, 0, b"bytes", false).unwrap();
        v.close(h).unwrap();
        v.durability().make_all_durable().unwrap();
        drop(v);
        // One range: offset 0, length 5, record 0 (zeros), place 0.
        let zeros: Vec<u8> = [0_u64, 5, 0, 0]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        rewrite_chunk(tmp.path(), &zeros);
        let v = reopen(tmp.path());
        assert_eq!(v.read(TreeId::Root, "a").unwrap(), vec![0; 5]);
    }
}
