//! History's port onto the journal for compaction: the checkpoint in a file
//! of its own beside the segments, written from history's state at its built
//! point, then the segments it covers let go (docs/trees.md, "Recording and
//! durability"; target git-model §3).
//!
//! ```text
//! journal/checkpoint       the newest published checkpoint: one frame,
//!                          salted and checksummed as a segment's are
//! journal/checkpoint.tmp   a checkpoint being written; open ignores it
//! journal/carry-<B:020>    live frames copied out of segments a checkpoint
//!                          at B lets go, flag `carried`, record numbers kept
//! ```
//!
//! Only history's compactor ([`super::super::history::compactor`]) calls this
//! module, on its own thread; the call path never imports it.
//!
//! Invariants:
//! - **C1.** [`JournalCompaction::publish`] takes a [`Sealed`] checkpoint,
//!   which only [`Sealed::new`] makes, from a [`Durable`] proof that every
//!   record at or below the checkpoint's `built` is durable.
//! - **C2, C2a.** A segment is covered when the next segment in the table
//!   starts at or before `built + 1`: the actor has rolled past it, and every
//!   record in it is at or below `built`. The newest segment is never
//!   covered. [`JournalCompaction::unlink_covered`] unlinks a covered segment
//!   only once a [`Published`] checkpoint covers it and every live frame in it
//!   has a durable copy the checkpoint names.
//! - **C3.** History forgets what a checkpoint covers only once
//!   [`Published`] reports it (`History::published`).
//! - **C4.** The checkpoint keeps, per pointer, the step at or below `built`
//!   that a record after it may still name (`step_heads`); the fold seeds
//!   each such pointer with that step, so a record folds the same on the old
//!   and the new checkpoint.
//! - **C5, C7.** A torn tail never removes the checkpoint, and the journal
//!   always opens: the salt, the redo start `built + 1` and "no record after
//!   `built`" come from the checkpoint file; a missing or wholly torn segment
//!   after `built` is judged as the first invalid frame (`Journal::open`).
//! - **C9.** No method holds the lock of Durability's state, of the segment
//!   table or of the relocations across I/O or a wait, and the volume's gate
//!   ([`CompactGate`]) is called with no lock held.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, mpsc};

use serde::{Deserialize, Serialize};

use super::durable::{Durability, DurableError, RecordNo};
use super::fold::{Folded, fold};
use super::segment::{FramePos, Segment, carry_name};
use super::{
    At, CARRIED, DropStats, Entry, Frame, HEADER, OpenRecord, Record, Tail, TreeLocals, crc_body,
    frame_at, frame_bytes, header_bytes, read_frame, read_header,
};
use crate::tree_actor::stages::{self, Unlocked, assert_unlocked};
use crate::tree_actor::{Error, Result};

/// The file name of the newest published checkpoint, in `journal/`.
pub(in crate::tree_actor) const CHECKPOINT: &str = "checkpoint";

/// The file name a checkpoint is written to before it is renamed into place.
pub(in crate::tree_actor) const CHECKPOINT_TMP: &str = "checkpoint.tmp";

/// What every record at or below `built` adds up to (C8). Each field's one
/// source at `built` (B):
///
/// | Field | Source at B |
/// |---|---|
/// | `salt` | the journal's, unchanged across compaction |
/// | `built` | History's built point, read in one hold with its pointers, objects and trees |
/// | `pointers` | History's built pointers at B, every value an oid |
/// | `objects` | History's built object numbers at B |
/// | `locals` | History's built trees at B: each tree's shape's locals |
/// | `upstreams` | the recovery fold over the records at or below B (`Folded::upstreams`) |
/// | `opens` | the fold: each live write-open with `by`, `anonymous` and `meta` (`Folded::opens`) |
/// | `page_saved` | the fold's last saving step per file with a live write-open (`Folded::saved_through`) |
/// | `page_writes` | the page writes the fold holds (`Folded::page_writes`), sorted, each once |
/// | `chunks` | the fold's chunk records no take, replace or save step has taken yet (`Folded::bytes`) |
/// | `step_heads` | every pointer, row chains included, the fold gives as `Target::Step(s)`: one a switch record at or below B names is an oid there. Its oid is `pointers[pointer]`. At most one per pointer (C3, C4) |
/// | `carried` | the carry files this checkpoint names live frames in, written only by [`Sealed::new`] |
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::tree_actor) struct Checkpoint {
    pub salt: u32,
    pub built: RecordNo,
    pub pointers: BTreeMap<String, String>,
    pub objects: BTreeMap<String, BTreeMap<u64, u64>>,
    pub locals: BTreeMap<String, TreeLocals>,
    pub upstreams: BTreeMap<String, String>,
    pub opens: Vec<OpenRecord>,
    /// Tree, object, saved_through.
    pub page_saved: Vec<(String, u64, u64)>,
    pub page_writes: Vec<RecordNo>,
    pub chunks: Vec<RecordNo>,
    /// Pointer to the step that last set it at or below B, while a record
    /// after B may still name that step: its oid is `pointers[pointer]`.
    pub step_heads: BTreeMap<String, RecordNo>,
    /// Each carry file by the `built` of the compaction that wrote it.
    pub carried: Vec<RecordNo>,
}

