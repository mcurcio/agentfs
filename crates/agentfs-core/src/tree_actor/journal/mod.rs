//! The tree actor's journal: the commit point for byte calls and pointer steps
//! (target git-model §3, §12; docs/trees.md, "Recording and durability").
//!
//! ```text
//! journal/checkpoint               the newest checkpoint: one frame
//! journal/<first record:020>.seg   one segment, about 64 MiB of frames
//! journal/carry-<built:020>        live frames a compaction carried
//! frame  = header head tail
//! header = "AFJ3" | u32le salt | u64le record | u64le mark | u32le flags
//!          | u32le head_len | u32le tail_len | u32le crc32(head + tail)
//!          | u32le crc32(first 40 header bytes)                    (44 bytes)
//! head   = JSON of the record's time and entry
//! tail   = the raw bytes of the record's calls or runs, or a take's files,
//!          in order
//! mark   = the durable mark when the frame was appended
//! flags  = bit 0 `carried`: a live open's frame copied forward, its
//!          original record number kept
//! ```
//!
//! A frame is appended with one `write`, and no fsync: a record is in the
//! kernel when the call returns, and durable once a FLUSH, an fsync or an
//! FSYNCDIR has made it so ([`durable`]). A frame never spans two segments.
//! The header is the record number's one home. Each write-open and step names
//! its writer with the pod context it ran in (`WriterAt`), so a recovery close
//! commits as the writer that opened.
//!
//! History's compactor, on its own thread and never under the actor's lock,
//! writes the checkpoint file as of history's built point B ([`compaction`]),
//! then unlinks every segment the actor has rolled past whose records are all
//! at or below B. A live open's frames, held page writes and untaken chunks
//! are copied into a carry file only when the dead bytes they pin exceed
//! their own. Recovery folds the checkpoint, the live frames it names, and
//! every frame after B.
//!
//! The salt is random per journal and kept through compaction, so a workload's
//! bytes inside a tail cannot pass as a frame. At an invalid frame after the
//! last valid record `r`, or a record number past `r + 1` (a lost segment), a
//! later valid frame whose mark passes `r` shows that a record in the hole was
//! durable: that is corruption, which an operator decides about
//! (docs/runbooks/recording-failures.md). Otherwise the hole is a torn tail,
//! records no fsync had covered: it is saved aside and cut off, with every
//! later segment.

mod commit_point;
pub(super) mod compaction;
pub(super) mod durable;
mod fold;
pub(super) mod readback;
pub(super) mod segment;

use crate::fsutil::sync_dir;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentfs_contracts::pod::WriterAt;
use serde::{Deserialize, Serialize};

use super::meta::{Meta, SetTime, Times};
use super::recording::{Call, FileMap};
use super::stages::{self, Timer, Which};
use super::tree::{MergeFileIdentity, NameHistory};
use super::{Error, Result, referee};
use compaction::{Checkpoint, Live, Published};
use durable::{Durability, RecordNo};
#[cfg(test)]
pub(super) use fold::LiveOpen;
pub(super) use fold::{Folded, FoldedOpen, Replay, fold, link_map, runs_from_frame};
use segment::{FramePos, Segment, carry_name};

const MAGIC: &[u8; 4] = b"AFJ3";
const HEADER: usize = 44;
/// The header bytes its own checksum covers.
const HEADER_SUMMED: usize = 40;
pub(super) const MAX_FRAME: usize = 96 << 20;
/// Header flag bit 0: a live frame compaction copied into a carry file.
const CARRIED: u32 = 1;

/// One frame as a record and where its tail is.
pub(super) type Frame = (Record, Tail);

/// A frame's tail: its bytes stay in the segment, read where needed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Tail {
    pub pos: FramePos,
    pub len: u64,
}

#[cfg(test)]
impl Tail {
    /// The tail's bytes in memory, for tests.
    pub fn bytes(&self) -> io::Result<Vec<u8>> {
        self.pos.bytes(0, self.len)
    }
}

/// One record: its number is read from and written to the frame's header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Record {
    pub seq: RecordNo,
    /// When it was recorded. It also gives the times a tree presents (a
    /// step's mtime and ctime); still no rule reads it (ledger R2).
    pub at_ns: u64,
    pub entry: Entry,
}

/// A record's head as the frame stores it: everything but its number.
#[derive(Serialize)]
struct HeadOut<'a> {
    at_ns: u64,
    entry: &'a Entry,
}

#[derive(Deserialize)]
struct HeadIn {
    at_ns: u64,
    entry: Entry,
}

impl Record {
    fn head(&self) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec(&HeadOut {
            at_ns: self.at_ns,
            entry: &self.entry,
        })
    }

    fn parse(seq: RecordNo, head: &[u8]) -> serde_json::Result<Self> {
        let HeadIn { at_ns, entry } = serde_json::from_slice(head)?;
        Ok(Self { seq, at_ns, entry })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Entry {
    /// The checkpoint file's one frame, never a segment's: what every record
    /// at or below its `built` adds up to ([`Checkpoint`]). The live frames
    /// before it are read from the segments and carry files that hold them.
    Checkpoint(Checkpoint),
    /// An open of an existing file; no pointers move.
    Open {
        tree: String,
        path: String,
        by: WriterAt,
        object: u64,
    },
    AnonymousOpen {
        tree: String,
        by: WriterAt,
        meta: Meta,
    },
    Call {
        handle: u64,
        call: CallHead,
    },
    /// The file's, never a handle's (ledger R1.4).
    PageWrite {
        tree: String,
        object: u64,
        runs: Vec<RunHead>,
    },
    /// Part of the bytes of the take or replace step that follows
    /// ([`TakeRecord::chunks`], [`Change::Replace`]), or of the layout of the
    /// save that follows ([`SaveOutcome::Saved`]), in its tail. Chunk
    /// records no step follows (a replace cut short, or one that changed
    /// nothing) are never read.
    Bytes,
    /// A step: the change itself, the commit point of it, and the pointer
    /// changes its build publishes. The builder makes its commits later:
    /// the change's, then one event commit per row, in order, on its chain.
    Step {
        by: WriterAt,
        change: Change,
        pointers: Vec<PointerChange>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        rows: Vec<RowRecord>,
    },
}

/// One event commit the build writes, in order: `chain` its ref
/// (`refs::row_chain`), `row` the chain's new `row.json` text, the rest its
/// message (`Message::Row`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::tree_actor) struct RowRecord {
    pub chain: String,
    pub row: String,
    pub kind: String,
    pub detail: String,
    pub by: String,
    pub at: agentfs_contracts::pod::PodContext,
}

