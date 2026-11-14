//! The tree actor's journal: the commit point for byte calls and pointer steps
//! (target git-model §3, §12).
//!
//! ```text
//! frame  = header head tail
//! header = "AFJ2" | u32le salt | u32le head_len | u32le tail_len
//!          | u32le crc32(head + tail) | u32le crc32(first 20 header bytes)
//! head   = JSON of Record
//! tail   = the raw bytes of the record's calls or runs, in order
//! ```
//!
//! Each write-open and step names its writer with the pod context it ran in
//! (`WriterAt`), so a recovery close commits as the writer that opened.
//! Format 4's `format` file gates the records' shape: the frame magic stays.
//!
//! The salt is random per journal and kept through compaction, so a workload's
//! bytes inside a tail cannot pass as a frame. An invalid frame with no later
//! valid frame is a torn tail; with one it is corruption, which an operator
//! decides about (docs/runbooks/recording-failures.md).

mod commit_point;
pub(super) mod durable;
mod fold;

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use agentfs_contracts::pod::WriterAt;
use serde::{Deserialize, Serialize};

use super::stages::{Timer, Which};
use super::{Error, Result};
pub(super) use commit_point::compact_at;
pub(super) use fold::{FoldedOpen, LiveOpen, fold};

const MAGIC: &[u8; 4] = b"AFJ2";
const HEADER: usize = 24;
pub(super) const MAX_FRAME: usize = 96 << 20;

