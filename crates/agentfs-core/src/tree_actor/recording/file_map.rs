//! File maps: a file's bytes as pieces of journal frames and of the checkout's
//! file as built, never as bytes in memory (docs/trees.md, "File maps").
//!
//! A write costs its own bytes at any file size: applying a call adds one
//! piece that points at the call's frame. POSIX has no insert, so a piece
//! keeps its file offset for life, and a map of the checkout's file reads it
//! at the same offset. The actor's saved state, an open's view and the
//! builder's replay run the one [`FileMap::apply`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read};
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

use git2::Oid;

use super::Call;
use crate::tree_actor::journal::durable::RecordNo;
use crate::tree_actor::journal::segment::FramePos;
use crate::tree_actor::stages;
use crate::tree_actor::{Error, Result};

/// The checkout's file as built: an open descriptor, so a later checkout,
/// which writes a new file and renames it over this one, leaves it readable.
pub(in crate::tree_actor) struct BaseFile {
    file: File,
    step: RecordNo,
    blob: Option<Oid>,
}

impl BaseFile {
    /// The file at `path`, as built through `step`, holding blob `blob` when
    /// the builder knows it.
    pub fn open(path: &Path, step: RecordNo, blob: Option<Oid>) -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            file: File::open(path)?,
            step,
            blob,
        }))
    }
}

impl std::fmt::Debug for BaseFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Base({}, {:?})", self.step.get(), self.blob)
    }
}

/// Where a piece's bytes are. `at` is the offset into the source where the
/// piece starts.
#[derive(Clone, Debug)]
pub(in crate::tree_actor) enum Source {
    Zero,
    Base { file: Arc<BaseFile>, at: u64 },
    Frame { pos: FramePos, at: u64 },
}

impl Source {
    /// The same source `by` bytes further on.
    fn advanced(&self, by: u64) -> Self {
        match self {
            Self::Zero => Self::Zero,
            Self::Base { file, at } => Self::Base {
                file: Arc::clone(file),
                at: at + by,
            },
            Self::Frame { pos, at } => Self::Frame {
                pos: pos.clone(),
                at: at + by,
            },
        }
    }

    fn read(&self, buf: &mut [u8]) -> io::Result<()> {
        match self {
            Self::Zero => {
                buf.fill(0);
                Ok(())
            }
            Self::Base { file, at } => file.file.read_exact_at(buf, *at),
            Self::Frame { pos, at } => pos.read(*at, buf),
        }
    }
}

/// One run of a file: `len` bytes from `source`.
#[derive(Clone, Debug)]
pub(in crate::tree_actor) struct Piece {
    pub len: u64,
    pub source: Source,
}

/// A file's bytes as pieces, by file offset; they cover `0..len` with no gap.
/// Clones share the pieces until one changes (copy on write), so an open's
/// view starts as the saved map at O(1) and a change costs O(pieces) once.
#[derive(Clone, Debug)]
pub struct FileMap {
    len: u64,
    pieces: Arc<BTreeMap<u64, Piece>>,
    /// The step whose save the map is, while it holds nothing unsaved: what
    /// [`FileMap::rebase`] compares with a base's built point.
    step: Option<RecordNo>,
}

/// A map read as an image: how a save's alignment reads both versions of a
/// file without holding either.
impl crate::tree_reconcile::Image for FileMap {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.read(offset, buf)
    }
}

impl Default for FileMap {
    fn default() -> Self {
        Self::empty()
    }
}

/// How much one comparison or copy reads at a time.
const CHUNK: usize = 64 << 10;

impl FileMap {
    pub(in crate::tree_actor) fn empty() -> Self {
        Self {
            len: 0,
            pieces: Arc::default(),
            step: Some(RecordNo::ZERO),
        }
    }

