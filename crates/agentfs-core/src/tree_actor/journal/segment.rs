//! The journal's segment files: `journal/<first record:020>.seg`, each about
//! 64 MiB, a frame never spanning two (docs/trees.md, "Recording and
//! durability"). A segment is shared as `Arc<Segment>`: a reader holding one
//! keeps reading it after the journal unlinks it, and its space returns when
//! the last holder lets go.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::durable::RecordNo;

/// The length past which the journal starts a new segment before a frame.
/// A frame longer than this has a segment of its own.
pub(in crate::tree_actor) const SEGMENT_BYTES: u64 = 64 << 20;

/// One segment file, open for reading and appending.
pub(in crate::tree_actor) struct Segment {
    first: RecordNo,
    path: PathBuf,
    file: File,
}

impl std::fmt::Debug for Segment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Segment({})", self.first.get())
    }
}

/// A segment's file name: its first record, zero-padded so names sort.
pub(in crate::tree_actor) fn name_of(first: RecordNo) -> String {
    format!("{:020}.seg", first.get())
}

fn first_of(name: &str) -> Option<RecordNo> {
    let digits = name.strip_suffix(".seg")?;
    (digits.len() == 20 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok().map(RecordNo::new))
        .flatten()
}

/// The segments in `dir`, by first record. Other names are not the journal's
/// and are left alone.
pub(in crate::tree_actor) fn list(dir: &Path) -> io::Result<Vec<(RecordNo, PathBuf)>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(first) = entry.file_name().to_str().and_then(first_of) {
            found.push((first, entry.path()));
        }
    }
    found.sort();
    Ok(found)
}

impl Segment {
    /// A new, empty segment whose first record will be `first`. Its directory
    /// entry is not yet durable: [`super::durable::Durability`] fsyncs the
    /// directory before its mark covers any record in it.
    pub fn create(dir: &Path, first: RecordNo) -> io::Result<Arc<Self>> {
        let path = dir.join(name_of(first));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        Ok(Arc::new(Self { first, path, file }))
    }

    /// An existing segment.
    pub fn open(first: RecordNo, path: &Path) -> io::Result<Arc<Self>> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Arc::new(Self {
            first,
            path: path.to_owned(),
            file,
        }))
    }

    /// The record its name gives: no record in it is older, except a
    /// checkpoint's and carried frames.
    pub fn first(&self) -> RecordNo {
        self.first
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    /// Fills `buf` from byte `at`. A segment never shrinks under a reader of
    /// what was appended, so a short read is damage.
    pub fn read_exact_at(&self, buf: &mut [u8], at: u64) -> io::Result<()> {
        self.file.read_exact_at(buf, at)
    }
}

/// Where one frame's tail starts: a record's bytes, read from its segment.
#[derive(Clone)]
pub(in crate::tree_actor) struct FramePos {
    pub segment: Arc<Segment>,
    pub tail_at: u64,
}

impl FramePos {
    /// The position `offset` bytes further into the same tail.
    pub fn at(&self, offset: u64) -> Self {
        Self {
            segment: Arc::clone(&self.segment),
            tail_at: self.tail_at + offset,
        }
    }

    /// Reads `buf.len()` bytes from `offset` into the tail.
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.segment.read_exact_at(buf, self.tail_at + offset)
    }

    /// The tail's bytes `offset..offset + len`, read into memory: for the
    /// builder's line maps and a page fill's image, never the call path.
    pub fn bytes(&self, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let len = usize::try_from(len).map_err(|_| io::Error::from_raw_os_error(libc::EFBIG))?;
        let mut buf = vec![0; len];
        self.read(offset, &mut buf)?;
        crate::tree_actor::stages::copied(len as u64);
        Ok(buf)
    }

    /// The same bytes on disk: the same segment file and offset. Stable
    /// across the actor's and the builder's handles on one segment.
    pub fn key(&self) -> (u64, u64) {
        (self.segment.first.get(), self.tail_at)
    }
}

impl std::fmt::Debug for FramePos {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.segment.first.get(), self.tail_at)
    }
}

impl PartialEq for FramePos {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for FramePos {}

/// A segment holding `bytes`, for unit tests of what reads frames.
#[cfg(test)]
pub(in crate::tree_actor) fn scratch(bytes: &[u8]) -> (tempfile::TempDir, FramePos) {
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let segment = Segment::create(dir.path(), RecordNo::new(1)).unwrap_or_else(|e| panic!("{e}"));
    segment
        .file
        .write_all_at(bytes, 0)
        .unwrap_or_else(|e| panic!("{e}"));
    (
        dir,
        FramePos {
            segment,
            tail_at: 0,
        },
    )
}

/// Frames for unit tests: each `add` appends bytes to one scratch segment
/// and returns where they are.
#[cfg(test)]
pub(in crate::tree_actor) struct Scratch {
    _dir: tempfile::TempDir,
    pos: FramePos,
    len: u64,
}

#[cfg(test)]
impl Scratch {
    pub fn new() -> Self {
        let (dir, pos) = scratch(b"");
        Self {
            _dir: dir,
            pos,
            len: 0,
        }
    }

    pub fn add(&mut self, bytes: &[u8]) -> FramePos {
        self.pos
            .segment
            .file()
            .write_all_at(bytes, self.len)
            .unwrap_or_else(|e| panic!("{e}"));
        let at = self.pos.at(self.len);
        self.len += bytes.len() as u64;
        at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_sort_by_first_record_and_others_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        for first in [10, 9, 100] {
            Segment::create(dir.path(), RecordNo::new(first)).unwrap();
        }
        fs::write(dir.path().join("torn"), b"").unwrap();
        fs::write(dir.path().join("12.seg"), b"").unwrap();
        let firsts: Vec<u64> = list(dir.path())
            .unwrap()
            .into_iter()
            .map(|(first, _)| first.get())
            .collect();
        assert_eq!(firsts, vec![9, 10, 100]);
        assert!(Segment::create(dir.path(), RecordNo::new(9)).is_err());
    }

    #[test]
    fn an_unlinked_segment_reads_on_while_held() {
        let (dir, pos) = scratch(b"hello world");
        fs::remove_file(pos.segment.path()).unwrap();
        assert_eq!(pos.at(6).bytes(0, 5).unwrap(), b"world");
        drop(dir);
    }
}
