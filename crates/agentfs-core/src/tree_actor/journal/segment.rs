//! The journal's segment files: `journal/<first record:020>.seg`, each about
//! 64 MiB by default, a frame never spanning two (docs/trees.md, "Recording
//! and durability"), and the carry files `journal/carry-<built:020>` that
//! compaction copies live frames into. A segment is shared as `Arc<Segment>`:
//! a reader holding one keeps reading it after the journal unlinks it, and its
//! space returns when the last holder lets go. A frame is named by its record
//! and an offset into its tail ([`FramePos::key`]), wherever compaction
//! carries it; the journal's [`Relocations`] say where a carried frame now is,
//! and each holder moves its positions there ([`FramePos::relocated`]),
//! letting go of the old file. Two segments are the same file when their
//! paths are equal ([`Segment::same_file`]): every carry file's `first` is
//! [`RecordNo::ZERO`].

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use super::durable::RecordNo;

/// The default length past which the journal starts a new segment before a
/// frame (`Upkeep::segment_bytes`). A frame longer than this has a segment of
/// its own.
pub(in crate::tree_actor) const SEGMENT_BYTES: u64 = 64 << 20;

/// One segment or carry file, open for reading and appending.
pub(in crate::tree_actor) struct Segment {
    first: RecordNo,
    path: PathBuf,
    file: File,
    /// The bytes written to it: what the journal's length counts.
    len: AtomicU64,
}

impl std::fmt::Debug for Segment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = self
            .path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        write!(f, "Segment({name})")
    }
}

/// A carry file's name: the built point of the compaction that wrote it.
pub(in crate::tree_actor) fn carry_name(built: RecordNo) -> String {
    format!("carry-{:020}", built.get())
}

/// The built point a carry file's name gives; `None` for another name.
pub(in crate::tree_actor) fn carry_built(path: &Path) -> Option<RecordNo> {
    let digits = path.file_name()?.to_str()?.strip_prefix("carry-")?;
    (digits.len() == 20 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok().map(RecordNo::new))
        .flatten()
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

/// The carry files in `dir`, by the built point each name gives.
pub(in crate::tree_actor) fn list_carries(dir: &Path) -> io::Result<Vec<(RecordNo, PathBuf)>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if let Some(built) = carry_built(&path) {
            found.push((built, path));
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
        Ok(Arc::new(Self {
            first,
            path,
            file,
            len: AtomicU64::new(0),
        }))
    }

    /// A new, empty carry file for a compaction at `built`, outside the
    /// segment table: its `first` is [`RecordNo::ZERO`]. One that a failed
    /// compaction at the same point left, named by no checkpoint, is
    /// replaced.
    pub fn create_carry(dir: &Path, built: RecordNo) -> io::Result<Arc<Self>> {
        let path = dir.join(carry_name(built));
        match fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        Ok(Arc::new(Self {
            first: RecordNo::ZERO,
            path,
            file,
            len: AtomicU64::new(0),
        }))
    }

    /// An existing segment (or, with `first` zero, a carry file or the
    /// checkpoint file), its length the file's.
    pub fn open(first: RecordNo, path: &Path) -> io::Result<Arc<Self>> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let len = file.metadata()?.len();
        Ok(Arc::new(Self {
            first,
            path: path.to_owned(),
            file,
            len: AtomicU64::new(len),
        }))
    }

    /// The record its name gives: no record in it is older. Zero for a carry
    /// file, which is outside the segment table.
    pub fn first(&self) -> RecordNo {
        self.first
    }

    /// The same file: paths equal. Carry files all have `first` zero, so
    /// `first` never tells two apart.
    pub fn same_file(&self, other: &Segment) -> bool {
        self.path == other.path
    }

    /// The bytes written to it so far.
    pub fn len(&self) -> u64 {
        self.len.load(Ordering::Acquire)
    }

    /// Its writer (the actor appending, the compactor carrying, recovery
    /// cutting a torn tail) set its length to `len`.
    pub fn set_len(&self, len: u64) {
        self.len.store(len, Ordering::Release);
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    /// `fdatasync`: its records and its length on disk. Every fsync of a
    /// segment goes through here.
    pub fn sync(&self) -> io::Result<()> {
        crate::tree_actor::stages::assert_unlocked(crate::tree_actor::stages::Unlocked::Fsync);
        #[cfg(test)]
        SYNCS.with(|n| n.set(n.get() + 1));
        self.file.sync_data()
    }

    /// Fills `buf` from byte `at`. A segment never shrinks under a reader of
    /// what was appended, so a short read is damage.
    pub fn read_exact_at(&self, buf: &mut [u8], at: u64) -> io::Result<()> {
        self.file.read_exact_at(buf, at)
    }
}