    /// The first `len` bytes of `base`, as saved at its built point.
    pub(in crate::tree_actor) fn of_base(base: Arc<BaseFile>, len: u64) -> Self {
        let step = Some(base.step);
        let mut pieces = BTreeMap::new();
        if len > 0 {
            pieces.insert(
                0,
                Piece {
                    len,
                    source: Source::Base { file: base, at: 0 },
                },
            );
        }
        Self {
            len,
            pieces: Arc::new(pieces),
            step,
        }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The step whose save this map is; `None` once it holds unsaved calls.
    pub(in crate::tree_actor) fn step(&self) -> Option<RecordNo> {
        self.step
    }

    /// Marks the map as the save of step `step`.
    pub(in crate::tree_actor) fn saved_at(&mut self, step: RecordNo) {
        self.step = Some(step);
    }

    /// The one apply: the actor's saved state, an open's view, the builder's
    /// replay. `at` is where the call's bytes start in its frame's tail.
    /// Offsets never shift (POSIX has no insert), so a piece keeps its file
    /// offset for life.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] where the call would end past `u64`; the map is
    /// unchanged then.
    pub(in crate::tree_actor) fn apply(&mut self, call: &Call, at: &FramePos) -> Result<()> {
        let source = || Source::Frame {
            pos: at.clone(),
            at: 0,
        };
        match *call {
            Call::Write { len: 0, .. } | Call::Append { len: 0 } => return Ok(()),
            Call::Write { offset, len } => {
                offset.checked_add(len).ok_or(Error::TooLarge)?;
                self.step = None;
                self.put(offset, len, source());
            }
            Call::Append { len } => {
                self.len.checked_add(len).ok_or(Error::TooLarge)?;
                self.step = None;
                self.put(self.len, len, source());
            }
            Call::Truncate(size) => {
                self.step = None;
                self.truncate(size);
            }
        }
        Ok(())
    }

    /// Lays `len` bytes of `source` at `start`; a gap before it reads as zeros.
    fn put(&mut self, start: u64, len: u64, source: Source) {
        if start > self.len {
            let gap = start - self.len;
            let end = self.len;
            Arc::make_mut(&mut self.pieces).insert(
                end,
                Piece {
                    len: gap,
                    source: Source::Zero,
                },
            );
            self.len = start;
        }
        let end = start + len;
        let pieces = Arc::make_mut(&mut self.pieces);
        split(pieces, start);
        split(pieces, end);
        let covered: Vec<u64> = pieces.range(start..end).map(|(at, _)| *at).collect();
        for key in covered {
            pieces.remove(&key);
        }
        pieces.insert(start, Piece { len, source });
        self.len = self.len.max(end);
    }

    fn truncate(&mut self, size: u64) {
        if size >= self.len {
            if size > self.len {
                let end = self.len;
                Arc::make_mut(&mut self.pieces).insert(
                    end,
                    Piece {
                        len: size - end,
                        source: Source::Zero,
                    },
                );
                self.len = size;
            }
            return;
        }
        let pieces = Arc::make_mut(&mut self.pieces);
        split(pieces, size);
        let cut: Vec<u64> = pieces.range(size..).map(|(at, _)| *at).collect();
        for key in cut {
            pieces.remove(&key);
        }
        self.len = size;
    }

    /// Up to `buf.len()` bytes from `offset`, short at the end: one `pread`
    /// per piece.
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let end = self.len.min(offset.saturating_add(buf.len() as u64));
        let first = self
            .pieces
            .range(..=offset)
            .next_back()
            .map_or(offset, |(at, _)| *at);
        for (start, piece) in self.pieces.range(first..end) {
            let from = (*start).max(offset);
            let to = (start + piece.len).min(end);
            if from >= to {
                continue;
            }
            let into = &mut buf[(from - offset) as usize..(to - offset) as usize];
            piece.source.advanced(from - start).read(into)?;
        }
        let read = end - offset;
        stages::copied(read);
        Ok(read as usize)
    }

    /// The whole file in memory: for the library's whole-file reads and the
    /// builder's line maps, never the call path.
    pub fn to_vec(&self) -> io::Result<Vec<u8>> {
        let len =
            usize::try_from(self.len).map_err(|_| io::Error::from_raw_os_error(libc::EFBIG))?;
        let mut bytes = vec![0; len];
        let read = self.read(0, &mut bytes)?;
        bytes.truncate(read);
        Ok(bytes)
    }