impl Checkpoint {
    /// The first record after it: `built + 1`.
    pub(in crate::tree_actor) fn next(&self) -> RecordNo {
        self.built.next()
    }

    /// The oldest step it keeps a head for.
    fn oldest_head(&self) -> Option<RecordNo> {
        self.step_heads.values().min().copied()
    }
}

/// The frames before a checkpoint it names as live: a call of an open it
/// lists, a page write it holds, a chunk no step has taken yet.
pub(super) struct Live<'a> {
    built: RecordNo,
    handles: BTreeSet<u64>,
    page_writes: &'a [RecordNo],
    chunks: &'a [RecordNo],
}

impl<'a> Live<'a> {
    pub(super) fn of(checkpoint: &'a Checkpoint) -> Self {
        Self {
            built: checkpoint.built,
            handles: checkpoint.opens.iter().map(|open| open.handle).collect(),
            page_writes: &checkpoint.page_writes,
            chunks: &checkpoint.chunks,
        }
    }

    /// Whether `record` is one of them.
    pub(super) fn names(&self, record: &Record) -> bool {
        record.seq <= self.built
            && match &record.entry {
                Entry::Call { handle, .. } => self.handles.contains(handle),
                Entry::PageWrite { .. } => self.page_writes.binary_search(&record.seq).is_ok(),
                Entry::Bytes => self.chunks.binary_search(&record.seq).is_ok(),
                _ => false,
            }
    }
}

/// The checkpoint file in `dir`, and the frame recovery folds first.
///
/// # Errors
///
/// [`Error::CorruptJournal`] (offset 0) for a missing or damaged file;
/// [`Error::Format`] for a frame that is not a checkpoint.
pub(super) fn read_checkpoint(dir: &Path) -> Result<(Checkpoint, Frame)> {
    let path = dir.join(CHECKPOINT);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Error::CorruptJournal(0)),
        Err(e) => return Err(e.into()),
    };
    if bytes.len() < 8 || &bytes[..4] != super::MAGIC {
        return Err(Error::CorruptJournal(0));
    }
    let salt = super::u32_at(&bytes, 4);
    let (header, head, tail) = read_frame(&bytes, salt).ok_or(Error::CorruptJournal(0))?;
    let record = Record::parse(header.record, head).map_err(|_| Error::CorruptJournal(0))?;
    let Entry::Checkpoint(checkpoint) = record.entry else {
        return Err(Error::Format);
    };
    if checkpoint.salt != salt {
        return Err(Error::CorruptJournal(0));
    }
    let file = Segment::open(RecordNo::ZERO, &path)?;
    let frame = (
        Record {
            seq: header.record,
            at_ns: record.at_ns,
            entry: Entry::Checkpoint(checkpoint.clone()),
        },
        Tail {
            pos: FramePos::new(file, header.record, (HEADER + head.len()) as u64),
            len: tail.len() as u64,
        },
    );
    Ok((checkpoint, frame))
}

/// `checkpoint` as the checkpoint file's one frame.
fn checkpoint_bytes(checkpoint: &Checkpoint) -> io::Result<Vec<u8>> {
    let record = Record {
        seq: RecordNo::ZERO,
        at_ns: crate::tree_actor::now_ns(),
        entry: Entry::Checkpoint(checkpoint.clone()),
    };
    let head = record.head().map_err(io::Error::other)?;
    frame_bytes(checkpoint.salt, record.seq, checkpoint.built, &head, &[])
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EFBIG))
}

/// Writes `checkpoint` to `checkpoint.tmp` in `dir` and fsyncs it.
fn write_tmp(dir: &Path, checkpoint: &Checkpoint) -> io::Result<()> {
    let bytes = checkpoint_bytes(checkpoint)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(dir.join(CHECKPOINT_TMP))?;
    file.write_all(&bytes)?;
    assert_unlocked(Unlocked::Fsync);
    file.sync_all()
}

