//! The tree actor's journal: the commit point for byte calls and pointer steps
//! (target git-model §3, §12; docs/trees.md, "Recording and durability").
//!
//! ```text
//! journal/<first record:020>.seg   one segment, about 64 MiB of frames
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
//! Compaction writes a checkpoint frame at the start of a new segment, as of
//! the built point, then drops (unlinks) every older segment whose records
//! are all built and which holds no live open's call or held page write. A
//! live open's frames are copied forward only when the dead bytes they pin
//! exceed their own. Recovery folds the last checkpoint, the live frames it
//! names from before it, and every frame after it.
//!
//! The salt is random per journal and kept through compaction, so a workload's
//! bytes inside a tail cannot pass as a frame. At an invalid frame after the
//! last valid record `r`, a later valid frame whose mark passes `r` shows that
//! a record in the hole was durable: that is corruption, which an operator
//! decides about (docs/runbooks/recording-failures.md). Otherwise the hole is
//! a torn tail, records no fsync had covered: it is saved aside and cut off,
//! with every later segment.

mod commit_point;
pub(super) mod durable;
mod fold;
pub(super) mod segment;

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentfs_contracts::pod::WriterAt;
use serde::{Deserialize, Serialize};

use super::stages::{self, Timer, Which};
use super::tree::{MergeFileIdentity, NameHistory};
use super::{Error, Result, referee};
pub(super) use commit_point::compact_at;
use durable::{Durability, RecordNo};
use segment::{FramePos, SEGMENT_BYTES, Segment};
#[cfg(test)]
pub(super) use fold::Folded;
pub(super) use fold::{FoldedOpen, fold, objects_through, runs_from_frame};

const MAGIC: &[u8; 4] = b"AFJ3";
const HEADER: usize = 44;
/// The header bytes its own checksum covers.
const HEADER_SUMMED: usize = 40;
pub(super) const MAX_FRAME: usize = 96 << 20;
/// Header flag bit 0: a live open's frame copied forward by compaction.
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
    /// Audit only: no rule reads it (ledger R2).
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
    /// The first frame only: what every record before `next` adds up to.
    /// `built` is the redo start, at most the built log's point: every pointer
    /// value is an oid as of it. `page_saved`: tree, object, saved_through.
    /// `objects`: each tree's object of each named file, by identity, as of
    /// `built`; the steps after it move them (git-model §3). The live opens'
    /// calls before it are read from the segments they were kept or carried
    /// in.
    Checkpoint {
        next: RecordNo,
        built: RecordNo,
        pointers: BTreeMap<String, String>,
        objects: BTreeMap<String, BTreeMap<u64, u64>>,
        upstreams: BTreeMap<String, String>,
        opens: Vec<OpenRecord>,
        page_saved: Vec<(String, u64, u64)>,
        /// The page writes before the checkpoint that memory held: recovery
        /// reads them from the segments they were kept or carried in.
        page_writes: Vec<RecordNo>,
    },
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
    /// A step: the change itself, the commit point of it, and the pointer
    /// changes its build publishes. The builder makes its commits later.
    Step {
        by: WriterAt,
        change: Change,
        pointers: Vec<PointerChange>,
    },
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
    /// A queue take, decided on the built snapshot. Its frame's tail holds
    /// the bytes of each file it leaves (`take.files` with a file, in order).
    Take {
        tree: String,
        take: TakeRecord,
    },
}

/// A page write's run that a close applies, by reference (the open's own
/// calls are implied: all its `Call` records).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PagePiece {
    pub record: RecordNo,
    pub offset: u64,
    pub len: u64,
}

/// What a close saved: the result's length and the digest of its piece list
/// (`FileMap::digest`), which the builder checks its replay against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum SaveOutcome {
    Saved { len: u64, pieces: u64 },
    Unchanged,
    OrphanDropped,
    AnonymousDropped,
}

/// A take as decided: how the head moves, and the files and names it leaves
/// that differ from the tree before it (bytes inside the line maps, and in
/// the record's tail: [`TakeRecord::tail`]).
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
}

impl TakeRecord {
    /// The record's tail: the bytes of each file it leaves, in `files` order,
    /// which the actor's and the builder's maps read.
    pub fn tail(&self) -> Vec<u8> {
        self.files
            .iter()
            .filter_map(|(_, file)| file.as_ref())
            .flat_map(|file| file.lines.bytes())
            .collect()
    }

