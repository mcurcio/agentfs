//! File maps: a file's bytes as pieces of journal frames and of the checkout's
//! file as built, never as bytes in memory (docs/trees.md, "File maps").
//!
//! A write costs its own bytes at any file size: applying a call adds one
//! piece that points at the call's frame. POSIX has no insert, so a piece
//! keeps its file offset for life, and a map of the checkout's file reads it
//! at the same offset. The actor's saved state, an open's view and the
//! builder's replay run the one [`FileMap::apply`].
//!
//! A map of a few pieces holds them in one slice of exact size; past
//! [`SMALL_PIECES`] it holds them in a persistent ordered map keyed by file
//! offset, so a write costs O(log pieces + pieces it covers) at any number
//! of pieces, and a clone stays O(1).

use std::fs::File;
use std::io::{self, Read};
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use git2::Oid;

use super::Call;
#[cfg(doc)]
use super::layout::Layout;
use super::layout::Located;
#[cfg(doc)]
use super::layout::Place;
use crate::tree_actor::journal::durable::RecordNo;
use crate::tree_actor::journal::segment::{FramePos, Relocations};
use crate::tree_actor::stages;
use crate::tree_actor::{Error, Result};

/// The checkout's file as built: a hard link to it in the state area's
/// `bases/`, so a later checkout, which writes a new file and renames it over
/// this one, leaves it readable. It holds no descriptor: reads open the link
/// through a bounded cache ([`OPEN_BASES`]), so a tree of any size needs no
/// more descriptors than that. The link goes when the last map lets go. Its
/// name is derived from its directory and number, never stored; the
/// directory is one shared path per state area ([`interned`]).
pub(in crate::tree_actor) struct BaseFile {
    dir: Arc<Path>,
    id: u64,
    step: RecordNo,
    blob: Option<Oid>,
}

/// The most base files open at once, process-wide: the descriptors file maps
/// hold, whatever the trees' sizes.
const OPEN_BASES: usize = 256;

/// The open base files, most recently read last.
static OPEN: Mutex<Vec<(u64, Arc<File>)>> = Mutex::new(Vec::new());

/// Base files' link names, unique in this process.
static NEXT_BASE: AtomicU64 = AtomicU64::new(0);

/// The directories of base links in use, each held once, process-wide.
static DIRS: Mutex<Vec<Weak<Path>>> = Mutex::new(Vec::new());

/// `dir` as the one shared path every base file in it holds: one allocation
/// per state area, however many files it has.
fn interned(dir: &Path) -> Arc<Path> {
    let mut dirs = DIRS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(found) = dirs
        .iter()
        .filter_map(Weak::upgrade)
        .find(|held| **held == *dir)
    {
        return found;
    }
    dirs.retain(|held| held.strong_count() > 0);
    let dir: Arc<Path> = Arc::from(dir);
    dirs.push(Arc::downgrade(&dir));
    dir
}

impl BaseFile {
    /// The file at `path`, as built through `step`, holding blob `blob` when
    /// the builder knows it: linked as `bases/<n>` (`bases` is the state
    /// area's directory of links).
    pub fn open(
        bases: &Path,
        path: &Path,
        step: RecordNo,
        blob: Option<Oid>,
    ) -> io::Result<Arc<Self>> {
        let base = Self {
            dir: interned(bases),
            id: NEXT_BASE.fetch_add(1, Ordering::Relaxed),
            step,
            blob,
        };
        std::fs::hard_link(path, base.link())?;
        Ok(Arc::new(base))
    }

    /// The link's path, `<bases>/<id>`.
    fn link(&self) -> PathBuf {
        self.dir.join(self.id.to_string())
    }