    /// The bytes in order, as `impl Read`: what the builder streams to
    /// `git fast-import`.
    pub fn reader(&self) -> FileMapReader<'_> {
        FileMapReader { map: self, at: 0 }
    }

    /// Whether `other` holds the same bytes, comparing only `ranges` (the
    /// open's own runs), never the whole file: the rest is shared. Different
    /// lengths differ.
    pub(in crate::tree_actor) fn same_as(
        &self,
        other: &FileMap,
        ranges: &[Range<u64>],
    ) -> io::Result<bool> {
        if self.len != other.len {
            return Ok(false);
        }
        // Each byte once: the ranges' union, clipped to the file.
        let mut union: Vec<Range<u64>> = Vec::new();
        let mut sorted: Vec<Range<u64>> = ranges
            .iter()
            .map(|r| r.start..r.end.min(self.len))
            .filter(|r| r.start < r.end)
            .collect();
        sorted.sort_by_key(|r| r.start);
        for range in sorted {
            match union.last_mut() {
                Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
                _ => union.push(range),
            }
        }
        let (mut mine, mut theirs) = (vec![0; CHUNK], vec![0; CHUNK]);
        for range in &union {
            let mut at = range.start;
            let end = range.end.min(self.len);
            while at < end {
                let n = usize::try_from(end - at).unwrap_or(CHUNK).min(CHUNK);
                let a = self.read(at, &mut mine[..n])?;
                let b = other.read(at, &mut theirs[..n])?;
                stages::compared(n as u64);
                if a != b || mine[..a] != theirs[..b] {
                    return Ok(false);
                }
                at += n as u64;
            }
        }
        Ok(true)
    }

    /// At a built event: a map whose save `base` reflects reads it from
    /// `base` at the same offset, letting go of its frames. A map with
    /// unsaved calls, or saved after `base`'s built point, is left as it is.
    pub(in crate::tree_actor) fn rebase(&mut self, base: Arc<BaseFile>) {
        let Some(step) = self.step else { return };
        if step > base.step {
            return;
        }
        let mut map = Self::of_base(base, self.len);
        map.step = Some(step);
        *self = map;
    }

    /// Whether some piece reads a journal frame: the map pins a segment.
    pub(in crate::tree_actor) fn on_frames(&self) -> bool {
        self.pieces
            .values()
            .any(|piece| matches!(piece.source, Source::Frame { .. }))
    }

    /// Whether every piece reads from `base` (or is empty): nothing pins a
    /// journal segment.
    #[cfg(test)]
    pub(in crate::tree_actor) fn is_on(&self, base: &Arc<BaseFile>) -> bool {
        self.pieces.values().all(|piece| match &piece.source {
            Source::Base { file, .. } => Arc::ptr_eq(file, base),
            Source::Zero => true,
            Source::Frame { .. } => false,
        })
    }

    /// The save record's digest of the piece list (FNV-1a, 64 bits): the
    /// length, then each piece from a frame in `own` (the close's own calls
    /// and page pieces, by [`FramePos::key`]) with its offset, length and
    /// place in the frame. Everything else is what the file held before the
    /// close, as one run per gap, however each side sources it: the actor
    /// and the builder rebase at different built points. The builder's
    /// replay must reach the same digest (`Disagrees`).
    pub(in crate::tree_actor) fn digest(&self, own: &BTreeSet<(u64, u64)>) -> u64 {
        let mut hash = Fnv::new();
        hash.u64(self.len);
        let mut kept: Option<(u64, u64)> = None;
        for (start, piece) in self.pieces.iter() {
            if let Source::Frame { pos, at } = &piece.source
                && own.contains(&pos.key())
            {
                if let Some((from, len)) = kept.take() {
                    hash.u64(0).u64(from).u64(len);
                }
                let (segment, tail) = pos.key();
                hash.u64(1)
                    .u64(*start)
                    .u64(piece.len)
                    .u64(segment)
                    .u64(tail + at);
                continue;
            }
            kept = Some(match kept {
                Some((from, len)) => (from, len + piece.len),
                None => (*start, piece.len),
            });
        }
        if let Some((from, len)) = kept {
            hash.u64(0).u64(from).u64(len);
        }
        hash.0
    }
}