    /// Where each file it leaves starts in [`TakeRecord::tail`], and its length.
    pub fn spans(&self) -> BTreeMap<MergeFileIdentity, (u64, u64)> {
        let mut at = 0;
        let mut spans = BTreeMap::new();
        for (identity, file) in &self.files {
            if let Some(file) = file {
                let len = file.lines.bytes().len() as u64;
                spans.insert(*identity, (at, len));
                at += len;
            }
        }
        spans
    }
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

#[derive(Debug, thiserror::Error)]
pub(super) enum CompactError {
    /// No checkpoint was written: the journal is as it was.
    #[error("journal compaction failed before its checkpoint: {0}")]
    Before(io::Error),
    /// The checkpoint is written and durable, but an older segment could not
    /// be dropped: it stays, and the next compaction tries again.
    #[error("journal compaction failed after its checkpoint: {0}")]
    After(io::Error),
}

/// What one [`Journal::drop_behind`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::tree_actor) struct DropStats {
    pub segments: u64,
    pub carried_bytes: u64,
}

pub(super) struct Journal {
    dir: PathBuf,
    /// Every linked segment in order, with its length; the last is appended to.
    segments: Vec<(Arc<Segment>, u64)>,
    salt: u32,
    unknown: bool,
    durability: Arc<Durability>,
    /// Bytes compaction has copied forward since open.
    carried: u64,
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
fn header_bytes(salt: u32, record: RecordNo, mark: RecordNo, flags: u32, head: u32, tail: u32, crc: u32) -> [u8; HEADER] {
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
    Frame(Frame, u64),
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
        pos: FramePos {
            segment: Arc::clone(segment),
            tail_at,
        },
        len: header.tail as u64,
    };
    Ok(At::Frame((record, tail), header.frame_len()))
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
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

/// The last record a checkpoint covers: every record before its `next`.
fn covered_by(checkpoint: &Record) -> RecordNo {
    match &checkpoint.entry {
        Entry::Checkpoint { next, .. } => RecordNo::new(next.get().saturating_sub(1)),
        _ => checkpoint.seq,
    }
}

/// Whether a frame from before checkpoint `checkpoint` is one it names as
/// live: a call of an open it lists, or a page write it held.
fn named_live(checkpoint: &Entry, record: &Record) -> bool {
    let Entry::Checkpoint {
        next,
        opens,
        page_writes,
        ..
    } = checkpoint
    else {
        return false;
    };
    record.seq < *next
        && match &record.entry {
            Entry::Call { handle, .. } => opens.iter().any(|open| open.handle == *handle),
            Entry::PageWrite { .. } => page_writes.binary_search(&record.seq).is_ok(),
            _ => false,
        }
}

/// One frame found at open.
struct Found {
    frame: Frame,
    carried: bool,
}

impl Journal {
    /// A new journal in the new directory `dir`, holding `first` (a
    /// checkpoint) in its first segment; draws the salt. Each is fsynced.
    pub fn create(dir: &Path, first: &Record) -> Result<()> {
        fs::DirBuilder::new().mode(0o700).create(dir)?;
        let segment = Segment::create(dir, covered_by(first).next())?;
        let head = first.head()?;
        let bytes = frame_bytes(draw_salt()?, first.seq, RecordNo::ZERO, &head, &[])
            .ok_or(Error::TooLarge)?;
        segment.file().write_all_at(&bytes, 0)?;
        segment.file().sync_all()?;
        sync_dir(dir)?;
        sync_parent(dir)?;
        Ok(())
    }