/// Fsyncs the journal's directory, with no lock held.
fn sync_dir(dir: &Path) -> io::Result<()> {
    assert_unlocked(Unlocked::Fsync);
    crate::fsutil::sync_dir(dir)
}

/// A new journal's first checkpoint, written and in place in `dir`, each
/// fsynced: what `Journal::create` writes before the first segment.
///
/// # Errors
///
/// A failed write, rename or fsync.
pub(super) fn write_first(dir: &Path, checkpoint: &Checkpoint) -> io::Result<()> {
    write_tmp(dir, checkpoint)?;
    fs::rename(dir.join(CHECKPOINT_TMP), dir.join(CHECKPOINT))?;
    sync_dir(dir)
}

/// Every record at or below `through` is durable: what [`Sealed::new`]
/// takes as proof of C1. Only [`JournalCompaction::wait_durable`] makes one.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::tree_actor) struct Durable {
    through: RecordNo,
}

/// The bytes each segment holds at or below a fold's point, by the
/// segment's first record: what [`JournalCompaction::fold_through`] read of
/// it. A segment the journal rolled to since has none.
#[derive(Debug, Default)]
pub(in crate::tree_actor) struct Below(BTreeMap<RecordNo, u64>);

impl Below {
    /// The bytes at or below the point in the segments that are none of
    /// `covered`: in the segment the point falls in.
    fn outside(&self, covered: &[Arc<Segment>]) -> u64 {
        self.0
            .iter()
            .filter(|(first, _)| covered.iter().all(|segment| segment.first() != **first))
            .map(|(_, bytes)| bytes)
            .sum()
    }
}

/// Live frames copied out of the segments a checkpoint lets go, durable:
/// the carry files the checkpoint will name. Only
/// [`JournalCompaction::carry`] makes one.
#[derive(Debug)]
pub(in crate::tree_actor) struct Carried {
    /// Every carry file the checkpoint names: kept ones and the new one.
    files: Vec<RecordNo>,
    /// Bytes copied into the new one.
    bytes: u64,
    /// The covered segments whose live frames (if any) now have a copy.
    unlink: Vec<RecordNo>,
    /// The new carry file, if one was written.
    made: Option<Made>,
    /// The bytes at or below the checkpoint's built point that stay linked.
    kept: u64,
}

/// The carry file one compaction wrote, with where each record it holds was
/// carried before: what [`JournalCompaction::abandon`] undoes.
#[derive(Clone, Debug)]
pub(in crate::tree_actor) struct Made {
    carry: Arc<Segment>,
    earlier: Vec<(RecordNo, Option<super::segment::Place>)>,
}

impl Carried {
    /// Bytes copied into the new carry file.
    #[cfg(test)]
    pub(in crate::tree_actor) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The carry file this compaction wrote, for
    /// [`JournalCompaction::abandon`].
    pub(in crate::tree_actor) fn made(&self) -> Option<Made> {
        self.made.clone()
    }

    /// What the compaction keeps of what its checkpoint covers: the bytes
    /// at or below the built point that stay linked once its unlinks have
    /// run. The carry files the checkpoint names, the covered segments kept
    /// as they are, and the segment the built point falls in up to that
    /// point. Nothing appended after the built point is in it.
    pub(in crate::tree_actor) fn kept(&self) -> u64 {
        self.kept
    }
}

/// A checkpoint bound to its proofs: its `built` durable, its carry files
/// written. Only [`Sealed::new`] makes one, and only a `Sealed` is published.
#[derive(Debug)]
pub(in crate::tree_actor) struct Sealed {
    checkpoint: Checkpoint,
    unlink: Vec<RecordNo>,
    bytes: u64,
}

impl Sealed {
    /// `checkpoint` with `Checkpoint::carried` written from `carried`, the
    /// one source of that list.
    ///
    /// # Errors
    ///
    /// [`CompactionError::NotPublished`] when its `built` passes what
    /// `durable` proves, or for step heads recovery's fold refuses.
    pub(in crate::tree_actor) fn new(
        mut checkpoint: Checkpoint,
        durable: Durable,
        carried: Carried,
    ) -> std::result::Result<Self, CompactionError> {
        if checkpoint.built > durable.through {
            return Err(CompactionError::NotPublished(io::Error::other(format!(
                "a checkpoint at {} past the durable {}",
                checkpoint.built.get(),
                durable.through.get()
            ))));
        }
        // What recovery's fold refuses (`journal::fold`): a checkpoint it
        // cannot read never replaces one it can.
        let unreadable = checkpoint.step_heads.iter().find(|(name, step)| {
            !checkpoint.pointers.contains_key(*name) || **step > checkpoint.built
        });
        if let Some((name, step)) = unreadable {
            return Err(CompactionError::NotPublished(io::Error::other(format!(
                "a checkpoint at {} with the step head {name} at {}",
                checkpoint.built.get(),
                step.get()
            ))));
        }
        checkpoint.carried = carried.files;
        Ok(Self {
            checkpoint,
            unlink: carried.unlink,
            bytes: carried.bytes,
        })
    }