    /// Reads `buf.len()` bytes at `at`, opening the link when the cache does
    /// not hold it (and letting the least recently read one go).
    fn read_exact_at(&self, buf: &mut [u8], at: u64) -> io::Result<()> {
        let file = {
            let mut open = OPEN.lock().unwrap_or_else(PoisonError::into_inner);
            match open.iter().position(|(id, _)| *id == self.id) {
                Some(index) => {
                    let entry = open.remove(index);
                    let file = Arc::clone(&entry.1);
                    open.push(entry);
                    file
                }
                None => {
                    let file = Arc::new(File::open(self.link())?);
                    if open.len() >= OPEN_BASES {
                        open.remove(0);
                    }
                    open.push((self.id, Arc::clone(&file)));
                    file
                }
            }
        };
        file.read_exact_at(buf, at)
    }
}

impl Drop for BaseFile {
    fn drop(&mut self) {
        OPEN.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(id, _)| *id != self.id);
        // A failure leaves a link the next start clears.
        let _ = std::fs::remove_file(self.link());
    }
}

/// The state area's directory of base links, emptied: what a start does
/// before any map reads a checkout. Links of an earlier run name nothing.
pub(in crate::tree_actor) fn clear_bases(bases: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(bases) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    std::fs::create_dir(bases)
}

impl std::fmt::Debug for BaseFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Base({}, {:?})", self.step.get(), self.blob)
    }
}

/// Where a piece's bytes are. `at` is the offset into the source where the
/// piece starts. A map of journal positions ([`FramePos`]) reads its bytes; a
/// map of [`Place`]s, a layout's replay, only names them.
#[derive(Clone, Debug)]
pub(in crate::tree_actor) enum Source<L = FramePos> {
    /// Zeros, and the record that made them: a call's gap or extension, or
    /// [`RecordNo::ZERO`] for zeros from before (a layout's own included).
    /// [`Layout::of_view`] tells a close's zeros from earlier ones by it.
    Zero {
        by: RecordNo,
    },
    Base {
        file: Arc<BaseFile>,
        at: u64,
    },
    Frame {
        pos: L,
        at: u64,
    },
}