    /// The journal and the frames recovery folds: the last checkpoint, the
    /// live frames it names from before it (kept or carried, each once, in
    /// record order), then every frame after it. Every segment is checked,
    /// one at a time; no tail is held in memory. At a torn tail: saves its
    /// bytes to `journal/torn`, logs `tree JournalTail: <n> bytes at
    /// <offset>`, truncates its segment and removes every later one. The
    /// durable mark starts at the highest mark a frame carries; the caller
    /// makes the rest durable before serving.
    ///
    /// # Errors
    ///
    /// [`Error::CorruptJournal`] for a missing, empty or checkpointless
    /// journal (offset 0), or at an invalid frame that a later valid frame's
    /// mark shows held a durable record (its offset over the segments in
    /// order). Nothing is changed then.
    pub fn open(dir: &Path) -> Result<(Self, Vec<Frame>)> {
        let listed = match segment::list(dir) {
            Ok(listed) => listed,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Error::CorruptJournal(0)),
            Err(e) => return Err(e.into()),
        };
        let Some((_, first_path)) = listed.first() else {
            return Err(Error::CorruptJournal(0));
        };
        let mut start = [0; 8];
        File::open(first_path)?
            .read_exact_at(&mut start, 0)
            .map_err(|_| Error::CorruptJournal(0))?;
        if &start[..4] != MAGIC {
            return Err(Error::CorruptJournal(0));
        }
        let salt = u32_at(&start, 4);
        let mut found: Vec<Found> = Vec::new();
        let mut segments: Vec<(Arc<Segment>, u64)> = Vec::new();
        // The last frame's number, and the last record the valid frames hold.
        let mut last = RecordNo::ZERO;
        let mut covered = RecordNo::ZERO;
        let mut mark = RecordNo::ZERO;
        let mut global = 0_usize;
        let mut hole: Option<(usize, usize, Vec<u8>)> = None;
        for (index, (first, path)) in listed.iter().enumerate() {
            let bytes = fs::read(path)?;
            let segment = Segment::open(*first, path)?;
            let mut offset = 0;
            while offset < bytes.len() {
                let Some((header, head, tail)) = read_frame(&bytes[offset..], salt) else {
                    hole = Some((index, offset, bytes[offset..].to_vec()));
                    break;
                };
                let record = Record::parse(header.record, head)
                    .map_err(|_| Error::CorruptJournal(global + offset))?;
                let checkpoint = matches!(record.entry, Entry::Checkpoint { .. });
                let carried = header.flags & CARRIED != 0;
                if !checkpoint && !carried {
                    if record.seq <= last {
                        return Err(Error::CorruptJournal(global + offset));
                    }
                    last = record.seq;
                }
                covered = covered.max(if checkpoint {
                    covered_by(&record)
                } else {
                    record.seq
                });
                mark = mark.max(header.mark);
                let tail_at = (offset + HEADER + head.len()) as u64;
                found.push(Found {
                    frame: (
                        record,
                        Tail {
                            pos: FramePos {
                                segment: Arc::clone(&segment),
                                tail_at,
                            },
                            len: tail.len() as u64,
                        },
                    ),
                    carried,
                });
                offset += HEADER + header.head + header.tail;
            }
            segments.push((segment, offset as u64));
            if hole.is_some() {
                break;
            }
            global += bytes.len();
        }
        if let Some((index, offset, mut rest)) = hole {
            for (_, path) in &listed[index + 1..] {
                rest.extend_from_slice(&fs::read(path)?);
            }
            if (index == 0 && offset == 0) || durable_past(&rest[1..], salt, covered) {
                return Err(Error::CorruptJournal(global + offset));
            }
            let mut saved = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(dir.join("torn"))?;
            saved.write_all(&rest)?;
            saved.sync_all()?;
            eprintln!("tree JournalTail: {} bytes at {}", rest.len(), global + offset);
            let (segment, length) = &segments[index];
            segment.file().set_len(*length)?;
            segment.file().sync_all()?;
            for (_, path) in &listed[index + 1..] {
                fs::remove_file(path)?;
            }
            sync_dir(dir)?;
        }
        let Some(at) = found
            .iter()
            .rposition(|f| matches!(f.frame.0.entry, Entry::Checkpoint { .. }))
        else {
            return Err(Error::CorruptJournal(0));
        };
        let checkpoint = found[at].frame.clone();
        let mut before: BTreeMap<RecordNo, Frame> = BTreeMap::new();
        for (index, f) in found.iter().enumerate() {
            if (index < at || f.carried) && named_live(&checkpoint.0.entry, &f.frame.0) {
                before.entry(f.frame.0.seq).or_insert_with(|| f.frame.clone());
            }
        }
        let after = found[at + 1..]
            .iter()
            .filter(|f| !f.carried)
            .map(|f| f.frame.clone());
        let frames: Vec<Frame> = std::iter::once(checkpoint)
            .chain(before.into_values())
            .chain(after)
            .collect();
        let dir_file = File::open(dir)?;
        let durability = Durability::new(
            segments.iter().map(|(s, _)| Arc::clone(s)).collect(),
            dir_file,
            covered,
            mark,
        );
        Ok((
            Self {
                dir: dir.to_owned(),
                segments,
                salt,
                unknown: false,
                durability,
                carried: 0,
            },
            frames,
        ))
    }