#[cfg(test)]
thread_local! {
    /// This thread's segment fsyncs, for tests.
    pub(in crate::tree_actor) static SYNCS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A place in one record's tail: the record's bytes, read from the segment
/// that holds its frame now.
#[derive(Clone)]
pub struct FramePos {
    pub(in crate::tree_actor) segment: Arc<Segment>,
    /// The frame's record: with `offset`, the name of these bytes.
    pub(in crate::tree_actor) record: RecordNo,
    /// Where the record's tail starts in `segment`.
    pub(in crate::tree_actor) tail: u64,
    /// Bytes into the tail.
    pub(in crate::tree_actor) offset: u64,
}

impl FramePos {
    /// The start of `record`'s tail, at byte `tail` of `segment`.
    pub(in crate::tree_actor) fn new(segment: Arc<Segment>, record: RecordNo, tail: u64) -> Self {
        Self {
            segment,
            record,
            tail,
            offset: 0,
        }
    }

    /// The position `offset` bytes further into the same tail.
    pub(in crate::tree_actor) fn at(&self, offset: u64) -> Self {
        Self {
            segment: Arc::clone(&self.segment),
            record: self.record,
            tail: self.tail,
            offset: self.offset + offset,
        }
    }

    /// Reads `buf.len()` bytes from `offset` into the tail.
    pub(in crate::tree_actor) fn read(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.segment
            .read_exact_at(buf, self.tail + self.offset + offset)
    }

    /// Reads from where compaction carried the frame, if it moved from the
    /// segment this position reads.
    pub(in crate::tree_actor) fn relocate(&mut self, moved: &Relocations) {
        if let Some(to) = self.relocated(moved) {
            *self = to;
        }
    }

    /// The same bytes where compaction carried their frame, if it moved
    /// from the segment this position reads.
    pub(in crate::tree_actor) fn relocated(&self, moved: &Relocations) -> Option<Self> {
        let (segment, tail) = moved.find(self.record)?;
        (!segment.same_file(&self.segment)).then(|| Self {
            segment,
            record: self.record,
            tail,
            offset: self.offset,
        })
    }

    /// The tail's bytes `offset..offset + len`, read into memory: for tests.
    #[cfg(test)]
    pub(in crate::tree_actor) fn bytes(&self, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let len = usize::try_from(len).map_err(|_| io::Error::from_raw_os_error(libc::EFBIG))?;
        let mut buf = vec![0; len];
        self.read(offset, &mut buf)?;
        crate::tree_actor::stages::copied(len as u64);
        Ok(buf)
    }

    /// The same bytes: the record and the offset into its tail. Stable
    /// across the actor's and the builder's handles on one segment, and
    /// across compaction carrying the frame to another.
    pub(in crate::tree_actor) fn key(&self) -> (u64, u64) {
        (self.record.get(), self.offset)
    }
}

/// A position names its bytes by its record and offset ([`FramePos::key`]).
impl crate::tree_actor::recording::layout::Located for FramePos {
    fn place(&self) -> crate::tree_actor::recording::layout::Place {
        crate::tree_actor::recording::layout::Place {
            record: self.record,
            at: self.offset,
        }
    }

    fn at(&self, by: u64) -> Self {
        FramePos::at(self, by)
    }
}

impl std::fmt::Debug for FramePos {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}+{}@{:?}",
            self.record.get(),
            self.offset,
            self.segment
        )
    }
}

/// Where compaction carried each live frame: its record's segment and tail
/// now. The journal fills it as it drops a segment; the actor and the builder
/// read it to move their positions, so the old segment's space returns. Shared
/// by every clone.
#[derive(Clone, Default)]
pub(in crate::tree_actor) struct Relocations(Arc<Mutex<Moved>>);

/// Each carried record's file and tail now, and how many times that table
/// changed.
#[derive(Default)]
struct Moved {
    table: BTreeMap<RecordNo, (Arc<Segment>, u64)>,
    generation: u64,
}

impl Relocations {
    fn moved_table(&self) -> std::sync::MutexGuard<'_, Moved> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `record`'s frame is now at `tail` of `segment`.
    /// With where it was carried before, for [`Relocations::put_back`].
    pub(super) fn moved(
        &self,
        record: RecordNo,
        segment: &Arc<Segment>,
        tail: u64,
    ) -> Option<(Arc<Segment>, u64)> {
        let mut moved = self.moved_table();
        moved.generation += 1;
        moved.table.insert(record, (Arc::clone(segment), tail))
    }

    /// `record`'s copy in a carry file no checkpoint came to name is let
    /// go: its frame is where it was before that copy, `earlier` (an older
    /// carry file) or its own segment.
    pub(super) fn put_back(&self, record: RecordNo, earlier: Option<(Arc<Segment>, u64)>) {
        let mut moved = self.moved_table();
        moved.generation += 1;
        match earlier {
            Some(earlier) => moved.table.insert(record, earlier),
            None => moved.table.remove(&record),
        };
    }

    /// `segment` is unlinked: the frames carried into it that no later copy
    /// names are no longer anywhere. The table changes only for a carry
    /// file; a segment that held no carried frame moves no holder.
    pub(super) fn dropped(&self, segment: &Segment) {
        let mut moved = self.moved_table();
        let before = moved.table.len();
        moved.table.retain(|_, (held, _)| !held.same_file(segment));
        if moved.table.len() != before {
            moved.generation += 1;
        }
    }

    /// Where `record`'s frame was carried, if it was.
    pub fn find(&self, record: RecordNo) -> Option<(Arc<Segment>, u64)> {
        self.moved_table().table.get(&record).cloned()
    }

    /// How many times frames were carried or let go: a holder that saw
    /// this value has moved to every copy made before it. O(1).
    pub(in crate::tree_actor) fn generation(&self) -> u64 {
        self.moved_table().generation
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
    segment.set_len(bytes.len() as u64);
    (dir, FramePos::new(segment, RecordNo::new(1), 0))
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
