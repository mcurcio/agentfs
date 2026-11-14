//! The tree actor's journal: the commit point for byte calls and pointer steps
//! (target git-model §3, §12).
//!
//! ```text
//! frame  = header head tail
//! header = "AFJ3" | u32le salt | u64le record | u64le mark | u32le flags
//!          | u32le head_len | u32le tail_len | u32le crc32(head + tail)
//!          | u32le crc32(first 40 header bytes)                    (44 bytes)
//! head   = JSON of the record's time and entry
//! tail   = the raw bytes of the record's calls or runs, in order
//! mark   = the durable mark when the frame was appended
//! flags  = 0
//! ```
//!
//! A frame is appended with one `write`, and no fsync: a record is in the
//! kernel when the call returns, and durable once a FLUSH, an fsync or an
//! FSYNCDIR has made it so ([`durable`]). The header is the record number's
//! one home. Each write-open and step names its writer with the pod context it
//! ran in (`WriterAt`), so a recovery close commits as the writer that opened.
//!
//! The salt is random per journal and kept through compaction, so a workload's
//! bytes inside a tail cannot pass as a frame. At an invalid frame after the
//! last valid record `r`, a later valid frame whose mark passes `r` shows that
//! a record in the hole was durable: that is corruption, which an operator
//! decides about (docs/runbooks/recording-failures.md). Otherwise the hole is
//! a torn tail, records no fsync had covered: it is saved aside and cut off.

mod commit_point;
pub(super) mod durable;
mod fold;

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentfs_contracts::pod::WriterAt;
use serde::{Deserialize, Serialize};

use super::stages::{Timer, Which};
use super::{Error, Result};
pub(super) use commit_point::compact_at;
use durable::{Durability, RecordNo};
pub(super) use fold::{FoldedOpen, LiveOpen, fold};

const MAGIC: &[u8; 4] = b"AFJ3";
const HEADER: usize = 44;
/// The header bytes its own checksum covers.
const HEADER_SUMMED: usize = 40;
pub(super) const MAX_FRAME: usize = 96 << 20;

/// One frame as a record and its tail bytes.
pub(super) type Frame = (Record, Vec<u8>);

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
    /// The first frame only. `page_saved`: tree, object, saved_through.
    Checkpoint {
        next_seq: u64,
        pointers: BTreeMap<String, String>,
        upstreams: BTreeMap<String, String>,
        opens: Vec<OpenRecord>,
        page_saved: Vec<(String, u64, u64)>,
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
    /// Every pointer change one step makes, and what else it decides.
    Step {
        by: WriterAt,
        closes: Option<u64>,
        /// Boxed: the step is the largest record, and most steps open nothing.
        opened: Option<Box<OpenRecord>>,
        linked: Option<u64>,
        fork: Option<(String, String)>,
        pointers: Vec<PointerChange>,
    },
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

/// Hex object ids. `None` is absent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PointerChange {
    pub name: String,
    pub new: Option<String>,
    pub old: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum CompactError {
    /// The journal is untouched.
    #[error("journal compaction failed before the rename: {0}")]
    Before(io::Error),
    /// The rename ran: the name holds the old or the new file, both complete,
    /// but the open file may not be the named one.
    #[error("journal compaction failed after the rename: {0}")]
    After(io::Error),
}

pub(super) struct Journal {
    file: Arc<File>,
    path: PathBuf,
    length: u64,
    salt: u32,
    unknown: bool,
    durability: Arc<Durability>,
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
    head: usize,
    tail: usize,
    crc: u32,
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
    let mut bytes = Vec::with_capacity(HEADER + head.len() + tail.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&salt.to_le_bytes());
    bytes.extend_from_slice(&record.get().to_le_bytes());
    bytes.extend_from_slice(&mark.get().to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&u32::try_from(head.len()).ok()?.to_le_bytes());
    bytes.extend_from_slice(&u32::try_from(tail.len()).ok()?.to_le_bytes());
    bytes.extend_from_slice(&crc_body(head, tail).to_le_bytes());
    let header_crc = crc32fast::hash(&bytes[..HEADER_SUMMED]);
    bytes.extend_from_slice(&header_crc.to_le_bytes());
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

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("journal path has no parent"))?;
    File::open(parent)?.sync_all()
}

fn draw_salt() -> io::Result<u32> {
    let mut salt = [0; 4];
    File::open("/dev/urandom")?.read_exact(&mut salt)?;
    Ok(u32::from_le_bytes(salt))
}