    /// The segment appended to.
    fn current(&self) -> &(Arc<Segment>, u64) {
        // Open and create leave one segment at least; nothing removes the last.
        &self.segments[self.segments.len() - 1]
    }

    /// Starts segment `first` before a frame of `bytes` bytes when the
    /// current one has records and would pass its size.
    fn roll(&mut self, first: RecordNo, bytes: u64) -> io::Result<bool> {
        let (_, length) = self.current();
        if *length == 0 || length.saturating_add(bytes) <= SEGMENT_BYTES {
            return Ok(false);
        }
        let segment = Segment::create(&self.dir, first)?;
        self.durability.add_segment(Arc::clone(&segment));
        self.segments.push((segment, 0));
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
        let (segment, at) = self.current().clone();
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
        let last = self.segments.len() - 1;
        self.segments[last].1 = at + bytes.len() as u64;
        self.durability.appended_to(record.seq);
        Ok(FramePos {
            segment,
            tail_at: at + (HEADER + head.len()) as u64,
        })
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

    /// Compaction's first half: `checkpoint` as the first frame of a new
    /// segment (or of the current one while it is empty), the segment and
    /// the directory fsynced. Every record it covers is built, so durable.
    ///
    /// # Errors
    ///
    /// [`CompactError::Before`]: the journal is as it was.
    pub fn checkpoint(&mut self, checkpoint: &Record) -> std::result::Result<(), CompactError> {
        let first = covered_by(checkpoint).next();
        let head = checkpoint.head().map_err(|e| CompactError::Before(e.into()))?;
        let bytes = frame_bytes(self.salt, checkpoint.seq, self.durability.mark(), &head, &[])
            .ok_or_else(|| CompactError::Before(io::Error::from_raw_os_error(libc::EFBIG)))?;
        // An empty segment, or one holding only what this checkpoint covers
        // (an earlier checkpoint, carried frames), takes it as it is.
        let rolled = if self.current().1 == 0 || self.current().0.first() >= first {
            false
        } else {
            let segment = Segment::create(&self.dir, first).map_err(CompactError::Before)?;
            self.segments.push((segment, 0));
            true
        };
        let (segment, at) = self.current().clone();
        let written = (|| -> io::Result<()> {
            segment.file().write_all_at(&bytes, at)?;
            segment.file().sync_data()?;
            sync_dir(&self.dir)?;
            crate::failpoints::hit("tree.compact_before_drop")
        })();
        if let Err(e) = written {
            if rolled {
                self.segments.pop();
                let _ = fs::remove_file(segment.path());
            } else if segment.file().set_len(at).is_err() {
                self.unknown = true;
            }
            return Err(CompactError::Before(e));
        }
        if rolled {
            self.durability.add_segment(Arc::clone(&segment));
        }
        let last = self.segments.len() - 1;
        self.segments[last].1 = at + bytes.len() as u64;
        Ok(())
    }

    /// Drops every segment before the current one whose records are all
    /// `<= built` and which holds no frame `live` accepts (a live open's call,
    /// a held page write). A segment whose live frames pin more dead bytes
    /// than their own has them copied forward (flag `carried`, the record
    /// number kept), fsynced, and is dropped too. A reader holding a segment
    /// keeps reading it; its space returns when the last lets go.
    ///
    /// # Errors
    ///
    /// A failed read, copy or unlink: the segments not dropped yet stay.
    pub fn drop_behind(
        &mut self,
        built: RecordNo,
        live: &dyn Fn(&Record) -> bool,
    ) -> Result<DropStats> {
        let mut stats = DropStats::default();
        let mut index = 0;
        while index + 1 < self.segments.len() {
            let (segment, length) = self.segments[index].clone();
            let mut at = 0;
            let mut newest = RecordNo::ZERO;
            let mut kept: Vec<(u64, Header)> = Vec::new();
            let mut kept_bytes = 0;
            while at < length {
                let mut bytes = [0; HEADER];
                segment.read_exact_at(&mut bytes, at)?;
                let header = read_header(&bytes, self.salt)
                    .ok_or_else(|| io::Error::other("invalid frame in an open journal"))?;
                let mut head = vec![0; header.head];
                segment.read_exact_at(&mut head, at + HEADER as u64)?;
                let record = Record::parse(header.record, &head)?;
                newest = newest.max(record.seq);
                if live(&record) {
                    kept.push((at, header));
                    kept_bytes += header.frame_len();
                }
                at += header.frame_len();
            }
            if newest > built {
                break;
            }
            if !kept.is_empty() && length - kept_bytes <= kept_bytes {
                index += 1;
                continue;
            }
            if !kept.is_empty() {
                let (current, mut end) = self.current().clone();
                for (from, header) in &kept {
                    let mut body = vec![0; header.head + header.tail];
                    segment.read_exact_at(&mut body, from + HEADER as u64)?;
                    // Rewriting computes a new header checksum: never launder a bad frame.
                    if crc_body(&body[..header.head], &body[header.head..]) != header.crc {
                        return Err(io::Error::other("invalid frame in an open journal").into());
                    }
                    let copy = header_bytes(
                        self.salt,
                        header.record,
                        self.durability.mark(),
                        header.flags | CARRIED,
                        header.head as u32,
                        header.tail as u32,
                        header.crc,
                    );
                    current.file().write_all_at(&copy, end)?;
                    current.file().write_all_at(&body, end + HEADER as u64)?;
                    end += header.frame_len();
                    stats.carried_bytes += header.frame_len();
                    stages::copied(header.tail as u64);
                }
                current.file().sync_data()?;
                let last = self.segments.len() - 1;
                self.segments[last].1 = end;
            }
            fs::remove_file(segment.path())?;
            self.durability.remove_segment(segment.first());
            self.segments.remove(index);
            stats.segments += 1;
        }
        self.carried += stats.carried_bytes;
        Ok(stats)
    }

    /// Every linked segment's bytes: what compaction is due by.
    pub fn len(&self) -> u64 {
        self.segments.iter().map(|(_, length)| length).sum()
    }

    /// Bytes compaction has copied forward since open.
    pub fn carried(&self) -> u64 {
        self.carried
    }

    /// The linked segments' first records, in order.
    #[cfg(test)]
    pub fn segment_firsts(&self) -> Vec<RecordNo> {
        self.segments.iter().map(|(s, _)| s.first()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint(next: u64) -> Record {
        checkpoint_with(next, Vec::new())
    }

    fn checkpoint_with(next: u64, opens: Vec<OpenRecord>) -> Record {
        Record {
            seq: RecordNo::ZERO,
            at_ns: 1,
            entry: Entry::Checkpoint {
                next: RecordNo::new(next),
                built: RecordNo::new(next.saturating_sub(1)),
                pointers: BTreeMap::new(),
                upstreams: BTreeMap::new(),
                opens,
                page_saved: Vec::new(),
                page_writes: Vec::new(),
                objects: BTreeMap::new(),
            },
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

    fn journal() -> (tempfile::TempDir, PathBuf, Journal) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("journal");
        Journal::create(&dir, &checkpoint(1)).unwrap();
        let (journal, frames) = Journal::open(&dir).unwrap();
        assert_eq!(frames.len(), 1);
        (tmp, dir, journal)
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
        }
    }

    #[test]
    fn frames_round_trip_and_compaction_keeps_the_salt_and_live_tails() {
        let (_tmp, dir, mut journal) = journal();
        let first = call(1, 7, b"one");
        let second = call(2, 8, b"\0two\xff");
        let pos = journal.append(&first.0, &first.1).unwrap();
        assert_eq!(pos.bytes(0, 3).unwrap(), b"one");
        journal.append(&second.0, &second.1).unwrap();
        let salt = journal.salt;
        journal
            .checkpoint(&checkpoint_with(3, vec![open_record(8)]))
            .unwrap();
        let live = |record: &Record| matches!(record.entry, Entry::Call { handle: 8, .. });
        // The first segment's live frame pins more dead bytes than its own:
        // copied forward, and the segment dropped.
        let stats = journal.drop_behind(RecordNo::new(2), &live).unwrap();
        assert_eq!(stats.segments, 1);
        assert!(stats.carried_bytes > 0);
        assert_eq!(journal.segment_firsts(), vec![RecordNo::new(3)]);
        let third = call(3, 8, b"three");
        journal.append(&third.0, &third.1).unwrap();
        drop(journal);
        let (journal, frames) = Journal::open(&dir).unwrap();
        assert_eq!(journal.salt, salt);
        assert!(matches!(frames[0].0.entry, Entry::Checkpoint { .. }));
        assert_eq!(
            tails(&frames[1..]),
            vec![(2, second.1.clone()), (3, third.1.clone())]
        );
    }

    #[test]
    fn a_segment_whose_live_frames_outweigh_the_dead_is_kept() {
        let (_tmp, dir, mut journal) = journal();
        let small = call(1, 7, b"x");
        let big = call(2, 8, &[9; 4096]);
        journal.append(&small.0, &small.1).unwrap();
        journal.append(&big.0, &big.1).unwrap();
        journal
            .checkpoint(&checkpoint_with(3, vec![open_record(8)]))
            .unwrap();
        let live = |record: &Record| matches!(record.entry, Entry::Call { handle: 8, .. });
        let stats = journal.drop_behind(RecordNo::new(2), &live).unwrap();
        assert_eq!(stats, DropStats::default());
        assert_eq!(journal.segment_firsts().len(), 2);
        drop(journal);
        let (_, frames) = Journal::open(&dir).unwrap();
        assert_eq!(tails(&frames[1..]), vec![(2, big.1)]);
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
            assert!(fs::metadata(path).unwrap().len() <= SEGMENT_BYTES);
        }
        assert_eq!(
            journal.len(),
            listed
                .iter()
                .map(|(_, p)| fs::metadata(p).unwrap().len())
                .sum::<u64>()
        );
        assert_eq!(journal.durability().make_all_durable(), Ok(RecordNo::new(70)));
        drop(journal);
        let (_, frames) = Journal::open(&dir).unwrap();
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

    #[test]
    fn a_missing_empty_or_frameless_journal_is_corruption_at_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("journal");
        assert!(matches!(Journal::open(&dir), Err(Error::CorruptJournal(0))));
        fs::create_dir(&dir).unwrap();
        assert!(matches!(Journal::open(&dir), Err(Error::CorruptJournal(0))));
        let path = dir.join(segment::name_of(RecordNo::new(1)));
        fs::write(&path, b"").unwrap();
        assert!(matches!(Journal::open(&dir), Err(Error::CorruptJournal(0))));
        fs::write(&path, vec![7; 100]).unwrap();
        assert!(matches!(Journal::open(&dir), Err(Error::CorruptJournal(0))));
        assert_eq!(fs::read(&path).unwrap(), vec![7; 100]);
    }

    #[test]
    fn a_torn_tail_is_saved_and_truncated() {
        let (_tmp, dir, mut journal) = journal();
        let first = call(1, 1, b"one");
        journal.append(&first.0, &first.1).unwrap();
        let length = journal.len();
        drop(journal);
        let path = only_segment(&dir);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"AFJ3 torn").unwrap();
        drop(file);
        let (journal, frames) = Journal::open(&dir).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(journal.len(), length);
        assert_eq!(fs::metadata(&path).unwrap().len(), length);
        assert_eq!(fs::read(dir.join("torn")).unwrap(), b"AFJ3 torn");
    }

    /// Three calls; the second's head damaged on disk. `durable` makes the
    /// first two durable before the third is appended, so its mark covers them.
    fn damaged_middle(durable: bool) -> (tempfile::TempDir, PathBuf, u64) {
        let (tmp, dir, mut journal) = journal();
        let first = call(1, 1, b"one");
        journal.append(&first.0, &first.1).unwrap();
        let hole = journal.len();
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
        match Journal::open(&dir) {
            Err(Error::CorruptJournal(at)) => assert_eq!(at as u64, hole),
            other => panic!("expected corruption, got {:?}", other.map(|(_, f)| f.len())),
        }
        assert_eq!(fs::read(&path).unwrap(), before, "nothing changed");
    }

    #[test]
    fn a_hole_no_later_mark_covers_is_a_torn_tail() {
        let (_tmp, dir, hole) = damaged_middle(false);
        let (journal, frames) = Journal::open(&dir).unwrap();
        assert_eq!(frames.len(), 2, "the checkpoint and the first call");
        assert_eq!(journal.len(), hole);
        assert_eq!(fs::metadata(only_segment(&dir)).unwrap().len(), hole);
        assert!(dir.join("torn").exists());
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
        let (_, frames) = Journal::open(&dir).unwrap();
        assert!(frames.len() < 66);
        assert_eq!(segment::list(&dir).unwrap().len(), 1);
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
