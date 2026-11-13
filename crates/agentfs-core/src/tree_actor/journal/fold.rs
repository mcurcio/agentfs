//! What a journal's frames add up to: the fold recovery starts from (target git-model §12).

use std::collections::{BTreeMap, BTreeSet};

use git2::Oid;

use super::{Entry, Frame, OpenRecord, PointerChange, Record, RunHead};
use crate::tree_actor::history::upstreams::Upstreams;
use crate::tree_actor::pointers::Pointers;
use crate::tree_actor::recording::{Call, pages};
use crate::tree_actor::{Error, Result, TreeId};
use crate::tree_git::RefUpdate;

/// A live write-open: a journal [`OpenRecord`] with its tree parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) struct LiveOpen {
    pub(in crate::tree_actor) handle: u64,
    pub(in crate::tree_actor) tree: TreeId,
    pub(in crate::tree_actor) writer: String,
    pub(in crate::tree_actor) object: u64,
    /// Never linked: its object has no name and no saved state.
    pub(in crate::tree_actor) anonymous: bool,
}

impl LiveOpen {
    pub(in crate::tree_actor) fn parse(record: &OpenRecord) -> Result<Self> {
        Ok(Self {
            handle: record.handle,
            tree: record.tree.parse::<TreeId>()?,
            writer: record.writer.clone(),
            object: record.object,
            anonymous: record.anonymous,
        })
    }

    pub(in crate::tree_actor) fn record(&self) -> OpenRecord {
        OpenRecord {
            handle: self.handle,
            tree: self.tree.to_string(),
            writer: self.writer.clone(),
            object: self.object,
            anonymous: self.anonymous,
        }
    }
}

/// A live open and its calls, as the journal leaves them.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::tree_actor) struct FoldedOpen {
    pub(in crate::tree_actor) open: LiveOpen,
    pub(in crate::tree_actor) calls: Vec<(u64, Call)>,
}

/// What a journal's frames add up to (git-model §12).
#[derive(Debug, PartialEq, Eq)]
pub(in crate::tree_actor) struct Folded {
    pub(in crate::tree_actor) pointers: Pointers,
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

pub(in crate::tree_actor) fn parse_oid(hex: &str) -> Result<Oid> {
    Oid::from_str(hex).map_err(|_| Error::Format)
}

pub(in crate::tree_actor) fn parse_change(change: &PointerChange) -> Result<RefUpdate> {
    Ok(RefUpdate {
        refname: change.name.clone(),
        new: change.new.as_deref().map(parse_oid).transpose()?,
        expected_old: change.old.as_deref().map(parse_oid).transpose()?,
    })
}

pub(in crate::tree_actor) fn runs_from_frame(
    heads: &[RunHead],
    tail: &[u8],
) -> Result<Vec<pages::Run>> {
    let mut rest = tail;
    let mut runs = Vec::with_capacity(heads.len());
    for head in heads {
        let len = usize::try_from(head.len).map_err(|_| Error::Format)?;
        if rest.len() < len {
            return Err(Error::Format);
        }
        let (bytes, after) = rest.split_at(len);
        runs.push(pages::Run {
            offset: head.offset,
            bytes: bytes.to_vec(),
        });
        rest = after;
    }
    if rest.is_empty() {
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
                    next_seq,
                    pointers,
                    upstreams,
                    opens,
                    page_saved,
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
                .map(|(name, hex)| Ok((name.clone(), parse_oid(hex)?)))
                .collect::<Result<_>>()?,
        ),
        upstreams: Upstreams::default(),
        next_seq: *next_seq,
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
        let seq = record.seq;
        folded.next_seq = folded.next_seq.max(seq.saturating_add(1));
        match &record.entry {
            Entry::Checkpoint { .. } => return Err(Error::Format),
            Entry::Open {
                tree,
                writer,
                object,
                ..
            } => folded.add_open(LiveOpen {
                handle: seq,
                tree: tree.parse::<TreeId>()?,
                writer: writer.clone(),
                object: *object,
                anonymous: false,
            }),
            Entry::AnonymousOpen { tree, writer } => folded.add_open(LiveOpen {
                handle: seq,
                tree: tree.parse::<TreeId>()?,
                writer: writer.clone(),
                object: seq,
                anonymous: true,
            }),
            Entry::Call { handle, call } => folded
                .opens
                .get_mut(handle)
                .ok_or(Error::Format)?
                .calls
                .push((seq, Call::from_frame(call, tail)?)),
            Entry::PageWrite { tree, object, runs } => folded
                .page_writes
                .entry((tree.parse::<TreeId>()?, *object))
                .or_default()
                .extend(runs_from_frame(runs, tail)?.into_iter().map(|r| (seq, r))),
            Entry::Step {
                closes,
                opened,
                linked,
                fork,
                pointers,
                ..
            } => {
                let changes = pointers
                    .iter()
                    .map(parse_change)
                    .collect::<Result<Vec<_>>>()?;
                folded.pointers.check(&changes)?;
                folded.pointers.apply(&changes);
                if let Some((name, parent)) = fork {
                    folded.add_upstream(name, parent)?;
                }
                if let Some(open) = opened {
                    folded.add_open(LiveOpen::parse(open)?);
                }
                if let Some(object) = linked {
                    folded
                        .opens
                        .values_mut()
                        .filter(|o| o.open.object == *object)
                        .for_each(|o| o.open.anonymous = false);
                }
                if let Some(handle) = closes {
                    let open = folded.opens.remove(handle).ok_or(Error::Format)?.open;
                    folded.saved_through.insert((open.tree, open.object), seq);
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