fn temporary(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

/// The last record a checkpoint covers: every record before its `next_seq`.
fn covered_by(checkpoint: &Record) -> RecordNo {
    match &checkpoint.entry {
        Entry::Checkpoint { next_seq, .. } => RecordNo::new(next_seq.saturating_sub(1)),
        _ => checkpoint.seq,
    }
}

/// Writes one checkpoint and `retained` to the temporary name and syncs them.
/// The file is synced whole, so every frame carries the mark of what it covers.
fn write_temporary(
    path: &Path,
    salt: u32,
    checkpoint: &Record,
    retained: &[Frame],
) -> io::Result<PathBuf> {
    let temp = temporary(path);
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temp)?;
    let mark = covered_by(checkpoint);
    for (record, tail) in std::iter::once((checkpoint, &[][..]))
        .chain(retained.iter().map(|(r, t)| (r, t.as_slice())))
    {
        let head = record.head()?;
        let bytes = frame_bytes(salt, record.seq, mark, &head, tail)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EFBIG))?;
        file.write_all(&bytes)?;
    }
    file.sync_all()?;
    Ok(temp)
}

impl Journal {
    /// A new journal holding `first`, atomically; draws the salt.
    pub fn create(path: &Path, first: &Record) -> Result<()> {
        let temp = write_temporary(path, draw_salt()?, first, &[])?;
        fs::rename(temp, path)?;
        sync_parent(path)?;
        Ok(())
    }

