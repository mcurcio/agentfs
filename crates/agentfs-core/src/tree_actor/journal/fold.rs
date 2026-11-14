//! What a journal's frames add up to: the fold recovery starts from, and each
//! tree's object numbers, whose one home is the journal (target git-model §3,
//! §12).

use std::collections::{BTreeMap, BTreeSet};

use agentfs_contracts::pod::WriterAt;
use git2::Oid;

use super::durable::RecordNo;
use super::segment::FramePos;
use super::{Change, Entry, Frame, OpenRecord, Record, RunHead, Tail, Target};
use crate::tree_actor::history::upstreams::Upstreams;
use crate::tree_actor::history::{Objects, objects_of};
use crate::tree_actor::pointers::Pointers;
use crate::tree_actor::recording::{Call, pages};
use crate::tree_actor::tree::{EventNumber, Shape};
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
}

impl LiveOpen {
    pub(in crate::tree_actor) fn parse(record: &OpenRecord) -> Result<Self> {
        Ok(Self {
            handle: record.handle,
            tree: record.tree.parse::<TreeId>()?,
            by: record.by.clone(),
            object: record.object,
            anonymous: record.anonymous,
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
    /// The pointers as the records name them: oids from the checkpoint, and
    /// the steps after it as [`Target`]s.
    pub(in crate::tree_actor) pointers: Pointers<Target>,
    /// The checkpoint's redo start: its pointers' oids are as of this record.
    pub(in crate::tree_actor) built: RecordNo,
    /// Each tree's object numbers as of `built`, from the checkpoint.
    pub(in crate::tree_actor) objects: BTreeMap<TreeId, Objects>,
    /// Each fork's upstream. The root has none.
    pub(in crate::tree_actor) upstreams: Upstreams,
    pub(in crate::tree_actor) next_seq: u64,
    pub(in crate::tree_actor) opens: BTreeMap<u64, FoldedOpen>,
    /// A file's page writes while a write-open of it lives.
    pub(in crate::tree_actor) page_writes: BTreeMap<(TreeId, u64), Vec<(u64, pages::Run)>>,
    /// The last step that closed a write-open of a file, while one lives.
    pub(in crate::tree_actor) saved_through: BTreeMap<(TreeId, u64), u64>,
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
            }),
            Change::Link { object, .. } => self
                .opens
                .values_mut()
                .filter(|o| o.open.object == *object)
                .for_each(|o| o.open.anonymous = false),
            Change::Save { handle, .. } => {
                let open = self.opens.remove(handle).ok_or(Error::Format)?.open;
                self.saved_through.insert((open.tree, open.object), seq);
            }
            Change::Fork { upstream } => {
                self.upstreams.add(seq, upstream.parse::<TreeId>()?);
            }
            Change::Rename { .. } | Change::Unlink { .. } | Change::Take { .. } => {}
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

/// Each tree's object numbers after the steps of `frames`, which follow the
/// checkpoint folded in `checkpoint`; `shapes` are the trees' shapes at its
/// built point. Each step's own transition ([`Shape::apply`], [`Shape::save`],
/// a fork's copy), the one the actor ran at the call: object numbers are the
/// journal's, never history's.
///
/// # Errors
///
/// [`Error::Format`] for a step naming a tree or open the journal lacks; a
/// transition's own failure.
pub(in crate::tree_actor) fn objects_through(
    mut shapes: BTreeMap<TreeId, Shape>,
    checkpoint: &Folded,
    frames: &[Frame],
) -> Result<BTreeMap<TreeId, Objects>> {
    let mut opens: BTreeMap<u64, (TreeId, u64)> = BTreeMap::new();
    for (handle, folded) in &checkpoint.opens {
        let open = &folded.open;
        if open.anonymous {
            shapes
                .get_mut(&open.tree)
                .ok_or(Error::Format)?
                .add_anonymous(open.object);
        }
        opens.insert(*handle, (open.tree, open.object));
    }
    for (record, _) in frames {
        let seq = record.seq.get();
        match &record.entry {
            Entry::Open { tree, object, .. } => {
                opens.insert(seq, (tree.parse()?, *object));
            }
            Entry::AnonymousOpen { tree, .. } => {
                let tree = tree.parse::<TreeId>()?;
                shapes
                    .get_mut(&tree)
                    .ok_or(Error::Format)?
                    .add_anonymous(seq);
                opens.insert(seq, (tree, seq));
            }
            Entry::Step { change, .. } => match change {
                Change::Fork { upstream } => {
                    let mut shape = shapes
                        .get(&upstream.parse::<TreeId>()?)
                        .ok_or(Error::Format)?
                        .clone();
                    for object in std::mem::take(&mut shape.anonymous) {
                        shape.drop_object(object);
                    }
                    shapes.insert(TreeId::Fork(seq), shape);
                }
                Change::Save {
                    handle, outcome, ..
                } => {
                    let (tree, object) = opens.remove(handle).ok_or(Error::Format)?;
                    shapes
                        .get_mut(&tree)
                        .ok_or(Error::Format)?
                        .save(object, *outcome);
                }
                Change::Create { tree, .. }
                | Change::Link { tree, .. }
                | Change::Rename { tree, .. }
                | Change::Unlink { tree, .. }
                | Change::Take { tree, .. } => {
                    let tree = tree.parse::<TreeId>()?;
                    shapes
                        .get_mut(&tree)
                        .ok_or(Error::Format)?
                        .apply(change, EventNumber::new(seq))?;
                    if matches!(change, Change::Create { .. }) {
                        opens.insert(seq, (tree, seq));
                    }
                }
            },
            Entry::Checkpoint { .. } | Entry::Call { .. } | Entry::PageWrite { .. } => {}
        }
    }
    shapes
        .iter()
        .map(|(tree, shape)| Ok((*tree, objects_of(shape)?)))
        .collect()
}

pub(in crate::tree_actor) fn parse_oid(hex: &str) -> Result<Oid> {
    Oid::from_str(hex).map_err(|_| Error::Format)
}

/// A page write's runs, each where its bytes are in the frame's tail.
pub(in crate::tree_actor) fn runs_from_frame(heads: &[RunHead], tail: &Tail) -> Result<Vec<pages::Run>> {
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
            entry:
                Entry::Checkpoint {
                    next,
                    built,
                    pointers,
                    upstreams,
                    opens,
                    page_saved,
                    objects,
                    ..
                },
            ..
        },
        _,
    )) = frames.next()
    else {
        return Err(Error::Format);
    };
    let mut folded = Folded {
        pointers: Pointers::new(
            pointers
                .iter()
                .map(|(name, hex)| {
                    parse_oid(hex)?;
                    Ok((name.clone(), Target::Oid(hex.clone())))
                })
                .collect::<Result<_>>()?,
        ),
        built: *built,
        objects: objects
            .iter()
            .map(|(tree, objects)| Ok((tree.parse::<TreeId>()?, objects.clone())))
            .collect::<Result<_>>()?,
        upstreams: Upstreams::default(),
        next_seq: next.get(),
        opens: BTreeMap::new(),
        page_writes: BTreeMap::new(),
        saved_through: page_saved
            .iter()
            .map(|(tree, object, seq)| Ok(((tree.parse::<TreeId>()?, *object), *seq)))
            .collect::<Result<_>>()?,
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
            }),
            Entry::AnonymousOpen { tree, by } => folded.add_open(LiveOpen {
                handle: seq,
                tree: tree.parse::<TreeId>()?,
                by: by.clone(),
                object: seq,
                anonymous: true,
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
            Entry::Step {
                by,
                change,
                pointers,
            } => {
                folded.pointers.check_targets(pointers)?;
                folded.pointers.apply_targets(pointers);
                folded.add_step(seq, by, change)?;
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