/// One frame as a record and its tail bytes.
pub(super) type Frame = (Record, Vec<u8>);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Record {
    pub seq: u64,
    /// Audit only: no rule reads it (ledger R2).
    pub at_ns: u64,
    pub entry: Entry,
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
    file: File,
    path: PathBuf,
    length: u64,
    salt: u32,
    unknown: bool,
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

/// `None` when the frame would exceed [`MAX_FRAME`].
fn header(salt: u32, head: &[u8], tail: &[u8]) -> Option<[u8; HEADER]> {
    if head.len().checked_add(tail.len())? > MAX_FRAME {
        return None;
    }
    let mut bytes = [0; HEADER];
    bytes[..4].copy_from_slice(MAGIC);
    bytes[4..8].copy_from_slice(&salt.to_le_bytes());
    bytes[8..12].copy_from_slice(&u32::try_from(head.len()).ok()?.to_le_bytes());
    bytes[12..16].copy_from_slice(&u32::try_from(tail.len()).ok()?.to_le_bytes());
    bytes[16..20].copy_from_slice(&crc_body(head, tail).to_le_bytes());
    let header_crc = crc32fast::hash(&bytes[..20]);
    bytes[20..24].copy_from_slice(&header_crc.to_le_bytes());
    Some(bytes)
}

/// The lengths in a header whose magic, salt and checksum are right.
fn header_lengths(bytes: &[u8], salt: u32) -> Option<(usize, usize, u32)> {
    if bytes.len() < HEADER
        || &bytes[..4] != MAGIC
        || u32_at(bytes, 4) != salt
        || u32_at(bytes, 20) != crc32fast::hash(&bytes[..20])
    {
        return None;
    }
    let head = u32_at(bytes, 8) as usize;
    let tail = u32_at(bytes, 12) as usize;
    if head.checked_add(tail)? > MAX_FRAME {
        return None;
    }
    Some((head, tail, u32_at(bytes, 16)))
}

/// A whole valid frame at the start of `bytes`: its head, tail and length.
fn read_frame(bytes: &[u8], salt: u32) -> Option<(&[u8], &[u8], usize)> {
    let (head_len, tail_len, crc) = header_lengths(bytes, salt)?;
    let length = HEADER + head_len + tail_len;
    if bytes.len() < length {
        return None;
    }
    let head = &bytes[HEADER..HEADER + head_len];
    let tail = &bytes[HEADER + head_len..length];
    (crc_body(head, tail) == crc).then_some((head, tail, length))
}

/// Whether a later valid frame of this journal follows `seq`.
fn later_frame(bytes: &[u8], salt: u32, seq: u64) -> bool {
    let mut pattern = [0; 8];
    pattern[..4].copy_from_slice(MAGIC);
    pattern[4..].copy_from_slice(&salt.to_le_bytes());
    bytes
        .windows(pattern.len())
        .enumerate()
        .filter(|(_, window)| *window == pattern)
        .filter_map(|(at, _)| read_frame(&bytes[at..], salt))
        .filter_map(|(head, _, _)| serde_json::from_slice::<Record>(head).ok())
        .any(|record| record.seq > seq)
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

/// Writes one checkpoint and `retained` to the temporary name and syncs them.
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
    for (record, tail) in std::iter::once((checkpoint, &[][..]))
        .chain(retained.iter().map(|(r, t)| (r, t.as_slice())))
    {
        let head = serde_json::to_vec(record)?;
        let bytes =
            header(salt, &head, tail).ok_or_else(|| io::Error::from_raw_os_error(libc::EFBIG))?;
        file.write_all(&bytes)?;
        file.write_all(&head)?;
        file.write_all(tail)?;
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

    /// The journal and its frames. At a torn tail: saves its bytes to
    /// `calls.wal.torn`, logs `tree JournalTail: <n> bytes at <offset>`,
    /// truncates and syncs.
    ///
    /// # Errors
    ///
    /// [`Error::CorruptJournal`] for a missing or frameless file (offset 0), or
    /// at an invalid frame with a later valid one. Nothing is changed then.
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
        let mut last = 0;
        let mut torn = false;
        while offset < bytes.len() {
            let Some((head, tail, length)) = read_frame(&bytes[offset..], salt) else {
                if offset == 0 || later_frame(&bytes[offset + 1..], salt, last) {
                    return Err(Error::CorruptJournal(offset));
                }
                torn = true;
                break;
            };
            let record: Record =
                serde_json::from_slice(head).map_err(|_| Error::CorruptJournal(offset))?;
            let is_checkpoint = matches!(record.entry, Entry::Checkpoint { .. });
            if is_checkpoint != (offset == 0) || (offset != 0 && record.seq <= last) {
                return Err(Error::CorruptJournal(offset));
            }
            last = record.seq;
            frames.push((record, tail.to_vec()));
            offset += length;
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
        Ok((
            Self {
                file,
                path: path.to_owned(),
                length: offset as u64,
                salt,
                unknown: false,
            },
            frames,
        ))
    }

    /// Appends one frame and syncs it: when this returns Ok the record stands.
    ///
    /// # Errors
    ///
    /// A failed write whose rollback worked: the error, file as before. A
    /// failed sync or rollback: the error, and [`Journal::unknown`] turns true.
    pub fn append(&mut self, record: &Record, tail: &[u8]) -> Result<()> {
        if self.unknown {
            return Err(Error::Stopped);
        }
        let _timer = Timer::start(Which::JournalAppend);
        let head = serde_json::to_vec(record)?;
        let bytes = header(self.salt, &head, tail).ok_or(Error::TooLarge)?;
        let at = self.length;
        let written = (|| -> io::Result<()> {
            crate::failpoints::hit("tree.wal_before_header")?;
            self.file.write_all_at(&bytes, at)?;
            crate::failpoints::hit("tree.wal_after_header")?;
            self.file.write_all_at(&head, at + HEADER as u64)?;
            self.file
                .write_all_at(tail, at + (HEADER + head.len()) as u64)?;
            crate::failpoints::hit("tree.wal_after_payload")
        })();
        if let Err(e) = written {
            if self
                .file
                .set_len(at)
                .and_then(|()| self.file.sync_all())
                .is_err()
            {
                self.unknown = true;
            }
            return Err(e.into());
        }
        let synced = (|| -> io::Result<()> {
            crate::failpoints::hit("tree.wal_before_fsync")?;
            let _timer = Timer::start(Which::JournalFsync);
            self.file.sync_all()?;
            crate::failpoints::hit("tree.wal_after_fsync")
        })();
        if let Err(e) = synced {
            // After a failed fsync the page cache says nothing about the disk.
            self.unknown = true;
            return Err(e.into());
        }
        self.length = at + (HEADER + head.len() + tail.len()) as u64;
        Ok(())
    }

    /// True once this file cannot be trusted: reopen to recover.
    pub fn unknown(&self) -> bool {
        self.unknown
    }

    pub fn sync(&self) -> Result<()> {
        if self.unknown {
            return Err(Error::Stopped);
        }
        Ok(self.file.sync_all()?)
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
            let (head_len, tail_len, crc) = header_lengths(&bytes, self.salt)
                .ok_or_else(|| io::Error::other("invalid frame in an open journal"))?;
            let mut head = vec![0; head_len];
            self.file.read_exact_at(&mut head, at + HEADER as u64)?;
            let record: Record = serde_json::from_slice(&head)?;
            if !first && keep(&record) {
                let mut tail = vec![0; tail_len];
                self.file
                    .read_exact_at(&mut tail, at + (HEADER + head_len) as u64)?;
                // Rewriting computes a new checksum: never launder a bad frame.
                if crc_body(&head, &tail) != crc {
                    return Err(io::Error::other("invalid frame in an open journal"));
                }
                kept.push((record, tail));
            }
            first = false;
            at += (HEADER + head_len + tail_len) as u64;
        }
        Ok(kept)
    }

    /// Replaces the journal with `checkpoint` then `retained`, keeping the salt.
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
            let file = OpenOptions::new().read(true).write(true).open(&self.path)?;
            self.length = file.metadata()?.len();
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
            seq: 0,
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
                seq,
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
        let kept = journal.retained(|record| record.seq == 2).unwrap();
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
        file.write_all(b"AFJ2 torn").unwrap();
        drop(file);
        let (journal, frames) = Journal::open(&path).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(journal.len(), length);
        assert_eq!(fs::metadata(&path).unwrap().len(), length);
        assert_eq!(
            fs::read(dir.path().join("calls.wal.torn")).unwrap(),
            b"AFJ2 torn"
        );
    }
}