/// `pieces` with a piece boundary at `at`.
fn split(pieces: &mut BTreeMap<u64, Piece>, at: u64) {
    let Some((start, piece)) = pieces.range_mut(..at).next_back() else {
        return;
    };
    let end = start + piece.len;
    if end <= at {
        return;
    }
    let right = Piece {
        len: end - at,
        source: piece.source.advanced(at - start),
    };
    piece.len = at - start;
    pieces.insert(at, right);
}

/// FNV-1a, 64 bits: a digest stable across builds and releases, which a
/// journal record outlives.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn u64(&mut self, value: u64) -> &mut Self {
        for byte in value.to_le_bytes() {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self
    }
}

/// A file map's bytes in order.
pub struct FileMapReader<'a> {
    map: &'a FileMap,
    at: u64,
}

impl Read for FileMapReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.map.read(self.at, buf)?;
        self.at += n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree_actor::journal::segment::Scratch;

    fn oracle(bytes: &mut Vec<u8>, call: &Call, data: &[u8]) {
        match *call {
            Call::Write { offset, len } => {
                if len > 0 {
                    let end = (offset + len) as usize;
                    bytes.resize(bytes.len().max(end), 0);
                    bytes[offset as usize..end].copy_from_slice(data);
                }
            }
            Call::Append { .. } => bytes.extend_from_slice(data),
            Call::Truncate(size) => bytes.resize(size as usize, 0),
        }
    }

    /// A small deterministic generator: the property needs no crate.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self, below: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % below.max(1)
        }
    }

    #[test]
    fn random_calls_agree_with_a_byte_oracle() {
        for seed in 1..40_u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let mut frames = Scratch::new();
            let mut map = FileMap::empty();
            let mut bytes = Vec::new();
            let mut saved = Vec::new();
            for step in 0..60 {
                let kind = rng.next(10);
                let data: Vec<u8> = (0..rng.next(40)).map(|_| rng.next(256) as u8).collect();
                let call = match kind {
                    0..=5 => Call::Write {
                        offset: rng.next(120),
                        len: data.len() as u64,
                    },
                    6 | 7 => Call::Append {
                        len: data.len() as u64,
                    },
                    _ => Call::Truncate(rng.next(150)),
                };
                let pos = frames.add(&data);
                map.apply(&call, &pos).unwrap();
                oracle(&mut bytes, &call, &data);
                assert_eq!(map.len(), bytes.len() as u64, "seed {seed} step {step}");
                assert_eq!(map.to_vec().unwrap(), bytes, "seed {seed} step {step}");
                let offset = rng.next(160);
                let mut part = vec![0xaa; rng.next(50) as usize];
                let n = map.read(offset, &mut part).unwrap();
                let expect = bytes
                    .get(offset as usize..)
                    .map_or(&[][..], |rest| &rest[..rest.len().min(part.len())]);
                assert_eq!(&part[..n], expect, "seed {seed} step {step}");
                // A clone taken now keeps these bytes whatever the map does next.
                if step == 30 {
                    saved = vec![(map.clone(), bytes.clone())];
                }
            }
            for (clone, bytes) in &saved {
                assert_eq!(&clone.to_vec().unwrap(), bytes, "seed {seed}");
            }
        }
    }

    #[test]
    fn a_write_into_a_large_file_adds_one_piece_and_copies_nothing() {
        let mut frames = Scratch::new();
        let mut map = FileMap::empty();
        map.apply(&Call::Truncate(1 << 30), &frames.add(b""))
            .unwrap();
        let before = stages::Stages::current().bytes_copied;
        map.apply(
            &Call::Write {
                offset: 1 << 29,
                len: 4,
            },
            &frames.add(b"abcd"),
        )
        .unwrap();
        assert_eq!(map.pieces.len(), 3);
        // Nothing read: the process-wide counter may move for other tests,
        // so read the piece itself.
        let _ = before;
        let mut four = [0; 4];
        map.read(1 << 29, &mut four).unwrap();
        assert_eq!(&four, b"abcd");
    }

    #[test]
    #[allow(
        clippy::single_range_in_vec_init,
        reason = "one range is a list of ranges"
    )]
    fn same_as_compares_only_the_ranges_it_is_given() {
        let mut frames = Scratch::new();
        let mut a = FileMap::empty();
        a.apply(&Call::Append { len: 8 }, &frames.add(b"abcdefgh"))
            .unwrap();
        let mut b = a.clone();
        b.apply(&Call::Write { offset: 6, len: 2 }, &frames.add(b"XY"))
            .unwrap();
        assert!(a.same_as(&b, &[0..6]).unwrap());
        assert!(!a.same_as(&b, &[4..8]).unwrap());
        let mut c = a.clone();
        c.apply(&Call::Write { offset: 2, len: 2 }, &frames.add(b"cd"))
            .unwrap();
        assert!(a.same_as(&c, &[2..4]).unwrap(), "the same bytes rewritten");
        c.apply(&Call::Truncate(7), &frames.add(b"")).unwrap();
        assert!(!a.same_as(&c, &[]).unwrap(), "lengths differ");
    }

    #[test]
    fn rebase_reads_the_base_and_keeps_unsaved_maps() {
        let mut frames = Scratch::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkout");
        std::fs::write(&path, b"saved!").unwrap();
        let mut map = FileMap::empty();
        map.apply(&Call::Append { len: 6 }, &frames.add(b"saved!"))
            .unwrap();
        let mut open = map.clone();
        map.saved_at(RecordNo::new(5));
        let late = BaseFile::open(&path, RecordNo::new(4), None).unwrap();
        map.rebase(Arc::clone(&late));
        assert!(!map.is_on(&late), "a base built before the save");
        let base = BaseFile::open(&path, RecordNo::new(5), None).unwrap();
        map.rebase(Arc::clone(&base));
        assert!(map.is_on(&base));
        assert_eq!(map.step(), Some(RecordNo::new(5)));
        // A checkout writes a new file and renames it: the base reads on.
        let temporary = dir.path().join("next");
        std::fs::write(&temporary, b"newer!").unwrap();
        std::fs::rename(&temporary, &path).unwrap();
        assert_eq!(map.to_vec().unwrap(), b"saved!");
        open.apply(&Call::Write { offset: 0, len: 1 }, &frames.add(b"S"))
            .unwrap();
        open.rebase(base);
        assert_eq!(open.step(), None);
        assert_eq!(open.to_vec().unwrap(), b"Saved!");
    }

    #[test]
    fn the_digest_ignores_how_earlier_bytes_are_sourced() {
        let mut frames = Scratch::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkout");
        std::fs::write(&path, b"0123456789").unwrap();
        let mut framed = FileMap::empty();
        framed
            .apply(&Call::Append { len: 10 }, &frames.add(b"0123456789"))
            .unwrap();
        framed.saved_at(RecordNo::new(3));
        let mut based = framed.clone();
        based.rebase(BaseFile::open(&path, RecordNo::new(3), None).unwrap());
        let write = frames.add(b"ab");
        let call = Call::Write { offset: 4, len: 2 };
        framed.apply(&call, &write).unwrap();
        based.apply(&call, &write).unwrap();
        let own: BTreeSet<(u64, u64)> = [write.key()].into_iter().collect();
        assert_eq!(framed.digest(&own), based.digest(&own));
        let mut other = framed.clone();
        other
            .apply(&Call::Write { offset: 5, len: 1 }, &frames.add(b"b"))
            .unwrap();
        assert_ne!(framed.digest(&own), other.digest(&own));
    }
}