/// What one step changes (git-model §12). The builder replays it on the
/// built trees with the transitions the actor applied (`Shape::apply`,
/// `close_calls`, `Call::apply`) and decides nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Change {
    /// A create; it opens handle = this record. Object = this record's number.
    Create {
        tree: String,
        path: String,
        meta: Meta,
    },
    /// A directory; its number is this record's.
    Mkdir {
        tree: String,
        path: String,
        meta: Meta,
    },
    /// An empty directory removed.
    Rmdir {
        tree: String,
        path: String,
    },
    /// A symbolic link: object = this record's number, its bytes (the
    /// target) this record's tail.
    Symlink {
        tree: String,
        path: String,
        meta: Meta,
    },
    /// A chmod or chown: the entry's new metadata, its kind kept. `""` is
    /// the root directory.
    SetMeta {
        tree: String,
        path: String,
        meta: Meta,
    },
    /// An explicit set of times. It builds no commit: times are each tree's
    /// own. `Now` is this record's time.
    SetTimes {
        tree: String,
        path: String,
        atime: Option<SetTime>,
        mtime: Option<SetTime>,
    },
    /// An anonymous object's first name.
    Link {
        tree: String,
        object: u64,
        path: String,
    },
    /// A write-open's close: its own calls (every `Call` record of `handle`)
    /// with these page-write pieces, and the outcome the actor decided.
    Save {
        tree: String,
        handle: u64,
        pages: Vec<PagePiece>,
        outcome: SaveOutcome,
    },
    /// A rename; for a directory, every entry under it moves.
    Rename {
        tree: String,
        from: String,
        to: String,
    },
    Unlink {
        tree: String,
        path: String,
    },
    /// A fork of `upstream`; fork id = this record's number.
    Fork {
        upstream: String,
    },
    /// Rows alone: no branch moves.
    Rows,
    /// A fork dropped: its branch and every entry of its queue deleted (the
    /// step's pointer changes); the actor, its replay and the builder forget
    /// the tree, and the builder deletes its checkout at its copy.
    Drop {
        fork: String,
    },
    /// A queue take, decided on the built snapshot. The tails of the
    /// [`Entry::Bytes`] records just before it hold the bytes of each file it
    /// leaves (`take.files` with a file, in order).
    Take {
        tree: String,
        take: Box<TakeRecord>,
    },
    /// A whole-file replace: afterwards `path` holds the `len` bytes in the
    /// tails of the `chunks` [`Entry::Bytes`] records just before it, with
    /// `meta`. A file there keeps its object and identity, as a save; a
    /// missing one is created at this record's number, as a create and a
    /// save.
    Replace {
        tree: String,
        path: String,
        meta: Meta,
        len: u64,
        chunks: u64,
    },
}

/// What one step adds to a commit besides its files' bytes: a commit, its
/// tree's three hashed folder levels down each changed path, and a line map
/// (docs/trees.md, "What a commit holds"). Hashed sub-folders hold at most
/// 256 entries a level, so this does not grow with the tree.
const STEP_LISTINGS: u64 = 3 * 256 * 40;

impl Change {
    /// The tree a step's build works in, and about how many bytes it writes:
    /// the bytes a save or take leaves and the listings each step writes
    /// (the lag gate's estimate; the builder's measured rate corrects it).
    /// `None` for rows alone, a drop and a set of times: they add to no
    /// tree's lag.
    ///
    /// # Errors
    ///
    /// [`Error::Format`] for a tree that is no [`super::TreeId`].
    pub fn build_cost(&self) -> Result<Option<(super::TreeId, u64)>> {
        let (tree, bytes) = match self {
            Self::Rows | Self::Drop { .. } | Self::SetTimes { .. } => return Ok(None),
            Self::Create { tree, .. }
            | Self::Mkdir { tree, .. }
            | Self::Rmdir { tree, .. }
            | Self::Symlink { tree, .. }
            | Self::SetMeta { tree, .. }
            | Self::Link { tree, .. }
            | Self::Rename { tree, .. }
            | Self::Unlink { tree, .. } => (tree, 0),
            Self::Take { tree, take } => (tree, take.lens.iter().sum()),
            Self::Replace { tree, len, .. } => (tree, *len),
            Self::Save { tree, outcome, .. } => match outcome {
                SaveOutcome::Saved { len, .. } => (tree, *len),
                _ => (tree, 0),
            },
            Self::Fork { upstream } => (upstream, 0),
        };
        let tree = tree.parse().map_err(|_| Error::Format)?;
        Ok(Some((tree, bytes.saturating_add(STEP_LISTINGS))))
    }
}

/// A page write's run that a close applies, by reference (the open's own
/// calls are implied: all its `Call` records).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PagePiece {
    pub record: RecordNo,
    pub offset: u64,
    pub len: u64,
}

/// What a close saved: the result's length, and how many [`Entry::Bytes`]
/// records just before the step hold its layout (`recording::layout`): each
/// range the close wrote, the rest of the file as before. The builder checks
/// its own replay against it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum SaveOutcome {
    Saved { len: u64, chunks: u64 },
    Unchanged,
    OrphanDropped,
    AnonymousDropped,
}

/// A take as decided: how the head moves, and the files and names it leaves
/// that differ from the tree before it, each file's line map here and its
/// bytes in the chunk records before it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct TakeRecord {
    pub entry: String,
    pub whole: bool,
    /// Audit: the built point the decision read.
    pub decided_at: RecordNo,
    pub head: TakeHead,
    pub files: Vec<(MergeFileIdentity, Option<referee::File>)>,
    pub names: Vec<(MergeFileIdentity, Option<NameHistory>)>,
    pub collisions: Vec<referee::Collision>,
    /// Each directory it adds, changes (`Some`) or removes (`None`).
    pub dirs: Vec<(String, Option<Meta>)>,
    /// The length of each file it leaves, in `files` order.
    pub lens: Vec<u64>,
    /// How many [`Entry::Bytes`] records just before it hold those files'
    /// bytes, in order.
    pub chunks: u64,
}

impl TakeRecord {
    /// Each file it leaves as its object's saved map at step `seq`, reading
    /// its bytes from `chunks`, the tails of the records just before it: what
    /// the actor, its replay and the builder all install.
    ///
    /// # Errors
    ///
    /// [`Error::Format`] when the chunks are not its own or hold another
    /// length than its files.
    pub fn maps(&self, chunks: &[Tail], seq: RecordNo) -> Result<Vec<(u64, FileMap)>> {
        let left: Vec<_> = self.files.iter().filter_map(|(_, f)| f.as_ref()).collect();
        if chunks.len() as u64 != self.chunks || left.len() != self.lens.len() {
            return Err(Error::Format);
        }
        Ok(left
            .iter()
            .map(|file| file.object)
            .zip(chunk_maps(chunks, &self.lens, seq)?)
            .collect())
    }

    /// How many chunk records `total` bytes take.
    pub fn chunks_for(total: u64) -> u64 {
        total.div_ceil(TAKE_CHUNK)
    }
}

/// The files laid end to end in `chunks` (the tails of [`Entry::Bytes`]
/// records), `lens` long each, as saved maps of step `seq` reading their
/// bytes there: a take's files and a replace's one file.
///
/// # Errors
///
/// [`Error::Format`] when the chunks hold another length than the files.
pub(super) fn chunk_maps(chunks: &[Tail], lens: &[u64], seq: RecordNo) -> Result<Vec<FileMap>> {
    let total: u64 = chunks.iter().map(|c| c.len).sum();
    if total != lens.iter().sum::<u64>() {
        return Err(Error::Format);
    }
    let mut chunks = chunks.iter();
    let mut at: Option<(&Tail, u64)> = None;
    let mut maps = Vec::with_capacity(lens.len());
    for len in lens {
        let mut map = FileMap::empty();
        let mut done = 0;
        while done < *len {
            let (chunk, offset) = match at {
                Some(place) if place.1 < place.0.len => place,
                _ => (chunks.next().ok_or(Error::Format)?, 0),
            };
            let n = (len - done).min(chunk.len - offset);
            map.apply(
                &Call::Write {
                    offset: done,
                    len: n,
                },
                &chunk.pos.at(offset),
            )?;
            done += n;
            at = Some((chunk, offset + n));
        }
        map.saved_at(seq);
        maps.push(map);
    }
    Ok(maps)
}

/// A replace step's file: its `len` bytes in `chunks`, as the saved map of
/// step `seq`.
///
/// # Errors
///
/// [`Error::Format`] when the chunks hold another length.
pub(super) fn chunk_map(chunks: &[Tail], len: u64, seq: RecordNo) -> Result<FileMap> {
    chunk_maps(chunks, &[len], seq)?.pop().ok_or(Error::Format)
}