impl<L: Located> Source<L> {
    /// The same source `by` bytes further on.
    fn advanced(&self, by: u64) -> Self {
        match self {
            Self::Zero { by: made } => Self::Zero { by: *made },
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

    /// Whether this source `mine` bytes in and `other` `theirs` bytes in
    /// name the same bytes: zeros, the same base file at the same offset, or
    /// the same frame bytes by [`Located::place`] (wherever compaction
    /// carried them).
    fn same_place(&self, mine: u64, other: &Self, theirs: u64) -> bool {
        match (self, other) {
            (Self::Zero { .. }, Self::Zero { .. }) => true,
            (Self::Base { file: a, at: x }, Self::Base { file: b, at: y }) => {
                Arc::ptr_eq(a, b) && x + mine == y + theirs
            }
            (Self::Frame { pos: a, at: x }, Self::Frame { pos: b, at: y }) => {
                let (a, b) = (a.place(), b.place());
                a.record == b.record && a.at + x + mine == b.at + y + theirs
            }
            _ => false,
        }
    }
}

impl Source {
    fn read(&self, buf: &mut [u8]) -> io::Result<()> {
        match self {
            Self::Zero { .. } => {
                buf.fill(0);
                Ok(())
            }
            Self::Base { file, at } => file.read_exact_at(buf, *at),
            Self::Frame { pos, at } => pos.read(*at, buf),
        }
    }
}

/// One run of a file: `len` bytes from `source`.
#[derive(Clone, Debug)]
pub(in crate::tree_actor) struct Piece<L = FramePos> {
    pub len: u64,
    pub source: Source<L>,
}

/// Up to this many pieces a map holds them in one exact-size slice; above, in
/// a persistent ordered map. Most maps hold one piece, and a map's node
/// costs hundreds of bytes for it; a map past this is a file under many
/// writes, where a slice's splice would cost every piece after the write.
pub(in crate::tree_actor) const SMALL_PIECES: usize = 32;

/// A file's pieces by start offset, in order, covering `0..len` with no gap.
/// Clones share them (O(1)); a small map copies its slice at a change, a
/// large one only the nodes on the path. Small becomes large when a change
/// passes [`SMALL_PIECES`], never back.
#[derive(Clone, Debug)]
enum Pieces<L: Located = FramePos> {
    Small(Arc<[(u64, Piece<L>)]>),
    Large(imbl::OrdMap<u64, Piece<L>>),
}

impl<L: Located> Default for Pieces<L> {
    fn default() -> Self {
        Self::Small(Arc::from(Vec::new()))
    }
}

/// The pieces in order from some offset.
pub(in crate::tree_actor) type PieceIter<'a, L = FramePos> =
    Box<dyn Iterator<Item = (u64, &'a Piece<L>)> + 'a>;

impl<L: Located> Pieces<L> {
    fn count(&self) -> usize {
        match self {
            Self::Small(pieces) => pieces.len(),
            Self::Large(pieces) => pieces.len(),
        }
    }

    fn iter(&self) -> PieceIter<'_, L> {
        self.from(0)
    }

    /// The pieces in order from the one holding `offset` (the first one
    /// when none starts at or before it).
    fn from(&self, offset: u64) -> PieceIter<'_, L> {
        match self {
            Self::Small(pieces) => {
                let first = first_at(pieces, offset.saturating_add(1)).saturating_sub(1);
                Box::new(pieces[first..].iter().map(|(start, piece)| (*start, piece)))
            }
            Self::Large(pieces) => {
                let first = pieces.get_prev(&offset).map_or(0, |(start, _)| *start);
                Box::new(pieces.range(first..).map(|(start, piece)| (*start, piece)))
            }
        }
    }

    /// A small map's pieces after an edit: kept small, or made large once
    /// they pass [`SMALL_PIECES`].
    fn set_small(&mut self, pieces: Vec<(u64, Piece<L>)>) {
        *self = if pieces.len() > SMALL_PIECES {
            Self::Large(pieces.into_iter().collect())
        } else {
            Self::Small(Arc::from(pieces))
        };
    }

    /// `piece` laid at `start`, over whatever was there.
    fn put(&mut self, start: u64, piece: Piece<L>) {
        let end = start + piece.len;
        match self {
            Self::Small(pieces) => {
                let mut pieces = pieces.to_vec();
                split(&mut pieces, start);
                split(&mut pieces, end);
                let (from, to) = (first_at(&pieces, start), first_at(&pieces, end));
                pieces.splice(from..to, [(start, piece)]);
                self.set_small(pieces);
            }
            Self::Large(pieces) => {
                split_large(pieces, start);
                split_large(pieces, end);
                let covered: Vec<u64> = pieces.range(start..end).map(|(at, _)| *at).collect();
                for at in covered {
                    pieces.remove(&at);
                }
                pieces.insert(start, piece);
            }
        }
    }

    /// `piece` added at `start`, the end of the pieces.
    fn push(&mut self, start: u64, piece: Piece<L>) {
        match self {
            Self::Small(pieces) => {
                let mut pieces = pieces.to_vec();
                pieces.push((start, piece));
                self.set_small(pieces);
            }
            Self::Large(pieces) => {
                pieces.insert(start, piece);
            }
        }
    }

    /// Every byte from `at` on dropped.
    fn cut(&mut self, at: u64) {
        match self {
            Self::Small(pieces) => {
                let mut pieces = pieces.to_vec();
                split(&mut pieces, at);
                pieces.truncate(first_at(&pieces, at));
                self.set_small(pieces);
            }
            Self::Large(pieces) => {
                split_large(pieces, at);
                let dropped: Vec<u64> = pieces.range(at..).map(|(start, _)| *start).collect();
                if dropped.len() * 2 > pieces.len() {
                    // Most go: build the rest instead.
                    *pieces = pieces
                        .range(..at)
                        .map(|(start, piece)| (*start, piece.clone()))
                        .collect();
                } else {
                    for start in dropped {
                        pieces.remove(&start);
                    }
                }
            }
        }
    }
}

impl Pieces {
    /// Each piece `moved` names a new place for, moved there. Returns
    /// whether any moved.
    fn relocate(&mut self, moved: &dyn Fn(&Piece) -> Option<FramePos>) -> bool {
        match self {
            Self::Small(pieces) => {
                if !pieces.iter().any(|(_, piece)| moved(piece).is_some()) {
                    return false;
                }
                let mut pieces = pieces.to_vec();
                for (_, piece) in &mut pieces {
                    move_piece(piece, moved);
                }
                self.set_small(pieces);
                true
            }
            Self::Large(pieces) => {
                let found: Vec<u64> = pieces
                    .iter()
                    .filter(|(_, piece)| moved(piece).is_some())
                    .map(|(start, _)| *start)
                    .collect();
                for start in &found {
                    if let Some(piece) = pieces.get_mut(start) {
                        move_piece(piece, moved);
                    }
                }
                !found.is_empty()
            }
        }
    }
}

/// `piece` read from where `moved` says its frame is now.
fn move_piece(piece: &mut Piece, moved: &dyn Fn(&Piece) -> Option<FramePos>) {
    if let Some(to) = moved(piece)
        && let Source::Frame { pos, .. } = &mut piece.source
    {
        *pos = to;
    }
}

/// A file's bytes as pieces, by file offset; they cover `0..len` with no gap.
/// Clones share the pieces until one changes (copy on write), so an open's
/// view starts as the saved map at O(1).
#[derive(Clone, Debug)]
pub struct FileMap<L: Located = FramePos> {
    len: u64,
    pieces: Pieces<L>,
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

impl<L: Located> Default for FileMap<L> {
    fn default() -> Self {
        Self::empty()
    }
}

/// How much one comparison or copy reads at a time.
const CHUNK: usize = 64 << 10;

impl<L: Located> FileMap<L> {
    pub(in crate::tree_actor) fn empty() -> Self {
        Self {
            len: 0,
            pieces: Pieces::default(),
            step: Some(RecordNo::ZERO),
        }
    }