    /// The checkpoint it publishes.
    #[cfg(test)]
    pub(in crate::tree_actor) fn checkpoint(&self) -> &Checkpoint {
        &self.checkpoint
    }
}

/// A checkpoint at `built` renamed into place with its directory synced:
/// what [`JournalCompaction::unlink_covered`] takes as proof of C2. Made by
/// [`JournalCompaction::publish`], or by `Journal::open` for the checkpoint
/// it read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) struct Published {
    built: RecordNo,
    carried: Vec<RecordNo>,
    oldest_head: Option<RecordNo>,
    /// The covered segments it lets go: none for the one open read.
    unlink: Vec<RecordNo>,
    /// Bytes its compaction carried.
    carried_bytes: u64,
}

impl Published {
    /// The redo start's record before: every record at or below it is covered.
    pub(in crate::tree_actor) fn built(&self) -> RecordNo {
        self.built
    }

    /// The oldest step the published checkpoint keeps a head for
    /// (`Checkpoint::step_heads`): the built log and the builder keep what
    /// names it, so History never reports a checkpoint past the record
    /// before it (`History::checkpointed_through`).
    pub(in crate::tree_actor) fn oldest_head(&self) -> Option<RecordNo> {
        self.oldest_head
    }

    /// The checkpoint `Journal::open` read, after its directory fsync.
    pub(in crate::tree_actor::journal) fn of_open(checkpoint: &Checkpoint) -> Self {
        Self {
            built: checkpoint.built,
            carried: checkpoint.carried.clone(),
            oldest_head: checkpoint.oldest_head(),
            unlink: Vec::new(),
            carried_bytes: 0,
        }
    }
}

/// Why a compaction stopped.
#[derive(Debug, thiserror::Error)]
pub(in crate::tree_actor) enum CompactionError {
    /// The checkpoint's own temporary file or a carry file could not be
    /// written or fsynced, or the checkpoint not renamed: nothing a restart
    /// reads changed. Logged as `tree CompactionFailed`, retried at the next
    /// built event.
    #[error("journal compaction failed before its checkpoint: {0}")]
    NotPublished(io::Error),
    /// The checkpoint is published but a covered segment could not be
    /// unlinked: it stays, and the next compaction tries again.
    #[error("journal compaction failed after its checkpoint: {0}")]
    NotDropped(io::Error),
    /// An fsync of the journal's directory or of a segment failed, or the
    /// actor has stopped: recorded through `Durability::fsync_failed`, so
    /// the actor stops, as a leader's failed fsync stops it.
    #[error(transparent)]
    Stopped(#[from] DurableError),
}

/// A gap in one compaction, where the volume's gate parks the compactor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Gap {
    /// After the coverage check, before the wait for durability.
    Covered,
    /// `checkpoint.tmp` written and fsynced, before the rename.
    Written,
    /// After the rename, before the directory's fsync.
    Renamed,
    /// After the directory's fsync, before History counts the checkpoint.
    DirSynced,
    /// After History counts the checkpoint, before the covered segments are
    /// unlinked.
    BeforeUnlink,
}

impl Gap {
    /// Every gap, in the order one compaction passes them.
    pub const ALL: [Gap; 5] = [
        Gap::Covered,
        Gap::Written,
        Gap::Renamed,
        Gap::DirSynced,
        Gap::BeforeUnlink,
    ];

    /// Its line on the FIFO gate: `covered`, `written`, `renamed`,
    /// `dir_synced`, `before_unlink`.
    pub fn name(self) -> &'static str {
        match self {
            Gap::Covered => "covered",
            Gap::Written => "written",
            Gap::Renamed => "renamed",
            Gap::DirSynced => "dir_synced",
            Gap::BeforeUnlink => "before_unlink",
        }
    }

    /// What an `Err` returned at this gap fails: before the rename nothing
    /// is published; at the rename the directory's fsync fails and the actor
    /// stops; after it the checkpoint stands and nothing is unlinked.
    pub(in crate::tree_actor) fn fault(self, error: io::Error) -> CompactionError {
        match self {
            Gap::Covered | Gap::Written => CompactionError::NotPublished(error),
            Gap::Renamed => CompactionError::Stopped(DurableError::Io(
                error.raw_os_error().unwrap_or(libc::EIO),
            )),
            Gap::DirSynced | Gap::BeforeUnlink => CompactionError::NotDropped(error),
        }
    }
}