/// The most bytes one [`Entry::Bytes`] record holds: a take's or replace's
/// files of any size are recorded in frames well under a segment.
pub(super) const TAKE_CHUNK: u64 = 16 << 20;

/// A take's, replace's or save's chunks: the tails of the `n` [`Entry::Bytes`] records
/// just before step `seq`, taken from `pending` (every such record not yet
/// taken, in order); every pending one before it goes too, being a cut-short
/// step's.
///
/// # Errors
///
/// [`Error::Format`] when they are not the `n` records just before it.
pub(super) fn take_chunks(pending: &mut Vec<(u64, Tail)>, seq: u64, n: u64) -> Result<Vec<Tail>> {
    let n = usize::try_from(n).map_err(|_| Error::Format)?;
    let from = pending.len().checked_sub(n).ok_or(Error::Format)?;
    let chunks: Vec<(u64, Tail)> = pending.drain(from..).collect();
    pending.clear();
    for (i, (record, _)) in chunks.iter().enumerate() {
        if *record + (n - i) as u64 != seq {
            return Err(Error::Format);
        }
    }
    Ok(chunks.into_iter().map(|(_, tail)| tail).collect())
}

/// How a take moves its tree's head.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum TakeHead {
    /// To an existing commit.
    FastForward { to: String },
    /// A landing merge of the queued commit.
    Landing { incoming: String },
    /// A sync merge of the upstream's commit.
    Sync { target: String },
    /// No head move: only the queue entry goes.
    Stay,
}

/// A live write-open, in a checkpoint or the step that created it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct OpenRecord {
    pub handle: u64,
    pub tree: String,
    pub by: WriterAt,
    pub object: u64,
    /// Never linked: its object has no name and no saved state.
    pub anonymous: bool,
    /// An anonymous open's metadata, which no commit holds yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

/// What a checkpoint keeps of one tree that history never holds: each
/// directory's number and times by path (`""` the root), and each file's
/// times by identity.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct TreeLocals {
    pub dirs: BTreeMap<String, (u64, Times)>,
    pub files: BTreeMap<u64, Times>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum CallHead {
    Write { offset: u64, len: u64 },
    Append { len: u64 },
    Truncate(u64),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RunHead {
    pub offset: u64,
    pub len: u64,
}

/// A pointer's value in a record: an oid, or the value step `n`'s build gives
/// this pointer (its commit, or its object table), resolved by the builder and
/// kept in the built log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Target {
    Oid(String),
    Step(RecordNo),
}

/// One pointer change. `None` is absent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PointerChange {
    pub name: String,
    pub new: Option<Target>,
    pub old: Option<Target>,
}

/// What one compaction let go (`JournalCompaction::unlink_covered`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::tree_actor) struct DropStats {
    pub segments: u64,
    pub carried_bytes: u64,
}

/// The actor's end of the journal: where it appends. The segment table and
/// the journal's length are [`Durability`]'s.
pub(super) struct Journal {
    dir: PathBuf,
    /// The segment appended to: the newest in the table.
    current: Arc<Segment>,
    salt: u32,
    unknown: bool,
    durability: Arc<Durability>,
    /// The next frame starts a new segment once the current one has a record
    /// and the frame would take it past this (`Upkeep::segment_bytes`).
    segment_bytes: u64,
}

fn crc_body(head: &[u8], tail: &[u8]) -> u32 {
    let mut crc = crc32fast::Hasher::new();
    crc.update(head);
    crc.update(tail);
    crc.finalize()
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut value = [0; 4];
    value.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(value)
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut value = [0; 8];
    value.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(value)
}

/// What a valid header says.
#[derive(Clone, Copy)]
struct Header {
    record: RecordNo,
    mark: RecordNo,
    flags: u32,
    head: usize,
    tail: usize,
    crc: u32,
}

impl Header {
    fn frame_len(&self) -> u64 {
        (HEADER + self.head + self.tail) as u64
    }
}

/// A header's 44 bytes, its own checksum last.
fn header_bytes(
    salt: u32,
    record: RecordNo,
    mark: RecordNo,
    flags: u32,
    head: u32,
    tail: u32,
    crc: u32,
) -> [u8; HEADER] {
    let mut bytes = [0; HEADER];
    bytes[..4].copy_from_slice(MAGIC);
    bytes[4..8].copy_from_slice(&salt.to_le_bytes());
    bytes[8..16].copy_from_slice(&record.get().to_le_bytes());
    bytes[16..24].copy_from_slice(&mark.get().to_le_bytes());
    bytes[24..28].copy_from_slice(&flags.to_le_bytes());
    bytes[28..32].copy_from_slice(&head.to_le_bytes());
    bytes[32..36].copy_from_slice(&tail.to_le_bytes());
    bytes[36..40].copy_from_slice(&crc.to_le_bytes());
    let header_crc = crc32fast::hash(&bytes[..HEADER_SUMMED]);
    bytes[HEADER_SUMMED..].copy_from_slice(&header_crc.to_le_bytes());
    bytes
}

/// One frame's bytes in one buffer: header, head, tail. `None` when the frame
/// would exceed [`MAX_FRAME`].
fn frame_bytes(
    salt: u32,
    record: RecordNo,
    mark: RecordNo,
    head: &[u8],
    tail: &[u8],
) -> Option<Vec<u8>> {
    if head.len().checked_add(tail.len())? > MAX_FRAME {
        return None;
    }
    let header = header_bytes(
        salt,
        record,
        mark,
        0,
        u32::try_from(head.len()).ok()?,
        u32::try_from(tail.len()).ok()?,
        crc_body(head, tail),
    );
    let mut bytes = Vec::with_capacity(HEADER + head.len() + tail.len());
    bytes.extend_from_slice(&header);
    bytes.extend_from_slice(head);
    bytes.extend_from_slice(tail);
    Some(bytes)
}

/// A header whose magic, salt and checksum are right.
fn read_header(bytes: &[u8], salt: u32) -> Option<Header> {
    if bytes.len() < HEADER
        || &bytes[..4] != MAGIC
        || u32_at(bytes, 4) != salt
        || u32_at(bytes, HEADER_SUMMED) != crc32fast::hash(&bytes[..HEADER_SUMMED])
    {
        return None;
    }
    let head = u32_at(bytes, 28) as usize;
    let tail = u32_at(bytes, 32) as usize;
    if head.checked_add(tail)? > MAX_FRAME {
        return None;
    }
    Some(Header {
        record: RecordNo::new(u64_at(bytes, 8)),
        mark: RecordNo::new(u64_at(bytes, 16)),
        flags: u32_at(bytes, 24),
        head,
        tail,
        crc: u32_at(bytes, 36),
    })
}

/// A whole valid frame at the start of `bytes`: its header, head and tail.
fn read_frame(bytes: &[u8], salt: u32) -> Option<(Header, &[u8], &[u8])> {
    let header = read_header(bytes, salt)?;
    let length = HEADER + header.head + header.tail;
    if bytes.len() < length {
        return None;
    }
    let head = &bytes[HEADER..HEADER + header.head];
    let tail = &bytes[HEADER + header.head..length];
    (crc_body(head, tail) == header.crc).then_some((header, head, tail))
}

/// Whether a later valid frame of this journal says a record past `last` was
/// durable when it was written: then a hole after `last` lost durable records.
fn durable_past(bytes: &[u8], salt: u32, last: RecordNo) -> bool {
    let mut pattern = [0; 8];
    pattern[..4].copy_from_slice(MAGIC);
    pattern[4..].copy_from_slice(&salt.to_le_bytes());
    bytes
        .windows(pattern.len())
        .enumerate()
        .filter(|(_, window)| *window == pattern)
        .filter_map(|(at, _)| read_frame(&bytes[at..], salt))
        .any(|(header, _, _)| header.mark > last)
}