    /// `len` bytes all "as before": zeros by no record. What
    /// [`Layout::of_close`] lays a close's calls on.
    pub(in crate::tree_actor) fn as_before(len: u64) -> Self {
        let mut map = Self::empty();
        if len > 0 {
            map.pieces.push(
                0,
                Piece {
                    len,
                    source: Source::Zero { by: RecordNo::ZERO },
                },
            );
        }
        map.len = len;
        map.step = None;
        map
    }

    /// The pieces in order, with their file offsets: what a layout reads.
    pub(in crate::tree_actor) fn pieces(&self) -> PieceIter<'_, L> {
        self.pieces.iter()
    }

    /// Every byte from `len` on dropped, when the map is longer.
    pub(in crate::tree_actor) fn cut(&mut self, len: u64) {
        if len < self.len {
            self.step = None;
            self.pieces.cut(len);
            self.len = len;
        }
    }

    /// `len` bytes of `source` laid at `start`, which is at most the map's
    /// length: a layout's range.
    pub(in crate::tree_actor) fn lay(&mut self, start: u64, len: u64, source: Source<L>) {
        if len > 0 {
            self.step = None;
            self.put(start, len, source, RecordNo::ZERO);
        }
    }
}

impl FileMap {
    /// The first `len` bytes of `base`, as saved at its built point.
    pub(in crate::tree_actor) fn of_base(base: Arc<BaseFile>, len: u64) -> Self {
        let step = Some(base.step);
        let pieces = if len > 0 {
            Pieces::Small(Arc::from([(
                0,
                Piece {
                    len,
                    source: Source::Base { file: base, at: 0 },
                },
            )]))
        } else {
            Pieces::default()
        };
        Self { len, pieces, step }
    }
}

impl<L: Located> FileMap<L> {
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