    /// The journal and every valid frame, durable or not (a killed agentfs
    /// leaves its appends in the kernel). At a torn tail: saves its bytes to
    /// `calls.wal.torn`, logs `tree JournalTail: <n> bytes at <offset>`,
    /// truncates and syncs. The durable mark starts at the highest mark a frame
    /// carries; the caller makes the rest durable before serving.
    ///
    /// # Errors
    ///
    /// [`Error::CorruptJournal`] for a missing or frameless file (offset 0), or
    /// at an invalid frame that a later valid frame's mark shows held a durable
    /// record. Nothing is changed then.
    pub fn open(path: &Path) -> Result<(Self, Vec<Frame>)> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(Error::CorruptJournal(0));
            }
            Err(e) => return Err(e.into()),
        };
        if bytes.len() < HEADER {
            return Err(Error::CorruptJournal(0));
        }
        let salt = u32_at(&bytes, 4);
        let mut frames: Vec<Frame> = Vec::new();
        let mut offset = 0;
        // The last frame's number, and the last record the valid frames hold.
        let mut last = RecordNo::ZERO;
        let mut covered = RecordNo::ZERO;
        let mut mark = RecordNo::ZERO;
        let mut torn = false;
        while offset < bytes.len() {
            let Some((header, head, tail)) = read_frame(&bytes[offset..], salt) else {
                if offset == 0 || durable_past(&bytes[offset + 1..], salt, covered) {
                    return Err(Error::CorruptJournal(offset));
                }
                torn = true;
                break;
            };
            let record =
                Record::parse(header.record, head).map_err(|_| Error::CorruptJournal(offset))?;
            let is_checkpoint = matches!(record.entry, Entry::Checkpoint { .. });
            if is_checkpoint != (offset == 0) || (offset != 0 && record.seq <= last) {
                return Err(Error::CorruptJournal(offset));
            }
            last = record.seq;
            covered = covered.max(if is_checkpoint {
                covered_by(&record)
            } else {
                record.seq
            });
            mark = mark.max(header.mark);
            frames.push((record, tail.to_vec()));
            offset += HEADER + header.head + header.tail;
        }
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        if torn {
            let rest = &bytes[offset..];
            let mut saved_name = path.as_os_str().to_owned();
            saved_name.push(".torn");
            let mut saved = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(PathBuf::from(saved_name))?;
            saved.write_all(rest)?;
            saved.sync_all()?;
            sync_parent(path)?;
            eprintln!("tree JournalTail: {} bytes at {offset}", rest.len());
            file.set_len(offset as u64)?;
            file.sync_all()?;
        }
        let file = Arc::new(file);
        let durability = Durability::new(Arc::clone(&file), covered, mark);
        Ok((
            Self {
                file,
                path: path.to_owned(),
                length: offset as u64,
                salt,
                unknown: false,
                durability,
            },
            frames,
        ))
    }

    /// Appends one frame: one buffer, one `write`, no fsync. When this returns
    /// Ok the record is in the kernel and stands against a crash of agentfs;
    /// [`Durability`] makes it stand against a power cut.
    ///
    /// # Errors
    ///
    /// A failed write whose rollback worked: the error, file as before. A
    /// failed rollback: the error, and [`Journal::unknown`] turns true.
    pub fn append(&mut self, record: &Record, tail: &[u8]) -> Result<()> {
        if self.unknown {
            return Err(Error::Stopped);
        }
        let _timer = Timer::start(Which::JournalAppend);
        let head = record.head()?;
        let bytes = frame_bytes(self.salt, record.seq, self.durability.mark(), &head, tail)
            .ok_or(Error::TooLarge)?;
        let at = self.length;
        let written = (|| -> io::Result<()> {
            crate::failpoints::hit("tree.wal_before_header")?;
            self.file.write_all_at(&bytes, at)?;
            crate::failpoints::hit("tree.wal_after_payload")
        })();
        if let Err(e) = written {
            if self.file.set_len(at).is_err() {
                self.unknown = true;
            }
            return Err(e.into());
        }
        self.length = at + bytes.len() as u64;
        self.durability.appended_to(record.seq);
        Ok(())
    }

    /// True once this file cannot be trusted: reopen to recover.
    pub fn unknown(&self) -> bool {
        self.unknown
    }

    /// The journal's group commit: the only way its records become durable.
    pub fn durability(&self) -> &Arc<Durability> {
        &self.durability
    }

    /// The frames after the first that `keep` accepts, read back unchanged, so
    /// memory holds only what a compaction retains.
    pub fn retained(&self, keep: impl Fn(&Record) -> bool) -> io::Result<Vec<Frame>> {
        let mut kept = Vec::new();
        let mut at = 0;
        let mut first = true;
        while at < self.length {
            let mut bytes = [0; HEADER];
            self.file.read_exact_at(&mut bytes, at)?;
            let header = read_header(&bytes, self.salt)
                .ok_or_else(|| io::Error::other("invalid frame in an open journal"))?;
            let mut head = vec![0; header.head];
            self.file.read_exact_at(&mut head, at + HEADER as u64)?;
            let record = Record::parse(header.record, &head)?;
            if !first && keep(&record) {
                let mut tail = vec![0; header.tail];
                self.file
                    .read_exact_at(&mut tail, at + (HEADER + header.head) as u64)?;
                // Rewriting computes a new checksum: never launder a bad frame.
                if crc_body(&head, &tail) != header.crc {
                    return Err(io::Error::other("invalid frame in an open journal"));
                }
                kept.push((record, tail));
            }
            first = false;
            at += (HEADER + header.head + header.tail) as u64;
        }
        Ok(kept)
    }

    /// Replaces the journal with `checkpoint` then `retained`, keeping the salt.
    /// The new file is synced whole: the durable mark rises to what it covers.
    ///
    /// # Errors
    ///
    /// [`CompactError::Before`]: the journal is untouched. [`CompactError::After`]:
    /// the rename ran; the name holds the old or the new file, both complete.
    pub fn compact(
        &mut self,
        checkpoint: &Record,
        retained: &[Frame],
    ) -> std::result::Result<(), CompactError> {
        (|| -> io::Result<()> {
            let temp = write_temporary(&self.path, self.salt, checkpoint, retained)?;
            crate::failpoints::hit("tree.compact_before_rename")?;
            fs::rename(&temp, &self.path)
        })()
        .map_err(CompactError::Before)?;
        (|| -> io::Result<()> {
            sync_parent(&self.path)?;
            crate::failpoints::hit("tree.compact_after_rename")?;
            let file = Arc::new(OpenOptions::new().read(true).write(true).open(&self.path)?);
            self.length = file.metadata()?.len();
            self.durability
                .replace_file(Arc::clone(&file), covered_by(checkpoint));
            self.file = file;
            Ok(())
        })()
        .map_err(CompactError::After)
    }

    pub fn len(&self) -> u64 {
        self.length
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint(next_seq: u64) -> Record {
        Record {
            seq: RecordNo::ZERO,
            at_ns: 1,
            entry: Entry::Checkpoint {
                next_seq,
                pointers: BTreeMap::new(),
                upstreams: BTreeMap::new(),
                opens: Vec::new(),
                page_saved: Vec::new(),
            },
        }
    }

    fn call(seq: u64, bytes: &[u8]) -> (Record, Vec<u8>) {
        (
            Record {
                seq: RecordNo::new(seq),
                at_ns: 2,
                entry: Entry::Call {
                    handle: 1,
                    call: CallHead::Append {
                        len: bytes.len() as u64,
                    },
                },
            },
            bytes.to_vec(),
        )
    }

    fn journal() -> (tempfile::TempDir, PathBuf, Journal) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calls.wal");
        Journal::create(&path, &checkpoint(1)).unwrap();
        let (journal, frames) = Journal::open(&path).unwrap();
        assert_eq!(frames.len(), 1);
        (dir, path, journal)
    }

    #[test]
    fn frames_round_trip_and_compaction_keeps_the_salt_and_tails() {
        let (_dir, path, mut journal) = journal();
        let first = call(1, b"one");
        let second = call(2, b"\0two\xff");
        journal.append(&first.0, &first.1).unwrap();
        journal.append(&second.0, &second.1).unwrap();
        assert_eq!(journal.len(), fs::metadata(&path).unwrap().len());
        let salt = journal.salt;
        let kept = journal
            .retained(|record| record.seq == RecordNo::new(2))
            .unwrap();
        assert_eq!(kept, vec![second.clone()]);
        journal.compact(&checkpoint(3), &kept).unwrap();
        assert_eq!(journal.len(), fs::metadata(&path).unwrap().len());
        let third = call(3, b"three");
        journal.append(&third.0, &third.1).unwrap();
        drop(journal);
        let (journal, frames) = Journal::open(&path).unwrap();
        assert_eq!(journal.salt, salt);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[1], second);
        assert_eq!(frames[2], third);
    }

    #[test]
    fn a_frame_over_the_limit_is_refused_before_the_file_changes() {
        let (_dir, path, mut journal) = journal();
        let before = fs::read(&path).unwrap();
        let huge = vec![0; MAX_FRAME];
        let record = call(1, b"").0;
        assert!(matches!(
            journal.append(&record, &huge),
            Err(Error::TooLarge)
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!journal.unknown());
    }

    #[test]
    fn a_missing_or_frameless_file_is_corruption_at_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calls.wal");
        assert!(matches!(
            Journal::open(&path),
            Err(Error::CorruptJournal(0))
        ));
        fs::write(&path, b"").unwrap();
        assert!(matches!(
            Journal::open(&path),
            Err(Error::CorruptJournal(0))
        ));
        fs::write(&path, vec![7; 100]).unwrap();
        assert!(matches!(
            Journal::open(&path),
            Err(Error::CorruptJournal(0))
        ));
        assert_eq!(fs::read(&path).unwrap(), vec![7; 100]);
    }

    #[test]
    fn a_torn_tail_is_saved_and_truncated() {
        let (dir, path, mut journal) = journal();
        let first = call(1, b"one");
        journal.append(&first.0, &first.1).unwrap();
        let length = journal.len();
        drop(journal);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"AFJ3 torn").unwrap();
        drop(file);
        let (journal, frames) = Journal::open(&path).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(journal.len(), length);
        assert_eq!(fs::metadata(&path).unwrap().len(), length);
        assert_eq!(
            fs::read(dir.path().join("calls.wal.torn")).unwrap(),
            b"AFJ3 torn"
        );
    }

    /// Three calls; the second's tail damaged on disk. `durable` makes the
    /// first two durable before the third is appended, so its mark covers them.
    fn damaged_middle(durable: bool) -> (tempfile::TempDir, PathBuf, u64) {
        let (dir, path, mut journal) = journal();
        let first = call(1, b"one");
        journal.append(&first.0, &first.1).unwrap();
        let hole = journal.len();
        let second = call(2, b"two");
        journal.append(&second.0, &second.1).unwrap();
        if durable {
            journal.durability().make_durable(RecordNo::new(2)).unwrap();
        }
        let third = call(3, b"three");
        journal.append(&third.0, &third.1).unwrap();
        drop(journal);
        let mut bytes = fs::read(&path).unwrap();
        // The first byte of the second frame's head: its body checksum fails.
        bytes[usize::try_from(hole).unwrap() + HEADER] ^= 0xff;
        fs::write(&path, bytes).unwrap();
        (dir, path, hole)
    }

    #[test]
    fn a_hole_a_later_mark_covers_is_corruption() {
        let (_dir, path, hole) = damaged_middle(true);
        let before = fs::read(&path).unwrap();
        match Journal::open(&path) {
            Err(Error::CorruptJournal(at)) => assert_eq!(at as u64, hole),
            other => panic!("expected corruption, got {:?}", other.map(|(_, f)| f.len())),
        }
        assert_eq!(fs::read(&path).unwrap(), before, "nothing changed");
    }

    #[test]
    fn a_hole_no_later_mark_covers_is_a_torn_tail() {
        let (dir, path, hole) = damaged_middle(false);
        let (journal, frames) = Journal::open(&path).unwrap();
        assert_eq!(frames.len(), 2, "the checkpoint and the first call");
        assert_eq!(journal.len(), hole);
        assert_eq!(fs::metadata(&path).unwrap().len(), hole);
        assert!(dir.path().join("calls.wal.torn").exists());
    }

    #[test]
    fn a_call_is_appended_without_an_fsync() {
        let (_dir, _path, mut journal) = journal();
        let first = call(1, b"one");
        journal.append(&first.0, &first.1).unwrap();
        assert_eq!(journal.durability().appended(), RecordNo::new(1));
        assert_eq!(journal.durability().mark(), RecordNo::ZERO);
        assert_eq!(
            journal.durability().make_all_durable(),
            Ok(RecordNo::new(1))
        );
    }
}