/// What a reader finds at a byte of a segment.
pub(super) enum At {
    /// A whole valid frame, and its length.
    Frame(Box<Frame>, u64),
    /// A valid frame numbered past what the reader asked for.
    Later,
    /// The end, or a frame not whole yet (an append the reader raced).
    End,
}

/// The frame at byte `at` of `segment`, when it checks and numbers at most
/// `through`: the builder's read, which never parses past what it was asked
/// for. A carried frame or a checkpoint numbers at most `through` always.
pub(super) fn frame_at(
    segment: &Arc<Segment>,
    at: u64,
    salt: u32,
    through: RecordNo,
) -> io::Result<At> {
    let mut header = [0; HEADER];
    match segment.read_exact_at(&mut header, at) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(At::End),
        Err(e) => return Err(e),
    }
    let Some(header) = read_header(&header, salt) else {
        return Ok(At::End);
    };
    if header.record > through {
        return Ok(At::Later);
    }
    let mut head = vec![0; header.head];
    let tail_at = at + (HEADER + header.head) as u64;
    match segment.read_exact_at(&mut head, at + HEADER as u64) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(At::End),
        Err(e) => return Err(e),
    }
    // The body's checksum, the tail read in pieces and let go.
    let mut crc = crc32fast::Hasher::new();
    crc.update(&head);
    let mut chunk = vec![0; header.tail.min(1 << 20)];
    let mut done = 0;
    while done < header.tail {
        let n = (header.tail - done).min(chunk.len());
        match segment.read_exact_at(&mut chunk[..n], tail_at + done as u64) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(At::End),
            Err(e) => return Err(e),
        }
        crc.update(&chunk[..n]);
        done += n;
    }
    if crc.finalize() != header.crc {
        return Ok(At::End);
    }
    let record = Record::parse(header.record, &head)?;
    let tail = Tail {
        pos: FramePos::new(Arc::clone(segment), header.record, tail_at),
        len: header.tail as u64,
    };
    Ok(At::Frame(Box::new((record, tail)), header.frame_len()))
}

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("journal path has no parent"))?;
    sync_dir(parent)
}

fn draw_salt() -> io::Result<u32> {
    let mut salt = [0; 4];
    File::open("/dev/urandom")?.read_exact(&mut salt)?;
    Ok(u32::from_le_bytes(salt))
}

/// Reads a whole file of a journal: a segment or a carry file.
fn read_all(path: &Path) -> Result<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(Error::CorruptJournal(0)),
        Err(e) => Err(e.into()),
    }
}

/// A frame of `segment` at `offset` of `bytes`, its tail where it is.
fn found_frame(segment: &Arc<Segment>, header: &Header, record: Record, offset: usize) -> Frame {
    let tail_at = (offset + HEADER + header.head) as u64;
    (
        record,
        Tail {
            pos: FramePos::new(Arc::clone(segment), header.record, tail_at),
            len: header.tail as u64,
        },
    )
}

/// Where a segment's valid frames end before what follows is judged.
struct Hole {
    index: usize,
    offset: usize,
}

impl Journal {
    /// A new journal in the new directory `dir`: draws the salt, writes
    /// `first` (its salt replaced) as the checkpoint file, then the empty
    /// first segment, named `first.next()`. Each is fsynced.
    pub fn create(dir: &Path, first: &Checkpoint) -> Result<()> {
        fs::DirBuilder::new().mode(0o700).create(dir)?;
        let mut first = first.clone();
        first.salt = draw_salt()?;
        compaction::write_first(dir, &first)?;
        let segment = Segment::create(dir, first.next())?;
        segment.file().sync_all()?;
        sync_dir(dir)?;
        sync_parent(dir)?;
        Ok(())
    }