/// Parks a volume's compactor at each gap of every compaction, and may fail
/// the step after a gap. Called on the compactor thread with no lock held.
pub trait CompactGate: Send + Sync {
    /// Returns when the compactor may go on past `gap`; `Err(e)` fails the
    /// compaction there, as [`Gap`]'s fault says.
    ///
    /// # Errors
    ///
    /// The failure to inject at `gap`.
    fn at(&self, gap: Gap) -> io::Result<()>;
}

/// The in-process gate: each gap reached is sent on a channel, and the
/// compactor waits for the holder's verdict. A gap nobody receives, or
/// reached once the verdict sender is dropped, passes at once, so a pair
/// with both ends dropped is a gate that never parks.
pub struct ChannelGate {
    reached: Mutex<mpsc::Sender<Gap>>,
    verdicts: Mutex<mpsc::Receiver<io::Result<()>>>,
}

impl ChannelGate {
    /// The gate, the receiver of each gap it reaches, and the sender of
    /// verdicts.
    #[must_use]
    pub fn pair() -> (Arc<Self>, mpsc::Receiver<Gap>, mpsc::Sender<io::Result<()>>) {
        let (reached, gaps) = mpsc::channel();
        let (verdict, verdicts) = mpsc::channel();
        (
            Arc::new(Self {
                reached: Mutex::new(reached),
                verdicts: Mutex::new(verdicts),
            }),
            gaps,
            verdict,
        )
    }
}

impl CompactGate for ChannelGate {
    fn at(&self, gap: Gap) -> io::Result<()> {
        let sent = self
            .reached
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(gap);
        if sent.is_err() {
            return Ok(());
        }
        self.verdicts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recv()
            .unwrap_or(Ok(()))
    }
}

/// A frame found in a file: where it starts and its header.
struct Found {
    at: u64,
    header: super::Header,
    record: Record,
}

/// Every frame of a complete file (a covered segment, a carry file), heads
/// parsed, tails unread.
fn frames_of(file: &Segment, salt: u32) -> io::Result<Vec<Found>> {
    let length = file.len();
    let mut found = Vec::new();
    let mut at = 0;
    while at < length {
        let mut bytes = [0; HEADER];
        file.read_exact_at(&mut bytes, at)?;
        let header = read_header(&bytes, salt)
            .ok_or_else(|| io::Error::other("invalid frame in an open journal"))?;
        let mut head = vec![0; header.head];
        file.read_exact_at(&mut head, at + HEADER as u64)?;
        let record = Record::parse(header.record, &head).map_err(io::Error::other)?;
        let len = header.frame_len();
        found.push(Found { at, header, record });
        at += len;
    }
    Ok(found)
}

/// History's port onto one volume's journal: the only writer of the
/// checkpoint file and the carry files, and the only code that unlinks a
/// segment. Used by the compactor thread alone. Holds no actor state.
pub(in crate::tree_actor) struct JournalCompaction {
    dir: PathBuf,
    durability: Arc<Durability>,
    gate: Option<Arc<dyn CompactGate>>,
}

impl JournalCompaction {
    /// The port onto the journal in `dir`, whose group commit and segment
    /// table are `durability`, parked at each gap by `gate` when it has one.
    pub(in crate::tree_actor) fn new(
        dir: PathBuf,
        durability: Arc<Durability>,
        gate: Option<Arc<dyn CompactGate>>,
    ) -> Self {
        Self {
            dir,
            durability,
            gate,
        }
    }

    /// The volume's gate at `gap`: `Ok` at once with no gate. An `Err` is
    /// classed by the gap ([`Gap::fault`]); one that stops the actor is
    /// recorded through `Durability::fsync_failed` first.
    ///
    /// # Errors
    ///
    /// As [`Gap::fault`].
    pub(in crate::tree_actor) fn gate(&self, gap: Gap) -> std::result::Result<(), CompactionError> {
        let Some(gate) = &self.gate else {
            return Ok(());
        };
        gate.at(gap).map_err(|e| {
            let fault = gap.fault(e);
            if let CompactionError::Stopped(DurableError::Io(errno)) = &fault {
                self.durability
                    .fsync_failed(&io::Error::from_raw_os_error(*errno));
            }
            fault
        })
    }

    /// Whether the volume has a gate: the compactor then compacts at every
    /// built event.
    pub(in crate::tree_actor) fn gated(&self) -> bool {
        self.gate.is_some()
    }