    /// How many pieces the map holds: what a close may do under the lock.
    pub(in crate::tree_actor) fn piece_count(&self) -> usize {
        self.pieces.count()
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
    pub(in crate::tree_actor) fn apply(&mut self, call: &Call, at: &L) -> Result<()> {
        let source = || Source::Frame {
            pos: at.clone(),
            at: 0,
        };
        let by = at.place().record;
        match *call {
            Call::Write { len: 0, .. } | Call::Append { len: 0 } => return Ok(()),
            Call::Write { offset, len } => {
                offset.checked_add(len).ok_or(Error::TooLarge)?;
                self.step = None;
                self.put(offset, len, source(), by);
            }
            Call::Append { len } => {
                self.len.checked_add(len).ok_or(Error::TooLarge)?;
                self.step = None;
                self.put(self.len, len, source(), by);
            }
            Call::Truncate(size) => {
                self.step = None;
                self.truncate(size, by);
            }
        }
        Ok(())
    }

    /// Lays `len` bytes of `source` at `start`; a gap before it reads as
    /// zeros record `by` made.
    fn put(&mut self, start: u64, len: u64, source: Source<L>, by: RecordNo) {
        if start > self.len {
            self.pieces.push(
                self.len,
                Piece {
                    len: start - self.len,
                    source: Source::Zero { by },
                },
            );
            self.len = start;
        }
        let piece = Piece { len, source };
        if start == self.len {
            self.pieces.push(start, piece);
        } else {
            self.pieces.put(start, piece);
        }
        self.len = self.len.max(start + len);
    }

    fn truncate(&mut self, size: u64, by: RecordNo) {
        if size > self.len {
            self.pieces.push(
                self.len,
                Piece {
                    len: size - self.len,
                    source: Source::Zero { by },
                },
            );
        } else if size < self.len {
            self.pieces.cut(size);
        }
        self.len = size;
    }
}

impl FileMap {
    /// Up to `buf.len()` bytes from `offset`, short at the end: one `pread`
    /// per piece.
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let end = self.len.min(offset.saturating_add(buf.len() as u64));
        for (start, piece) in self.pieces.from(offset).take_while(|(at, _)| *at < end) {
            let from = start.max(offset);
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

    /// The ranges where `self` reads a different source than `base` (a base
    /// file: the same one at the same offset; a frame: the same bytes by
    /// [`FramePos::key`]; zeros): every byte outside them is equal, so
    /// [`FileMap::same_as`] need compare only these. In order, merged.
    /// O(pieces of both); reads no byte.
    pub(in crate::tree_actor) fn differs_from(&self, base: &FileMap) -> Vec<Range<u64>> {
        let mut ranges: Vec<Range<u64>> = Vec::new();
        let mut add = |range: Range<u64>| match ranges.last_mut() {
            Some(last) if last.end >= range.start => last.end = last.end.max(range.end),
            _ => ranges.push(range),
        };
        let mut theirs = base.pieces.iter();
        let mut other = theirs.next();
        for (start, piece) in self.pieces.iter() {
            let end = start + piece.len;
            let mut at = start;
            while at < end {
                while let Some((from, held)) = other {
                    if from + held.len > at {
                        break;
                    }
                    other = theirs.next();
                }
                let Some((from, held)) = other else {
                    add(at..end);
                    break;
                };
                let to = end.min(from + held.len);
                if !piece.source.same_place(at - start, &held.source, at - from) {
                    add(at..to);
                }
                at = to;
            }
        }
        ranges
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

    /// Moves each piece read from a frame compaction carried to its copy, so
    /// the map no longer holds the dropped segment. Returns whether any moved.
    pub(in crate::tree_actor) fn relocate(&mut self, moved: &Relocations) -> bool {
        self.pieces.relocate(&|piece: &Piece| match &piece.source {
            Source::Frame { pos, .. } => pos.relocated(moved),
            _ => None,
        })
    }

    /// Whether some piece reads a journal frame: the map pins a segment.
    pub(in crate::tree_actor) fn on_frames(&self) -> bool {
        self.pieces
            .iter()
            .any(|(_, piece)| matches!(piece.source, Source::Frame { .. }))
    }

    /// Whether every piece reads from `base` (or is empty): nothing pins a
    /// journal segment.
    #[cfg(test)]
    pub(in crate::tree_actor) fn is_on(&self, base: &Arc<BaseFile>) -> bool {
        self.pieces.iter().all(|(_, piece)| match &piece.source {
            Source::Base { file, .. } => Arc::ptr_eq(file, base),
            Source::Zero { .. } => true,
            Source::Frame { .. } => false,
        })
    }
}

/// The index of the first piece starting at or after `at`.
fn first_at<L>(pieces: &[(u64, Piece<L>)], at: u64) -> usize {
    pieces.partition_point(|(start, _)| *start < at)
}

/// `pieces` with a piece boundary at `at`.
fn split<L: Located>(pieces: &mut Vec<(u64, Piece<L>)>, at: u64) {
    let index = first_at(pieces, at);
    let Some((start, piece)) = index.checked_sub(1).and_then(|i| pieces.get_mut(i)) else {
        return;
    };
    let start = *start;
    let end = start + piece.len;
    if end <= at {
        return;
    }
    let right = Piece {
        len: end - at,
        source: piece.source.advanced(at - start),
    };
    piece.len = at - start;
    pieces.insert(index, (at, right));
}

/// [`split`] for a large map: O(log pieces).
fn split_large<L: Located>(pieces: &mut imbl::OrdMap<u64, Piece<L>>, at: u64) {
    let Some((start, piece)) = pieces.get_prev_mut(&at) else {
        return;
    };
    let start = *start;
    let end = start + piece.len;
    if start == at || end <= at {
        return;
    }
    let right = Piece {
        len: end - at,
        source: piece.source.advanced(at - start),
    };
    piece.len = at - start;
    pieces.insert(at, right);
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
    /// A write splits the piece it lands in; the bytes copied are counted in
    /// `tree_stages`' `bytes_copied_per_write_is_its_own`, which runs alone.
    fn a_write_into_a_large_file_adds_one_piece() {
        let mut frames = Scratch::new();
        let mut map = FileMap::empty();
        map.apply(&Call::Truncate(1 << 30), &frames.add(b""))
            .unwrap();
        map.apply(
            &Call::Write {
                offset: 1 << 29,
                len: 4,
            },
            &frames.add(b"abcd"),
        )
        .unwrap();
        assert_eq!(map.piece_count(), 3);
        let mut four = [0; 4];
        map.read(1 << 29, &mut four).unwrap();
        assert_eq!(&four, b"abcd");
    }

    /// A random call on a file of length `len`, its bytes added to `frames`.
    fn random_call(rng: &mut Rng, frames: &mut Scratch, len: u64) -> (Call, FramePos, Vec<u8>) {
        let data: Vec<u8> = (0..1 + rng.next(24)).map(|_| rng.next(256) as u8).collect();
        let call = match rng.next(20) {
            0..=14 => Call::Write {
                offset: rng.next(len + 40),
                len: data.len() as u64,
            },
            15..=18 => Call::Append {
                len: data.len() as u64,
            },
            _ => Call::Truncate(rng.next(len + 20)),
        };
        let data = if matches!(call, Call::Truncate(_)) {
            Vec::new()
        } else {
            data
        };
        let pos = frames.add(&data);
        (call, pos, data)
    }

    /// Every piece of the map, as a layout names it.
    fn all_frames(map: &FileMap) -> super::super::layout::Layout {
        super::super::layout::Layout::of_view(map, RecordNo::ZERO)
    }

    /// A map that starts in the persistent map and one that starts small and
    /// moves to it past [`SMALL_PIECES`] hold the same bytes and pieces (by
    /// the layout, which names each piece) after every call, and match a
    /// byte oracle; clones keep their bytes.
    #[test]
    fn small_and_large_pieces_agree_with_a_byte_oracle() {
        for seed in 1..24_u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let mut frames = Scratch::new();
            let mut small = FileMap::empty();
            let mut large = FileMap {
                pieces: Pieces::Large(imbl::OrdMap::new()),
                ..FileMap::empty()
            };
            let mut bytes = Vec::new();
            let mut kept = Vec::new();
            let mut went_large = false;
            for step in 0..400 {
                let (call, pos, data) = random_call(&mut rng, &mut frames, small.len());
                small.apply(&call, &pos).unwrap();
                large.apply(&call, &pos).unwrap();
                oracle(&mut bytes, &call, &data);
                went_large |= matches!(small.pieces, Pieces::Large(_));
                assert_eq!(
                    small.piece_count(),
                    large.piece_count(),
                    "seed {seed} step {step}"
                );
                assert_eq!(
                    all_frames(&small),
                    all_frames(&large),
                    "seed {seed} step {step}"
                );
                if step % 25 == 0 {
                    assert_eq!(small.to_vec().unwrap(), bytes, "seed {seed} step {step}");
                    assert_eq!(large.to_vec().unwrap(), bytes, "seed {seed} step {step}");
                    kept.push((small.clone(), large.clone(), bytes.clone()));
                }
                let offset = rng.next(bytes.len() as u64 + 10);
                let mut part = vec![0xaa; rng.next(60) as usize];
                let n = large.read(offset, &mut part).unwrap();
                let expect = bytes
                    .get(offset as usize..)
                    .map_or(&[][..], |rest| &rest[..rest.len().min(part.len())]);
                assert_eq!(&part[..n], expect, "seed {seed} step {step}");
            }
            assert!(
                went_large,
                "seed {seed}: the small map never passed the bound"
            );
            for (small, large, bytes) in &kept {
                assert_eq!(&small.to_vec().unwrap(), bytes, "seed {seed}");
                assert_eq!(&large.to_vec().unwrap(), bytes, "seed {seed}");
            }
        }
    }

    /// `differs_from` is a superset: every byte outside its ranges is equal
    /// in both maps, a byte past the base's end is inside them, and
    /// `same_as` over them tells equal files.
    #[test]
    fn differs_from_covers_every_byte_that_differs() {
        for seed in 1..64_u64 {
            let mut rng = Rng(seed.wrapping_mul(0x2545_f491_4f6c_dd1d));
            let mut frames = Scratch::new();
            let mut base = FileMap::empty();
            for _ in 0..rng.next(60) {
                let (call, pos, _) = random_call(&mut rng, &mut frames, base.len());
                base.apply(&call, &pos).unwrap();
            }
            let mut view = base.clone();
            for _ in 0..rng.next(60) {
                let (call, pos, _) = random_call(&mut rng, &mut frames, view.len());
                // Sometimes the same bytes again, at the same place.
                if rng.next(3) == 0
                    && let Call::Write { offset, len } = call
                    && offset + len <= view.len()
                {
                    let mut same = vec![0; len as usize];
                    view.read(offset, &mut same).unwrap();
                    view.apply(&call, &frames.add(&same)).unwrap();
                    continue;
                }
                view.apply(&call, &pos).unwrap();
            }
            let ranges = view.differs_from(&base);
            let (mine, theirs) = (view.to_vec().unwrap(), base.to_vec().unwrap());
            for (at, byte) in mine.iter().enumerate() {
                let inside = ranges.iter().any(|r| r.contains(&(at as u64)));
                match theirs.get(at) {
                    Some(other) if !inside => assert_eq!(byte, other, "seed {seed} byte {at}"),
                    None => assert!(inside, "seed {seed}: byte {at} past the base"),
                    _ => {}
                }
            }
            assert!(
                ranges.windows(2).all(|w| w[0].end < w[1].start),
                "seed {seed}"
            );
            assert_eq!(
                view.same_as(&base, &ranges).unwrap(),
                mine == theirs,
                "seed {seed}"
            );
            assert!(base.differs_from(&base).is_empty(), "seed {seed}");
        }
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
        let late = BaseFile::open(dir.path(), &path, RecordNo::new(4), None).unwrap();
        map.rebase(Arc::clone(&late));
        assert!(!map.is_on(&late), "a base built before the save");
        let base = BaseFile::open(dir.path(), &path, RecordNo::new(5), None).unwrap();
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
}