    /// The journal, the frames recovery folds, and the checkpoint it read.
    /// The frames are the checkpoint's, the live frames it names from at or
    /// before its `built` (from a carry file before a surviving segment,
    /// each once, in record order), then every frame after `built`. Every
    /// segment is checked, one at a time; no tail is held in memory. At a
    /// torn tail (an invalid frame, or a record past the next number: a lost
    /// or wholly torn segment): saves its bytes to `journal/torn`, logs
    /// `tree JournalTail: <n> bytes at <offset>`, cuts its segment (removing
    /// it when nothing valid is left) and removes every later one. With no
    /// segment left, one named for the next record is made. The directory
    /// is fsynced before it returns. The durable mark starts at the highest
    /// mark a frame carries, and at least the checkpoint's `built`; the
    /// caller makes the rest durable before serving.
    ///
    /// # Errors
    ///
    /// [`Error::CorruptJournal`] for a journal with no checkpoint file, or a
    /// damaged one, or a carry file it names missing or damaged (offset 0);
    /// at a hole that a later valid frame's mark shows held a durable record
    /// (its offset over the segments in order); and where the valid records
    /// end before `built`, the built log's point (records the builder made
    /// durable before it built them). [`Error::Format`] for a checkpoint or
    /// a carried frame in a segment. Nothing is changed then.
    pub fn open_with(
        dir: &Path,
        built: RecordNo,
        segment_bytes: u64,
    ) -> Result<(Self, Vec<Frame>, Published)> {
        let (checkpoint, checkpoint_frame) = compaction::read_checkpoint(dir)?;
        let salt = checkpoint.salt;
        let live = Live::of(&checkpoint);
        let listed = match segment::list(dir) {
            Ok(listed) => listed,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Error::CorruptJournal(0)),
            Err(e) => return Err(e.into()),
        };
        // The carried copies first: one wins over a surviving original.
        let mut before: BTreeMap<RecordNo, Frame> = BTreeMap::new();
        let mut carries = Vec::new();
        for carried in &checkpoint.carried {
            let path = dir.join(carry_name(*carried));
            let bytes = read_all(&path)?;
            let carry = Segment::open(RecordNo::ZERO, &path)?;
            let mut offset = 0;
            while offset < bytes.len() {
                let (header, head, _) =
                    read_frame(&bytes[offset..], salt).ok_or(Error::CorruptJournal(0))?;
                let record =
                    Record::parse(header.record, head).map_err(|_| Error::CorruptJournal(0))?;
                if live.names(&record) {
                    before
                        .entry(record.seq)
                        .or_insert_with(|| found_frame(&carry, &header, record, offset));
                }
                offset += header.frame_len() as usize;
            }
            carries.push(carry);
        }
        let mut segments: Vec<Arc<Segment>> = Vec::new();
        let mut after: Vec<Frame> = Vec::new();
        // The last record read, and the last valid record after `built`.
        let mut last = RecordNo::ZERO;
        let mut covered = checkpoint.built;
        let mut mark = checkpoint.built;
        let mut global = 0_usize;
        let mut hole: Option<Hole> = None;
        for (index, (first, path)) in listed.iter().enumerate() {
            let bytes = fs::read(path)?;
            let segment = Segment::open(*first, path)?;
            let mut offset = 0;
            while offset < bytes.len() {
                let Some((header, head, _)) = read_frame(&bytes[offset..], salt) else {
                    hole = Some(Hole { index, offset });
                    break;
                };
                let record = Record::parse(header.record, head)
                    .map_err(|_| Error::CorruptJournal(global + offset))?;
                if matches!(record.entry, Entry::Checkpoint(_)) || header.flags & CARRIED != 0 {
                    return Err(Error::Format);
                }
                if record.seq <= last {
                    return Err(Error::CorruptJournal(global + offset));
                }
                if record.seq > checkpoint.built {
                    // Records after the checkpoint follow on, one by one: a
                    // later number is a hole (a lost segment).
                    if record.seq != covered.next() {
                        hole = Some(Hole { index, offset });
                        break;
                    }
                    covered = record.seq;
                }
                last = record.seq;
                mark = mark.max(header.mark);
                let len = header.frame_len() as usize;
                if record.seq > checkpoint.built {
                    after.push(found_frame(&segment, &header, record, offset));
                } else if live.names(&record) {
                    before
                        .entry(record.seq)
                        .or_insert_with(|| found_frame(&segment, &header, record, offset));
                }
                offset += len;
            }
            segment.set_len(offset as u64);
            segments.push(segment);
            if hole.is_some() {
                break;
            }
            global += bytes.len();
        }
        if built > covered {
            let at = hole.as_ref().map_or(global, |hole| global + hole.offset);
            return Err(Error::CorruptJournal(at));
        }
        if let Some(Hole { index, offset }) = hole {
            // One segment in memory at a time: frames never span two.
            let rest = fs::read(&listed[index].1)?[offset..].to_vec();
            let later = &listed[index + 1..];
            let mut durable = durable_past(&rest, salt, covered);
            for (_, path) in later {
                durable = durable || durable_past(&fs::read(path)?, salt, covered);
            }
            if durable {
                return Err(Error::CorruptJournal(global + offset));
            }
            let mut saved = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(dir.join("torn"))?;
            saved.write_all(&rest)?;
            let mut torn = rest.len() as u64;
            for (_, path) in later {
                torn += io::copy(&mut File::open(path)?, &mut saved)?;
            }
            saved.sync_all()?;
            eprintln!("tree JournalTail: {torn} bytes at {}", global + offset);
            if offset == 0 {
                // Nothing valid is left in it: its name would pass the next record.
                if let Some(segment) = segments.pop() {
                    fs::remove_file(segment.path())?;
                }
            } else {
                let segment = &segments[index];
                segment.file().set_len(offset as u64)?;
                segment.file().sync_all()?;
            }
            for (_, path) in later {
                fs::remove_file(path)?;
            }
        }
        if segments.is_empty() {
            segments.push(Segment::create(dir, covered.next())?);
        }
        // A checkpoint renamed but not yet synced, and every cut above, are
        // durable before History may trim the built log by it.
        sync_dir(dir)?;
        let frames: Vec<Frame> = std::iter::once(checkpoint_frame)
            .chain(before.into_values())
            .chain(after)
            .collect();
        let current = segments.last().map(Arc::clone).ok_or(Error::CorruptJournal(0))?;
        let durability = Durability::new(segments, File::open(dir)?, covered, mark);
        for carry in carries {
            durability.add_carry(carry, 0);
        }
        let published = Published::of_open(&checkpoint);
        Ok((
            Self {
                dir: dir.to_owned(),
                current,
                salt,
                unknown: false,
                durability,
                segment_bytes,
            },
            frames,
            published,
        ))
    }

    /// [`Journal::open_with`] with the default segment size, and the frames
    /// alone: the tests' read of a journal.
    #[cfg(test)]
    pub fn open(dir: &Path, built: RecordNo) -> Result<(Self, Vec<Frame>)> {
        let (journal, frames, _) = Self::open_with(dir, built, segment::SEGMENT_BYTES)?;
        Ok((journal, frames))
    }

    /// Starts segment `first` before a frame of `bytes` bytes when the
    /// current one has records and would pass its size. The new segment is
    /// in the table before any record is appended to it: the roll event the
    /// compaction's coverage rule reads (C2a).
    fn roll(&mut self, first: RecordNo, bytes: u64) -> io::Result<bool> {
        let length = self.current.len();
        if length == 0
            || length.saturating_add(bytes) <= self.segment_bytes
            || self.current.first() >= first
        {
            return Ok(false);
        }
        let segment = Segment::create(&self.dir, first)?;
        self.durability.add_segment(Arc::clone(&segment));
        self.current = segment;
        Ok(true)
    }

    /// Appends one frame: one buffer, one `write`, no fsync. When this returns
    /// Ok the record is in the kernel and stands against a crash of agentfs;
    /// [`Durability`] makes it stand against a power cut. Returns where its
    /// tail is.
    ///
    /// # Errors
    ///
    /// A failed write whose rollback worked: the error, file as before. A
    /// failed rollback: the error, and [`Journal::unknown`] turns true.
    pub fn append(&mut self, record: &Record, tail: &[u8]) -> Result<FramePos> {
        if self.unknown {
            return Err(Error::Stopped);
        }
        let _timer = Timer::start(Which::JournalAppend);
        let head = record.head()?;
        let bytes = frame_bytes(self.salt, record.seq, self.durability.mark(), &head, tail)
            .ok_or(Error::TooLarge)?;
        stages::copied(tail.len() as u64);
        self.roll(record.seq, bytes.len() as u64)?;
        let segment = Arc::clone(&self.current);
        let at = segment.len();
        let written = (|| -> io::Result<()> {
            crate::failpoints::hit("tree.wal_before_header")?;
            segment.file().write_all_at(&bytes, at)?;
            crate::failpoints::hit("tree.wal_after_payload")
        })();
        if let Err(e) = written {
            if segment.file().set_len(at).is_err() {
                self.unknown = true;
            }
            return Err(e.into());
        }
        segment.set_len(at + bytes.len() as u64);
        self.durability.appended_to(record.seq);
        Ok(FramePos::new(
            segment,
            record.seq,
            at + (HEADER + head.len()) as u64,
        ))
    }

    /// True once this file cannot be trusted: reopen to recover.
    pub fn unknown(&self) -> bool {
        self.unknown
    }

    /// The journal's group commit: the only way its records become durable.
    pub fn durability(&self) -> &Arc<Durability> {
        &self.durability
    }

    /// The journal's salt: what tells its frames from a workload's bytes.
    pub fn salt(&self) -> u32 {
        self.salt
    }

    /// The journal's directory.
    #[cfg(test)]
    pub fn dir(&self) -> &Path {

        &self.dir
    }

    /// The linked segments' first records, in order.
    #[cfg(test)]
    pub fn segment_firsts(&self) -> Vec<RecordNo> {
        self.durability.table().iter().map(|s| s.first()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::compaction::{JournalCompaction, Sealed};
    use super::*;

    fn checkpoint(built: u64) -> Checkpoint {
        checkpoint_with(built, Vec::new())
    }

    fn checkpoint_with(built: u64, opens: Vec<OpenRecord>) -> Checkpoint {
        Checkpoint {
            salt: 0,
            built: RecordNo::new(built),
            pointers: BTreeMap::new(),
            objects: BTreeMap::new(),
            locals: BTreeMap::new(),
            upstreams: BTreeMap::new(),
            opens,
            page_saved: Vec::new(),
            page_writes: Vec::new(),
            chunks: Vec::new(),
            step_heads: BTreeMap::new(),
            carried: Vec::new(),
        }
    }

    fn call(seq: u64, handle: u64, bytes: &[u8]) -> (Record, Vec<u8>) {
        (
            Record {
                seq: RecordNo::new(seq),
                at_ns: 2,
                entry: Entry::Call {
                    handle,
                    call: CallHead::Append {
                        len: bytes.len() as u64,
                    },
                },
            },
            bytes.to_vec(),
        )
    }

    /// A new journal whose segments roll past `segment_bytes`.
    fn journal_of(segment_bytes: u64) -> (tempfile::TempDir, PathBuf, Journal) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("journal");
        Journal::create(&dir, &checkpoint(0)).unwrap();
        let (journal, frames, published) =
            Journal::open_with(&dir, RecordNo::ZERO, segment_bytes).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(published.built(), RecordNo::ZERO);
        (tmp, dir, journal)
    }

    fn journal() -> (tempfile::TempDir, PathBuf, Journal) {
        journal_of(segment::SEGMENT_BYTES)
    }

    fn tails(frames: &[Frame]) -> Vec<(u64, Vec<u8>)> {
        frames
            .iter()
            .map(|(record, tail)| (record.seq.get(), tail.bytes().unwrap()))
            .collect()
    }

    fn only_segment(dir: &Path) -> PathBuf {
        let listed = segment::list(dir).unwrap();
        assert_eq!(listed.len(), 1);
        listed[0].1.clone()
    }

    fn open_record(handle: u64) -> OpenRecord {
        OpenRecord {
            handle,
            tree: "root".into(),
            by: WriterAt {
                writer: agentfs_contracts::name::WriterLabel::agentfs(),
                at: agentfs_contracts::pod::PodContext::default(),
            },
            object: handle,
            anonymous: false,
            meta: None,
        }
    }

    /// Publishes `checkpoint` (its salt the journal's) on `journal` as the
    /// compactor does, then unlinks what it covers.
    fn compact(dir: &Path, journal: &Journal, mut checkpoint: Checkpoint) -> DropStats {
        checkpoint.salt = journal.salt();
        let port = JournalCompaction::new(dir.to_owned(), Arc::clone(journal.durability()), None);
        let durable = port.wait_durable(checkpoint.built).unwrap();
        let carried = port.carry(&checkpoint).unwrap();
        let bytes = carried.bytes();
        let sealed = Sealed::new(checkpoint, durable, carried).unwrap();
        assert!(sealed.checkpoint().carried.len() <= 1);
        let published = port.publish(&sealed).unwrap();
        let stats = port.unlink_covered(&published).unwrap();
        assert_eq!(stats.carried_bytes, bytes);
        stats
    }

    #[test]
    fn frames_round_trip_and_compaction_keeps_the_salt_and_live_tails() {
        let (_tmp, dir, mut journal) = journal_of(1);
        let first = call(1, 7, &[b"one".as_slice(), &[0; 256]].concat());
        let second = call(2, 8, b"\0two\xff");

        let pos = journal.append(&first.0, &first.1).unwrap();
        assert_eq!(pos.bytes(0, 3).unwrap(), b"one");
        // Two frames in one segment: the live one pins the dead one.
        journal.segment_bytes = segment::SEGMENT_BYTES;
        let held = journal.append(&second.0, &second.1).unwrap().at(1);
        journal.segment_bytes = 1;
        let third = call(3, 8, b"three");
        journal.append(&third.0, &third.1).unwrap();
        let salt = journal.salt;
        let stats = compact(&dir, &journal, checkpoint_with(2, vec![open_record(8)]));
        // The first segment's live frame pins more dead bytes than its own:
        // carried, and the segment unlinked.
        assert_eq!(stats.segments, 1);
        assert!(stats.carried_bytes > 0);
        assert_eq!(journal.segment_firsts(), vec![RecordNo::new(3)]);
        assert!(dir.join(carry_name(RecordNo::new(2))).exists());
        // A holder moves to the copy: the same name, the same bytes, the
        // unlinked segment no longer held.
        let relocations = journal.durability().relocations();
        let moved = held.relocated(relocations).unwrap();
        assert_eq!(moved.key(), held.key());
        assert_eq!(moved.bytes(0, 3).unwrap(), b"two");
        assert_eq!(moved.segment.first(), RecordNo::ZERO);
        assert!(held.relocated(relocations).is_some());
        assert!(moved.relocated(relocations).is_none());
        let fourth = call(4, 8, b"four");
        journal.append(&fourth.0, &fourth.1).unwrap();
        drop(journal);
        let (journal, frames) = Journal::open(&dir, RecordNo::ZERO).unwrap();
        assert_eq!(journal.salt, salt);
        assert!(matches!(frames[0].0.entry, Entry::Checkpoint(_)));
        assert_eq!(
            tails(&frames[1..]),
            vec![(2, second.1.clone()), (3, third.1), (4, fourth.1)]
        );
    }

    #[test]
    fn a_segment_whose_live_frames_outweigh_the_dead_is_kept() {
        let (_tmp, dir, mut journal) = journal();
        let small = call(1, 7, b"x");
        let big = call(2, 8, &[9; 4096]);
        journal.append(&small.0, &small.1).unwrap();
        journal.append(&big.0, &big.1).unwrap();
        journal.segment_bytes = 1;
        let next = call(3, 8, b"y");
        journal.append(&next.0, &next.1).unwrap();
        let stats = compact(&dir, &journal, checkpoint_with(2, vec![open_record(8)]));
        assert_eq!(stats, DropStats::default());
        assert_eq!(journal.segment_firsts().len(), 2);
        drop(journal);
        let (_, frames) = Journal::open(&dir, RecordNo::ZERO).unwrap();
        assert_eq!(tails(&frames[1..]), vec![(2, big.1), (3, next.1)]);
    }

    /// The newest segment is never covered, and one is covered only once the
    /// next starts at or before the checkpoint's `built + 1` (C2, C2a).
    #[test]
    fn only_a_segment_the_journal_rolled_past_is_covered() {
        let (_tmp, dir, mut journal) = journal_of(1);
        for seq in 1..=3 {
            let (record, tail) = call(seq, 1, b"dead");
            journal.append(&record, &tail).unwrap();
        }
        // Built through 2: segment 2's next starts at 3, so 1 and 2 go.
        assert_eq!(compact(&dir, &journal, checkpoint(2)).segments, 2);
        assert_eq!(journal.segment_firsts(), vec![RecordNo::new(3)]);
        // Built through 3: 3 is the newest, never covered.
        assert_eq!(compact(&dir, &journal, checkpoint(3)).segments, 0);
        drop(journal);
        let (_, frames) = Journal::open(&dir, RecordNo::new(3)).unwrap();
        assert_eq!(frames.len(), 1, "every record is the checkpoint's");
    }

    #[test]
    fn segments_roll_past_their_size_and_a_frame_never_spans_two() {
        let (_tmp, dir, mut journal) = journal();
        let block = vec![5; 1 << 20];
        for seq in 1..=70 {
            let (record, tail) = call(seq, 1, &block);
            journal.append(&record, &tail).unwrap();
        }
        let listed = segment::list(&dir).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed[0].0 == RecordNo::new(1));
        for (_, path) in &listed {
            assert!(fs::metadata(path).unwrap().len() <= segment::SEGMENT_BYTES);
        }
        assert_eq!(
            journal.durability().len(),
            listed
                .iter()
                .map(|(_, p)| fs::metadata(p).unwrap().len())
                .sum::<u64>()
        );
        assert_eq!(
            journal.durability().make_all_durable(),
            Ok(RecordNo::new(70))
        );
        drop(journal);
        let (_, frames) = Journal::open(&dir, RecordNo::ZERO).unwrap();
        assert_eq!(frames.len(), 71);
    }

    #[test]
    fn a_frame_over_the_limit_is_refused_before_the_file_changes() {
        let (_tmp, dir, mut journal) = journal();
        let path = only_segment(&dir);
        let before = fs::read(&path).unwrap();
        let huge = vec![0; MAX_FRAME];
        let record = call(1, 1, b"").0;
        assert!(matches!(
            journal.append(&record, &huge),
            Err(Error::TooLarge)
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!journal.unknown());
    }

    /// No checkpoint file, or a damaged one, is corruption at offset 0;
    /// `checkpoint.tmp` is never read.
    #[test]
    fn a_missing_or_damaged_checkpoint_is_corruption_at_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("journal");
        assert!(matches!(
            Journal::open(&dir, RecordNo::ZERO),
            Err(Error::CorruptJournal(0))
        ));
        fs::create_dir(&dir).unwrap();
        assert!(matches!(
            Journal::open(&dir, RecordNo::ZERO),
            Err(Error::CorruptJournal(0))
        ));
        fs::write(dir.join(compaction::CHECKPOINT_TMP), b"").unwrap();
        assert!(matches!(
            Journal::open(&dir, RecordNo::ZERO),
            Err(Error::CorruptJournal(0))
        ));
        let path = dir.join(compaction::CHECKPOINT);
        fs::write(&path, vec![7; 100]).unwrap();
        assert!(matches!(
            Journal::open(&dir, RecordNo::ZERO),
            Err(Error::CorruptJournal(0))
        ));
        assert_eq!(fs::read(&path).unwrap(), vec![7; 100]);
    }

    #[test]
    fn a_torn_tail_is_saved_and_truncated() {
        let (_tmp, dir, mut journal) = journal();
        let first = call(1, 1, b"one");
        journal.append(&first.0, &first.1).unwrap();
        let length = journal.durability().len();
        drop(journal);
        let path = only_segment(&dir);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"AFJ3 torn").unwrap();
        drop(file);
        let (journal, frames) = Journal::open(&dir, RecordNo::ZERO).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(journal.durability().len(), length);
        assert_eq!(fs::metadata(&path).unwrap().len(), length);
        assert_eq!(fs::read(dir.join("torn")).unwrap(), b"AFJ3 torn");
    }

    /// Three calls; the second's head damaged on disk. `durable` makes the
    /// first two durable before the third is appended, so its mark covers them.
    fn damaged_middle(durable: bool) -> (tempfile::TempDir, PathBuf, u64) {
        let (tmp, dir, mut journal) = journal();
        let first = call(1, 1, b"one");
        journal.append(&first.0, &first.1).unwrap();
        let hole = journal.durability().len();
        let second = call(2, 1, b"two");
        journal.append(&second.0, &second.1).unwrap();
        if durable {
            journal.durability().make_durable(RecordNo::new(2)).unwrap();
        }
        let third = call(3, 1, b"three");
        journal.append(&third.0, &third.1).unwrap();
        drop(journal);
        let path = only_segment(&dir);
        let mut bytes = fs::read(&path).unwrap();
        // The first byte of the second frame's head: its body checksum fails.
        bytes[usize::try_from(hole).unwrap() + HEADER] ^= 0xff;
        fs::write(&path, bytes).unwrap();
        (tmp, dir, hole)
    }

    #[test]
    fn a_hole_a_later_mark_covers_is_corruption() {
        let (_tmp, dir, hole) = damaged_middle(true);
        let path = only_segment(&dir);
        let before = fs::read(&path).unwrap();
        match Journal::open(&dir, RecordNo::ZERO) {
            Err(Error::CorruptJournal(at)) => assert_eq!(at as u64, hole),
            other => panic!("expected corruption, got {:?}", other.map(|(_, f)| f.len())),
        }
        assert_eq!(fs::read(&path).unwrap(), before, "nothing changed");
    }

    #[test]
    fn a_hole_no_later_mark_covers_is_a_torn_tail() {
        let (_tmp, dir, hole) = damaged_middle(false);
        let (journal, frames) = Journal::open(&dir, RecordNo::ZERO).unwrap();
        assert_eq!(frames.len(), 2, "the checkpoint and the first call");
        assert_eq!(journal.durability().len(), hole);
        assert_eq!(fs::metadata(only_segment(&dir)).unwrap().len(), hole);
        assert!(dir.join("torn").exists());
    }

    /// A hole no later mark covers, but the built log is past it: the builder
    /// made those records durable before it built them, so they were lost to
    /// the disk, not torn. Cutting them would move refs back.
    #[test]
    fn a_hole_the_built_log_passes_is_corruption() {
        let (_tmp, dir, hole) = damaged_middle(false);
        let path = only_segment(&dir);
        let before = fs::read(&path).unwrap();
        match Journal::open(&dir, RecordNo::new(2)) {
            Err(Error::CorruptJournal(at)) => assert_eq!(at as u64, hole),
            other => panic!("expected corruption, got {:?}", other.map(|(_, f)| f.len())),
        }
        assert_eq!(fs::read(&path).unwrap(), before, "nothing changed");
        assert!(!dir.join("torn").exists());
        // Built only through the first: the hole is a torn tail.
        assert!(Journal::open(&dir, RecordNo::new(1)).is_ok());
    }

    #[test]
    fn a_torn_tail_in_an_earlier_segment_removes_the_later_ones() {
        let (_tmp, dir, mut journal) = journal();
        let block = vec![5; 1 << 20];
        for seq in 1..=66 {
            let (record, tail) = call(seq, 1, &block);
            journal.append(&record, &tail).unwrap();
        }
        drop(journal);
        let listed = segment::list(&dir).unwrap();
        assert_eq!(listed.len(), 2);
        // The last frame of the first segment torn: nothing was durable.
        let first = &listed[0].1;
        let length = fs::metadata(first).unwrap().len();
        OpenOptions::new()
            .write(true)
            .open(first)
            .unwrap()
            .set_len(length - 10)
            .unwrap();
        let (_, frames) = Journal::open(&dir, RecordNo::ZERO).unwrap();
        assert!(frames.len() < 66);
        assert_eq!(segment::list(&dir).unwrap().len(), 1);
    }

    /// C7: covered segments unlinked and the next segment's frames lost, or
    /// no segment left at all, with nothing after the checkpoint durable:
    /// the journal opens at the checkpoint, a segment named for the next
    /// record made where none is left.
    #[test]
    fn a_journal_with_nothing_durable_after_its_checkpoint_opens() {
        let (_tmp, dir, mut journal) = journal_of(1);
        for seq in 1..=4 {
            let (record, tail) = call(seq, 1, b"dead");
            journal.append(&record, &tail).unwrap();
        }
        compact(&dir, &journal, checkpoint(3));
        drop(journal);
        let listed = segment::list(&dir).unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
        // The next segment's first frame torn: never durable.
        fs::write(&listed[0].1, b"AFJ3").unwrap();
        let (journal, frames) = Journal::open(&dir, RecordNo::new(3)).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(journal.segment_firsts(), vec![RecordNo::new(4)]);
        drop(journal);
        // No segment left at all.
        for (_, path) in segment::list(&dir).unwrap() {
            fs::remove_file(path).unwrap();
        }
        let (mut journal, frames) = Journal::open(&dir, RecordNo::new(3)).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(journal.segment_firsts(), vec![RecordNo::new(4)]);
        let (record, tail) = call(4, 1, b"next");
        journal.append(&record, &tail).unwrap();
        drop(journal);
        let (_, frames) = Journal::open(&dir, RecordNo::new(3)).unwrap();
        assert_eq!(tails(&frames[1..]), vec![(4, b"next".to_vec())]);
    }

    /// 2.13: a lost segment after the checkpoint that held a record proven
    /// durable refuses to open: by a later frame's mark, or by the built log
    /// past the lost records with no later segment.
    #[test]
    fn a_lost_durable_segment_refuses_to_open() {
        let (_tmp, dir, mut journal) = journal_of(1);
        for seq in 1..=2 {
            let (record, tail) = call(seq, 1, b"durable");
            journal.append(&record, &tail).unwrap();
        }
        journal.durability().make_all_durable().unwrap();
        let (record, tail) = call(3, 1, b"marked");
        journal.append(&record, &tail).unwrap();
        drop(journal);
        let listed = segment::list(&dir).unwrap();
        assert_eq!(listed.len(), 3);
        // The middle segment unlinked by hand: record 3's mark says 2 was durable.
        fs::remove_file(&listed[1].1).unwrap();
        assert!(matches!(
            Journal::open(&dir, RecordNo::ZERO),
            Err(Error::CorruptJournal(_))
        ));
        assert_eq!(segment::list(&dir).unwrap().len(), 2, "nothing changed");
        // No later segment, but the built log is past the lost record.
        fs::remove_file(&listed[2].1).unwrap();
        assert!(matches!(
            Journal::open(&dir, RecordNo::new(2)),
            Err(Error::CorruptJournal(_))
        ));
        // Built only through the first: what was lost was never durable.
        assert!(Journal::open(&dir, RecordNo::new(1)).is_ok());
    }

    /// The builder's reader never reads past the durable mark: a frame in the
    /// kernel but not yet fsynced may still be torn.
    #[test]
    fn the_builder_never_reads_past_the_mark() {
        use crate::tree_actor::history::builder::JournalReader;
        let (_tmp, _dir, mut journal) = journal();
        for seq in 1..=2 {
            let (record, tail) = call(seq, 1, b"durable");
            journal.append(&record, &tail).unwrap();
        }
        journal.durability().make_all_durable().unwrap();
        let (record, tail) = call(3, 1, b"unsynced");
        journal.append(&record, &tail).unwrap();
        assert_eq!(journal.durability().mark(), RecordNo::new(2));
        let mut reader = JournalReader::open(
            Arc::clone(journal.durability()),
            journal.salt(),
            RecordNo::ZERO,
        )
        .unwrap();
        let (frames, _) = reader
            .read(journal.durability().appended(), usize::MAX)
            .unwrap();
        let read: Vec<u64> = frames.iter().map(|(r, _)| r.seq.get()).collect();
        assert_eq!(read, vec![1, 2], "the unsynced frame waits for its fsync");
    }

    /// Rewrites, in the live segment, the head of the first frame whose head
    /// holds `field` followed by `old`, with `new` (the same length) in its
    /// place, and fixes its checksums: a damaged record that still reads.
    #[cfg(feature = "failpoints")]
    fn rewrite_head(root: &Path, field: &str, old: &str, new: &str) {
        assert_eq!(old.len(), new.len());
        let path = only_segment(&root.join("journal"));
        let bytes = fs::read(&path).unwrap();
        let salt = u32_at(&bytes, 4);
        let needle = format!("{field}{old}");
        let mut at = 0;
        loop {
            let (header, head, _) = read_frame(&bytes[at..], salt).unwrap();
            let text = String::from_utf8(head.to_vec()).unwrap();
            if let Some(found) = text.find(&needle) {
                let mut head = head.to_vec();
                let place = found + field.len();
                head[place..place + old.len()].copy_from_slice(new.as_bytes());
                let tail =
                    bytes[at + HEADER + head.len()..at + header.frame_len() as usize].to_vec();
                let fixed = header_bytes(
                    salt,
                    header.record,
                    header.mark,
                    header.flags,
                    head.len() as u32,
                    tail.len() as u32,
                    crc_body(&head, &tail),
                );
                let file = OpenOptions::new().write(true).open(&path).unwrap();
                file.write_all_at(&fixed, at as u64).unwrap();
                file.write_all_at(&head, (at + HEADER) as u64).unwrap();
                file.sync_all().unwrap();
                return;
            }
            at += header.frame_len() as usize;
        }
    }

    /// A5: a recorded layout that names bytes "as before" past the file it
    /// is laid on (its save's length tampered with) cannot be laid: building
    /// stops at its step with the alarm (`Disagrees`), and recording goes on.
    #[cfg(feature = "failpoints")]
    #[test]
    fn a_layout_past_the_file_stops_the_builder() {
        use crate::tree_actor::history::built::{BuildError, WaitError};
        use crate::tree_actor::test_support::{ATTRS, new_actor, writer};
        use crate::tree_actor::{Saved, TreeId};
        let tmp = tempfile::tempdir().unwrap();
        let unbuilt = crate::failpoints::configure("tree.build_before_batch=error@*").unwrap();
        let mut v = new_actor(tmp.path());
        let h = v
            .write_open(TreeId::Root, "a", &writer("w"), Some(ATTRS))
            .unwrap();
        v.write(h, 0, b"bytes", false).unwrap();
        let Saved::Commit(step) = v.close(h).unwrap() else {
            panic!("the close saves");
        };
        v.durability().make_all_durable().unwrap();
        // The step's length, 5, as 9: four bytes "as before" past an empty file.
        rewrite_head(tmp.path(), "\"Saved\":{\"len\":", "5", "9");
        drop(unbuilt);
        match v.settle() {
            Err(Error::History(WaitError::Failed(BuildError::Disagrees(at)))) => {
                assert_eq!(at, step);
            }
            other => panic!("expected Disagrees, got {other:?}"),
        }
        assert_eq!(v.read(TreeId::Root, "a").unwrap(), b"bytes");
        let h = v
            .write_open(TreeId::Root, "b", &writer("w"), Some(ATTRS))
            .unwrap();
        v.write(h, 0, b"recorded", false).unwrap();
        v.close(h).unwrap();
        assert_eq!(v.read(TreeId::Root, "b").unwrap(), b"recorded");
    }

    /// A6: a save whose page-write piece names a run the builder does not
    /// track (the page write's record names another file) replays to
    /// nothing: the commit is flagged, history holds the bytes the workload
    /// saved, read from the journal by the run's record, and building goes
    /// on.
    #[cfg(feature = "failpoints")]
    #[test]
    fn a_save_whose_pieces_name_no_run_is_flagged_and_keeps_its_bytes() {
        use crate::tree_actor::test_support::{ATTRS, new_actor, writer};
        use crate::tree_actor::{TreeId, UnverifiedCounts};
        let tmp = tempfile::tempdir().unwrap();
        let unbuilt = crate::failpoints::configure("tree.build_before_batch=error@*").unwrap();
        let mut v = new_actor(tmp.path());
        let h = v
            .write_open(TreeId::Root, "a", &writer("w"), Some(ATTRS))
            .unwrap();
        v.write(h, 0, b"hello world", false).unwrap();
        v.page_writeback(h, 6, b"PAGES").unwrap();
        v.close(h).unwrap();
        let saved = v.read(TreeId::Root, "a").unwrap();
        assert_eq!(saved, b"hello PAGES");
        v.durability().make_all_durable().unwrap();
        // The page write's record names object 9, a file that never was.
        rewrite_head(
            tmp.path(),
            "\"object\":",
            &format!("{h},\"runs\""),
            "9,\"runs\"",
        );
        drop(unbuilt);
        v.settle().unwrap();
        assert_eq!(v.read(TreeId::Root, "a").unwrap(), saved);
        let repo = v.history().repo().clone();
        let head = v.head(TreeId::Root).unwrap();
        let message = String::from_utf8(repo.read_commit(head).unwrap().message).unwrap();
        assert!(
            message.contains("\nAgentfs-Unverified: recorded 11:")
                && message.contains(" replayed none by "),
            "{message}"
        );
        let checkout = crate::tree_actor::history::checkout_path(tmp.path(), TreeId::Root, "a");
        assert_eq!(fs::read(checkout).unwrap(), saved);
        assert_eq!(
            v.verification().counts(),
            UnverifiedCounts {
                unresolved: 1,
                mismatch_confirmed: 0
            }
        );
    }

    #[test]
    fn a_call_is_appended_without_an_fsync() {
        let (_tmp, _dir, mut journal) = journal();
        let first = call(1, 1, b"one");
        journal.append(&first.0, &first.1).unwrap();
        assert_eq!(journal.durability().appended(), RecordNo::new(1));
        assert_eq!(journal.durability().mark(), RecordNo::ZERO);
        assert_eq!(
            journal.durability().make_all_durable(),
            Ok(RecordNo::new(1))
        );
    }
}