    /// Every record at or below `through` made durable (group commit).
    ///
    /// # Errors
    ///
    /// [`DurableError`] as `Durability::make_durable` gives it.
    pub(in crate::tree_actor) fn wait_durable(
        &self,
        through: RecordNo,
    ) -> std::result::Result<Durable, DurableError> {
        self.durability.make_durable(through)?;
        Ok(Durable { through })
    }

    /// The recovery fold (`journal::fold`) over the published checkpoint,
    /// the live frames it names and every record after it at or below
    /// `durable`'s point, read from the segments with no lock held. One
    /// definition of each transition: the fold recovery runs. With it, the
    /// bytes it read of each segment ([`Below`]).
    ///
    /// # Errors
    ///
    /// [`CompactionError::NotPublished`] for a failed read or a record the
    /// fold refuses.
    pub(in crate::tree_actor) fn fold_through(
        &self,
        durable: &Durable,
    ) -> std::result::Result<(Folded, Below), CompactionError> {
        let not_published = |e: Error| CompactionError::NotPublished(io::Error::other(e));
        let (previous, frame) = read_checkpoint(&self.dir).map_err(not_published)?;
        let live = Live::of(&previous);
        let salt = previous.salt;
        let mut before: BTreeMap<RecordNo, Frame> = BTreeMap::new();
        let named: BTreeSet<String> = previous
            .carried
            .iter()
            .map(|built| carry_name(*built))
            .collect();
        let mut files: Vec<(Arc<Segment>, RecordNo, bool)> = self
            .durability
            .carries()
            .into_iter()
            .filter(|carry| {
                carry
                    .path()
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| named.contains(n))
            })
            .map(|carry| (carry, previous.built, false))
            .collect();
        files.extend(
            self.durability
                .table()
                .into_iter()
                .map(|segment| (segment, durable.through, true)),
        );
        let mut after = Vec::new();
        let mut below = Below::default();
        for (file, through, segment) in files {
            let mut at = 0;
            let later = loop {
                match frame_at(&file, at, salt, through).map_err(|e| not_published(e.into()))? {
                    At::Frame(found, len) => {
                        at += len;
                        let (record, _) = &*found;
                        if record.seq <= previous.built {
                            if live.names(record) {
                                before.entry(record.seq).or_insert(*found);
                            }
                        } else {
                            after.push(*found);
                        }
                    }
                    At::Later => break true,
                    At::End => break false,
                }
            };
            if segment {
                below.0.insert(file.first(), at);
            }
            if later {
                break;
            }
        }
        let frames: Vec<Frame> = std::iter::once(frame)
            .chain(before.into_values())
            .chain(after)
            .collect();
        let folded = fold(&frames).map_err(not_published)?;
        Ok((folded, below))
    }

    /// The covered segments: each before the newest whose next segment
    /// starts at or before `built + 1` (C2, C2a).
    fn covered(&self, built: RecordNo) -> Vec<Arc<Segment>> {
        let table = self.durability.table();
        table
            .windows(2)
            .filter(|pair| pair[1].first() <= built.next())
            .map(|pair| Arc::clone(&pair[0]))
            .collect()
    }

    /// Copies the live frames (a call of an open `checkpoint.opens` lists, a
    /// page write in `checkpoint.page_writes`, a chunk in `checkpoint.chunks`)
    /// out of every covered segment and carry file whose dead bytes outweigh
    /// them into `carry-<built>`, fsynced with the directory, and records each
    /// copy in the relocations. A file whose live frames outweigh its dead
    /// bytes is kept as it is; one with none is let go without a copy.
    /// `below` is the fold's reading of each segment, for
    /// [`Carried::kept`].
    ///
    /// A compaction that ends before its checkpoint is renamed hands
    /// [`Carried::made`] to [`JournalCompaction::abandon`].
    ///
    /// # Errors
    ///
    /// [`CompactionError::NotPublished`]: the new carry file is removed.
    pub(in crate::tree_actor) fn carry(
        &self,
        checkpoint: &Checkpoint,
        below: &Below,
    ) -> std::result::Result<Carried, CompactionError> {
        let built = checkpoint.built;
        let not_published = CompactionError::NotPublished;
        // One a failed compaction at this point left is no checkpoint's.
        let stale = carry_name(built);
        for carry in self.durability.carries() {
            if carry.path().file_name().and_then(|n| n.to_str()) == Some(stale.as_str())
                && self.durability.remove_carry(&carry)
            {
                self.durability.relocations().dropped(&carry);
                Durability::unlink(&carry).map_err(not_published)?;
            }
        }
        let live = Live::of(checkpoint);
        let mut files = Vec::new();
        let mut copies: Vec<(Arc<Segment>, super::Header, u64)> = Vec::new();
        let mut unlink = Vec::new();
        let mut kept = 0;
        let mut decide = |file: &Arc<Segment>| -> io::Result<bool> {
            let frames = frames_of(file, checkpoint.salt)?;
            let length = file.len();
            let kept: Vec<&Found> = frames.iter().filter(|f| live.names(&f.record)).collect();
            let kept_bytes: u64 = kept.iter().map(|f| f.header.frame_len()).sum();
            if !kept.is_empty() && length - kept_bytes <= kept_bytes {
                return Ok(true);
            }
            copies.extend(kept.iter().map(|f| (Arc::clone(file), f.header, f.at)));
            Ok(false)
        };
        for carry in self.durability.carries() {
            if decide(&carry).map_err(not_published)? {
                let built = super::segment::carry_built(carry.path())
                    .ok_or_else(|| not_published(io::Error::other("a carry file's name")))?;
                files.push(built);
                kept += carry.len();
            }
        }
        let covered = self.covered(built);
        for segment in &covered {
            if decide(segment).map_err(not_published)? {
                kept += segment.len();
            } else {
                unlink.push(segment.first());
            }
        }
        kept += below.outside(&covered);
        let mut bytes = 0;
        let mut made = None;
        if !copies.is_empty() {
            let carry = Segment::create_carry(&self.dir, built).map_err(not_published)?;
            let written = self.copy(&carry, &copies, checkpoint.salt);
            match written {
                Ok((moved, length)) => {
                    bytes = length;
                    kept += length;
                    carry.set_len(length);
                    self.durability.add_carry(Arc::clone(&carry), length);
                    // The copies stand: holders may move to them.
                    let relocations = self.durability.relocations();
                    let earlier = moved
                        .into_iter()
                        .map(|(record, tail)| (record, relocations.moved(record, &carry, tail)))
                        .collect();
                    made = Some(Made { carry, earlier });
                    files.push(built);
                }
                Err(e) => {
                    let _ = Durability::unlink(&carry);
                    return Err(not_published(e));
                }
            }
        }
        files.sort_unstable();
        Ok(Carried {
            files,
            bytes,
            unlink,
            made,
            kept,
        })
    }

    /// Lets go of the carry file a compaction made, when it ends before its
    /// checkpoint is renamed: no checkpoint names the file, so it leaves
    /// Durability's list, each record it holds is where it was before, and
    /// the file is unlinked. A compaction that fails this way again and
    /// again leaves nothing behind. A holder that moved to the file keeps
    /// reading it until it moves again.
    pub(in crate::tree_actor) fn abandon(&self, made: Option<Made>) {
        let Some(Made { carry, earlier }) = made else {
            return;
        };
        if !self.durability.remove_carry(&carry) {
            return;
        }
        for (record, earlier) in earlier {
            self.durability.relocations().put_back(record, earlier);
        }
        if let Err(e) = Durability::unlink(&carry) {
            eprintln!("tree CompactionFailed: {}: {e}", carry.path().display());
        }
    }

    /// Writes each frame of `copies` into `carry` (flag `carried`, record
    /// number kept), fsyncs it and the directory: where each record's tail
    /// now is, and the bytes written.
    fn copy(
        &self,
        carry: &Segment,
        copies: &[(Arc<Segment>, super::Header, u64)],
        salt: u32,
    ) -> io::Result<(Vec<(RecordNo, u64)>, u64)> {
        let mut end = 0;
        let mut moved = Vec::with_capacity(copies.len());
        let mut seen = BTreeSet::new();
        for (from, header, at) in copies {
            // A frame and its copy both linked (after `NotDropped`): one copy.
            if !seen.insert(header.record) {
                continue;
            }
            let mut body = vec![0; header.head + header.tail];
            from.read_exact_at(&mut body, at + HEADER as u64)?;
            // Rewriting computes a new header checksum: never launder a bad frame.
            if crc_body(&body[..header.head], &body[header.head..]) != header.crc {
                return Err(io::Error::other("invalid frame in an open journal"));
            }
            let copy = header_bytes(
                salt,
                header.record,
                self.durability.mark(),
                header.flags | CARRIED,
                header.head as u32,
                header.tail as u32,
                header.crc,
            );
            carry.file().write_all_at(&copy, end)?;
            carry.file().write_all_at(&body, end + HEADER as u64)?;
            moved.push((header.record, end + (HEADER + header.head) as u64));
            end += header.frame_len();
            stages::copied(header.tail as u64);
        }
        carry.sync()?;
        sync_dir(&self.dir)?;
        Ok((moved, end))
    }

    /// Writes the sealed checkpoint to `checkpoint.tmp`, fsyncs it, renames
    /// it to `checkpoint` and fsyncs the directory, with the gate at
    /// [`Gap::Written`] and [`Gap::Renamed`].
    ///
    /// # Errors
    ///
    /// [`CompactionError::NotPublished`] while the old checkpoint still
    /// stands; [`CompactionError::Stopped`] for a failed fsync of the
    /// directory, recorded through `Durability::fsync_failed`.
    pub(in crate::tree_actor) fn publish(
        &self,
        sealed: &Sealed,
    ) -> std::result::Result<Published, CompactionError> {
        let checkpoint = &sealed.checkpoint;
        write_tmp(&self.dir, checkpoint).map_err(CompactionError::NotPublished)?;
        self.gate(Gap::Written)?;
        fs::rename(self.dir.join(CHECKPOINT_TMP), self.dir.join(CHECKPOINT))
            .map_err(CompactionError::NotPublished)?;
        self.gate(Gap::Renamed)?;
        if let Err(e) = sync_dir(&self.dir) {
            self.durability.fsync_failed(&e);
            return Err(DurableError::Io(e.raw_os_error().unwrap_or(libc::EIO)).into());
        }
        Ok(Published {
            built: checkpoint.built,
            carried: checkpoint.carried.clone(),
            oldest_head: checkpoint.oldest_head(),
            unlink: sealed.unlink.clone(),
            carried_bytes: sealed.bytes,
        })
    }

    /// Unlinks every covered segment `published` lets go, and every carry
    /// file it does not name, then fsyncs the directory. Each leaves the
    /// segment table under Durability's lock in O(segments), once its unlink
    /// has run with no lock held. A reader holding an unlinked file keeps reading
    /// it, and its bytes count in [`JournalCompaction::len`] until the last
    /// holder lets go.
    ///
    /// # Errors
    ///
    /// [`CompactionError::NotDropped`]: what was not unlinked stays in the
    /// table, and the next compaction tries it again;
    /// [`CompactionError::Stopped`] for a failed fsync of the directory.
    pub(in crate::tree_actor) fn unlink_covered(
        &self,
        published: &Published,
    ) -> std::result::Result<DropStats, CompactionError> {
        let mut stats = DropStats {
            segments: 0,
            carried_bytes: published.carried_bytes,
        };
        let mut unlinked = false;
        // The unlink first: a file whose unlink fails is still in the
        // table, where the next compaction finds it.
        let table = self.durability.table();
        for first in &published.unlink {
            let Some(segment) = table.iter().find(|s| s.first() == *first) else {
                continue;
            };
            Durability::unlink(segment).map_err(CompactionError::NotDropped)?;
            unlinked = true;
            if self.durability.remove_segment(*first).is_some() {
                self.durability.relocations().dropped(segment);
                stats.segments += 1;
            }
        }
        for carry in self.durability.carries() {
            let named = super::segment::carry_built(carry.path())
                .is_some_and(|built| published.carried.contains(&built));
            if named {
                continue;
            }
            Durability::unlink(&carry).map_err(CompactionError::NotDropped)?;
            unlinked = true;
            if self.durability.remove_carry(&carry) {
                self.durability.relocations().dropped(&carry);
            }
        }
        if unlinked && let Err(e) = sync_dir(&self.dir) {
            self.durability.fsync_failed(&e);
            return Err(DurableError::Io(e.raw_os_error().unwrap_or(libc::EIO)).into());
        }
        Ok(stats)
    }

    /// The linked files' length, segments and carry files: what compaction
    /// is due by (`Durability::linked`). An unlinked file a reader still
    /// holds (C6) counts in `Durability::len`, the journal's figure, and
    /// not here.
    pub(in crate::tree_actor) fn len(&self) -> u64 {
        self.durability.linked()
    }
}

/// A frame file's handle for tests: the checkpoint file in `dir`.
#[cfg(test)]
pub(in crate::tree_actor) fn checkpoint_in(dir: &Path) -> Checkpoint {
    read_checkpoint(dir)
        .unwrap_or_else(|e| panic!("checkpoint in {}: {e}", dir.display()))
        .0
}

/// Rewrites the checkpoint file in `dir` with `checkpoint`: a test's edit.
#[cfg(test)]
pub(in crate::tree_actor) fn rewrite_checkpoint(dir: &Path, checkpoint: &Checkpoint) {
    write_first(dir, checkpoint).unwrap_or_else(|e| panic!("{e}"));
}
