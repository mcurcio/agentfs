//! A save's layout: what a close left, as the record holds it (docs/trees.md,
//! "When history cannot check a save").
//!
//! A layout is the saved length, then each range the close wrote, as zeros or
//! as journal bytes named by a [`Place`] (a record and an offset into its
//! tail). Every other byte below the length is "as before": it keeps the
//! value it had before the save at the same offset, since POSIX has no
//! insert. A close records its layout in the tails of the [`Entry::Bytes`]
//! records just before its step ([`Layout::to_chunks`]), and the actor, a
//! start's replay and the builder each build the saved map from it with one
//! function, [`Layout::lay`]: every saved map is a function of records alone.
//!
//! The builder replays each save from its own inputs ([`Layout::replay`]) and
//! compares the result with the record by equality: layouts are canonical
//! (ranges disjoint and in order, adjacent zeros merged, adjacent bytes merged
//! when they are contiguous in one record, and every byte at or past the old
//! length that no call wrote an explicit zero range), so equal layouts are
//! the same save.
//!
//! [`Entry::Bytes`]: crate::tree_actor::journal::Entry::Bytes

use super::Call;
use super::file_map::{FileMap, Source};
use super::pages::{self, Run};
use crate::tree_actor::journal::durable::RecordNo;
use crate::tree_actor::journal::segment::FramePos;
use crate::tree_actor::journal::{PagePiece, SaveOutcome, TAKE_CHUNK, Tail};
use crate::tree_actor::{Error, Result};

/// The name of journal bytes: a record and an offset into its tail. Stable
/// across compaction ([`FramePos::key`]).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Place {
    pub record: RecordNo,
    pub at: u64,
}

/// What a file map's frame pieces hold: a journal position the bytes are
/// read from ([`FramePos`]), or only their name ([`Place`]), for a replay
/// that reads no byte.
pub trait Located: Clone + std::fmt::Debug {
    /// The bytes' name.
    fn place(&self) -> Place;
    /// The same tail `by` bytes further on.
    #[must_use]
    fn at(&self, by: u64) -> Self;
}

impl Located for Place {
    fn place(&self) -> Place {
        *self
    }

    fn at(&self, by: u64) -> Self {
        Self {
            record: self.record,
            at: self.at + by,
        }
    }
}

/// What one range of a layout holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) enum Laid {
    Zero,
    Frame(Place),
}

/// A save's layout. Canonical: disjoint ranges in order, adjacent mergeable
/// ranges merged, every unwritten byte at or past the old length a zero
/// range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) struct Layout {
    len: u64,
    /// Each range: its file offset, its length (never zero), what it holds.
    laid: Vec<(u64, u64, Laid)>,
}

/// A layout's short form, for display only (a flag's trailer, a log line):
/// its length and an FNV-1a digest of its length and ranges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    pub len: u64,
    pub digest: u64,
}

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{:016x}", self.len, self.digest)
    }
}

impl std::str::FromStr for Fingerprint {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        let (len, digest) = text.split_once(':').ok_or(Error::Format)?;
        if digest.len() != 16 {
            return Err(Error::Format);
        }
        Ok(Self {
            len: len.parse().map_err(|_| Error::Format)?,
            digest: u64::from_str_radix(digest, 16).map_err(|_| Error::Format)?,
        })
    }
}

/// The bytes one range takes in a chunk: offset, length, record, offset into
/// the record's tail, each a little-endian u64; record 0 is zeros.
const RANGE_BYTES: usize = 32;

/// The most ranges one chunk record holds: its tail is at most
/// [`TAKE_CHUNK`] bytes, well under a frame's limit.
const CHUNK_RANGES: usize = TAKE_CHUNK as usize / RANGE_BYTES;

/// Whether `next` at the end of `last`, `len` bytes long, is one range with it.
fn merges(last: Laid, len: u64, next: Laid) -> bool {
    match (last, next) {
        (Laid::Zero, Laid::Zero) => true,
        (Laid::Frame(a), Laid::Frame(b)) => a.record == b.record && a.at + len == b.at,
        _ => false,
    }
}

impl Layout {
    pub fn len(&self) -> u64 {
        self.len
    }

    /// A close's layout: `calls` applied by [`FileMap::apply`] on `saved_len`
    /// bytes all "as before", read off by [`Layout::of_view`] from record 1.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`], as [`FileMap::apply`].
    pub fn of_close<L: Located>(saved_len: u64, calls: &[(Call, L)]) -> Result<Self> {
        let mut map = FileMap::<L>::as_before(saved_len);
        for (call, at) in calls {
            map.apply(call, at)?;
        }
        Ok(Self::of_view(&map, RecordNo::new(1)))
    }

    /// The layout `view` holds for a close whose own records start at
    /// `first`: a frame of record `first` or later is bytes the close wrote,
    /// zeros a record from `first` on made are zeros it wrote, and every
    /// other piece is "as before".
    pub fn of_view<L: Located>(view: &FileMap<L>, first: RecordNo) -> Self {
        let mut laid: Vec<(u64, u64, Laid)> = Vec::new();
        for (start, piece) in view.pieces() {
            let what = match &piece.source {
                Source::Frame { pos, at } if pos.place().record >= first => {
                    Laid::Frame(pos.place().at(*at))
                }
                Source::Zero { by } if *by >= first => Laid::Zero,
                _ => continue,
            };
            match laid.last_mut() {
                Some((off, len, last)) if *off + *len == start && merges(*last, *len, what) => {
                    *len += piece.len;
                }
                _ => laid.push((start, piece.len, what)),
            }
        }
        Self {
            len: view.len(),
            laid,
        }
    }

    /// The builder's replay of a recorded save: [`pages::recorded_close_calls`]
    /// then [`Layout::of_close`]. `Ok(None)` when a piece names no run. The
    /// failpoint `tree.build_replay_skew` (action `error`) drops the last own
    /// call, as a replay that disagrees with its record.
    ///
    /// # Errors
    ///
    /// As [`Layout::of_close`].
    pub fn replay<L: Located>(
        saved_len: u64,
        own: &[(u64, Call, L)],
        writes: &[(u64, Run<L>)],
        pieces: &[PagePiece],
    ) -> Result<Option<Self>> {
        let mut own = own;
        if crate::failpoints::hit("tree.build_replay_skew").is_err() {
            own = &own[..own.len().saturating_sub(1)];
        }
        let Some(calls) = pages::recorded_close_calls(own, writes, pieces) else {
            return Ok(None);
        };
        Self::of_close(saved_len, &calls).map(Some)
    }

    /// The saved map: `before` with each range laid over it at the same
    /// offset, cut or grown to the length. `resolve` gives where a place's
    /// bytes are.
    ///
    /// # Errors
    ///
    /// [`Error::Format`] for an "as before" range past `before`'s length,
    /// whose bytes do not exist, or a place `resolve` cannot find; an error
    /// `resolve` returns.
    pub fn lay(
        &self,
        before: &FileMap,
        resolve: &mut dyn FnMut(Place) -> Result<Option<FramePos>>,
    ) -> Result<FileMap> {
        let mut at = 0;
        for (off, len, _) in &self.laid {
            if *off > at && *off > before.len() {
                return Err(Error::Format);
            }
            at = off + len;
        }
        if self.len > at && self.len > before.len() {
            return Err(Error::Format);
        }
        let mut map = before.clone();
        map.cut(self.len);
        for (off, len, what) in &self.laid {
            let source = match what {
                Laid::Zero => Source::Zero { by: RecordNo::ZERO },
                Laid::Frame(place) => Source::Frame {
                    pos: resolve(*place)?.ok_or(Error::Format)?,
                    at: 0,
                },
            };
            map.lay(*off, *len, source);
        }
        if map.len() != self.len {
            return Err(Error::Format);
        }
        Ok(map)
    }

    /// The layout's short form, for display.
    pub fn fingerprint(&self) -> Fingerprint {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut add = |bytes: &[u8]| {
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        add(&self.len.to_le_bytes());
        for chunk in self.to_chunks() {
            add(&chunk);
        }
        Fingerprint {
            len: self.len,
            digest: hash,
        }
    }

    /// Whether the save only grew the file from `before_len`: no range below
    /// it. A builder's line map then appends.
    pub fn only_extends(&self, before_len: u64) -> bool {
        self.len >= before_len
            && self
                .laid
                .first()
                .is_none_or(|(off, _, _)| *off >= before_len)
    }

    /// The ranges as chunk records' tails, in order, each at most
    /// [`CHUNK_RANGES`] ranges; none for a layout with no range.
    pub fn to_chunks(&self) -> Vec<Vec<u8>> {
        self.laid
            .chunks(CHUNK_RANGES)
            .map(|ranges| {
                let mut chunk = Vec::with_capacity(ranges.len() * RANGE_BYTES);
                for (off, len, what) in ranges {
                    let (record, at) = match what {
                        Laid::Zero => (0, 0),
                        Laid::Frame(place) => (place.record.get(), place.at),
                    };
                    for value in [*off, *len, record, at] {
                        chunk.extend_from_slice(&value.to_le_bytes());
                    }
                }
                chunk
            })
            .collect()
    }

    /// The layout of length `len` whose ranges `chunks` hold.
    ///
    /// # Errors
    ///
    /// [`Error::Format`] for chunks [`Layout::to_chunks`] did not write: a
    /// partial range, an empty one, ranges that overlap or are out of order,
    /// one past `len`, zeros naming a place, or a layout that is not
    /// canonical.
    pub fn from_chunks(len: u64, chunks: &[&[u8]]) -> Result<Self> {
        let mut laid: Vec<(u64, u64, Laid)> = Vec::new();
        for chunk in chunks {
            if chunk.len() % RANGE_BYTES != 0 || chunk.is_empty() {
                return Err(Error::Format);
            }
            for range in chunk.as_chunks::<RANGE_BYTES>().0 {
                let value = |i: usize| {
                    let mut bytes = [0; 8];
                    bytes.copy_from_slice(&range[i * 8..i * 8 + 8]);
                    u64::from_le_bytes(bytes)
                };
                let (off, n, record, at) = (value(0), value(1), value(2), value(3));
                let what = match record {
                    0 if at == 0 => Laid::Zero,
                    0 => return Err(Error::Format),
                    record => Laid::Frame(Place {
                        record: RecordNo::new(record),
                        at,
                    }),
                };
                let end = off.checked_add(n).ok_or(Error::Format)?;
                if n == 0 || end > len {
                    return Err(Error::Format);
                }
                if let Some((last_off, last_len, last)) = laid.last() {
                    let last_end = last_off + last_len;
                    if off < last_end || (off == last_end && merges(*last, *last_len, what)) {
                        return Err(Error::Format);
                    }
                }
                laid.push((off, n, what));
            }
        }
        if chunks.len() != laid.len().div_ceil(CHUNK_RANGES)
            || chunks
                .iter()
                .rev()
                .skip(1)
                .any(|chunk| chunk.len() != CHUNK_RANGES * RANGE_BYTES)
        {
            return Err(Error::Format);
        }
        Ok(Self { len, laid })
    }

    /// The layout of length `len` in the tails of `chunks`, the chunk records
    /// just before its step.
    ///
    /// # Errors
    ///
    /// A failed read; as [`Layout::from_chunks`].
    pub fn read(len: u64, chunks: &[Tail]) -> Result<Self> {
        let mut bytes = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            let mut buf = vec![0; usize::try_from(chunk.len).map_err(|_| Error::Format)?];
            chunk.pos.read(0, &mut buf)?;
            bytes.push(buf);
        }
        Self::from_chunks(len, &bytes.iter().map(Vec::as_slice).collect::<Vec<_>>())
    }

    /// The save's outcome as its step records it: its length, and how many
    /// chunk records before the step hold its ranges.
    pub fn outcome(&self) -> SaveOutcome {
        SaveOutcome::Saved {
            len: self.len,
            chunks: self.laid.len().div_ceil(CHUNK_RANGES) as u64,
        }
    }

    /// Its ranges, for evidence: (offset, length, record, offset into the
    /// tail), record 0 for zeros.
    pub fn ranges(&self) -> Vec<[u64; 4]> {
        self.laid
            .iter()
            .map(|(off, len, what)| match what {
                Laid::Zero => [*off, *len, 0, 0],
                Laid::Frame(place) => [*off, *len, place.record.get(), place.at],
            })
            .collect()
    }

    /// The layout `ranges` give ([`Layout::ranges`]), checked as
    /// [`Layout::from_chunks`] checks.
    ///
    /// # Errors
    ///
    /// As [`Layout::from_chunks`].
    pub fn of_ranges(len: u64, ranges: &[[u64; 4]]) -> Result<Self> {
        let bytes: Vec<u8> = ranges
            .iter()
            .flat_map(|range| range.iter().flat_map(|v| v.to_le_bytes()))
            .collect();
        let chunks: Vec<&[u8]> = bytes.chunks(CHUNK_RANGES * RANGE_BYTES).collect();
        Self::from_chunks(len, &chunks)
    }
}

/// A resolver of places from known journal positions: each record's tail
/// start, from any position in it.
#[derive(Default)]
pub(in crate::tree_actor) struct Positions(std::collections::HashMap<RecordNo, FramePos>);

impl Positions {
    /// Knows `pos`'s record.
    pub fn add(&mut self, pos: &FramePos) {
        self.0.entry(pos.record).or_insert_with(|| {
            let mut start = pos.clone();
            start.offset = 0;
            start
        });
    }

    /// Every record a frame piece of `map` reads.
    pub fn of_map(map: &FileMap) -> Self {
        let mut known = Self::default();
        for (_, piece) in map.pieces() {
            if let Source::Frame { pos, .. } = &piece.source {
                known.add(pos);
            }
        }
        known
    }

    /// Where `place`'s bytes are, if its record is known.
    pub fn find(&self, place: Place) -> Option<FramePos> {
        self.0.get(&place.record).map(|start| start.at(place.at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree_actor::journal::segment::Scratch;
    use crate::tree_actor::recording::file_map::BaseFile;
    use std::sync::Arc;

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

    /// Frames on one scratch segment, each its own record, as the journal
    /// numbers them: a place's record tells the calls apart.
    struct Frames {
        scratch: Scratch,
        next: u64,
    }

    impl Frames {
        fn new(first: u64) -> Self {
            Self {
                scratch: Scratch::new(),
                next: first,
            }
        }

        fn add(&mut self, bytes: &[u8]) -> FramePos {
            let mut pos = self.scratch.add(bytes);
            pos.record = RecordNo::new(self.next);
            // A record's tail starts where its bytes do.
            pos.tail += pos.offset;
            pos.offset = 0;
            self.next += 1;
            pos
        }
    }

    /// A random call on a file of `len` bytes: writes (into holes too),
    /// appends, truncates that cut or extend.
    fn random_call(rng: &mut Rng, frames: &mut Frames, len: u64) -> (Call, FramePos) {
        let data: Vec<u8> = (0..1 + rng.next(24)).map(|_| rng.next(256) as u8).collect();
        let call = match rng.next(20) {
            0..=11 => Call::Write {
                offset: rng.next(len + 40),
                len: data.len() as u64,
            },
            12..=16 => Call::Append {
                len: data.len() as u64,
            },
            _ => Call::Truncate(rng.next(len + 30)),
        };
        let data = if matches!(call, Call::Truncate(_)) {
            Vec::new()
        } else {
            data
        };
        (call, frames.add(&data))
    }

    /// A before map with holes: random calls from record 1, some saved on a
    /// base file read from the checkout.
    fn random_before(rng: &mut Rng, frames: &mut Frames, base: &Arc<BaseFile>) -> FileMap {
        let mut map = if rng.next(2) == 0 {
            FileMap::of_base(Arc::clone(base), rng.next(65))
        } else {
            FileMap::empty()
        };
        for _ in 0..rng.next(12) {
            let (call, pos) = random_call(rng, frames, map.len());
            map.apply(&call, &pos).unwrap();
        }
        map
    }

    fn resolver(calls: &[(Call, FramePos)]) -> Positions {
        let mut known = Positions::default();
        for (_, pos) in calls {
            known.add(pos);
        }
        known
    }

    /// A2: a close's layout laid on the map before it gives the bytes its
    /// calls give, and read off the open's view it is the layout of its
    /// calls: random befores (holes included), writes, appends, truncates
    /// that cut or extend, and page-write runs among the calls.
    #[test]
    fn a_layout_laid_on_its_before_gives_the_bytes_its_calls_give() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkout");
        std::fs::write(&path, [7_u8; 64]).unwrap();
        let base = BaseFile::open(dir.path(), &path, RecordNo::new(1), None).unwrap();
        for seed in 1..200_u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let mut frames = Frames::new(1);
            let before = random_before(&mut rng, &mut frames, &base);
            let handle = frames.next;
            frames.next += 1;
            let with_runs = rng.next(3) == 0;
            let mut after = before.clone();
            let mut calls = Vec::new();
            for _ in 0..1 + rng.next(30) {
                let (call, pos) = if with_runs && rng.next(4) == 0 {
                    // A page write's run: a write of part of its record.
                    let bytes: Vec<u8> = (0..8).map(|_| rng.next(256) as u8).collect();
                    let pos = frames.add(&bytes);
                    let skip = rng.next(4);
                    let run = Run {
                        offset: rng.next(after.len() + 8),
                        len: 8 - skip,
                        at: pos.at(skip),
                    };
                    (
                        Call::Write {
                            offset: run.offset,
                            len: run.len,
                        },
                        run.at,
                    )
                } else {
                    random_call(&mut rng, &mut frames, after.len())
                };
                after.apply(&call, &pos).unwrap();
                calls.push((call, pos));
            }
            let layout = Layout::of_close(before.len(), &calls).unwrap();
            let known = resolver(&calls);
            let laid = layout
                .lay(&before, &mut |place| Ok(known.find(place)))
                .unwrap();
            assert_eq!(
                laid.to_vec().unwrap(),
                after.to_vec().unwrap(),
                "seed {seed}"
            );
            if !with_runs {
                assert_eq!(
                    Layout::of_view(&after, RecordNo::new(handle)),
                    layout,
                    "seed {seed}"
                );
            }
            // The same layout through places alone: a replay that reads no byte.
            let places: Vec<(Call, Place)> = calls
                .iter()
                .map(|(call, pos)| (call.clone(), pos.place()))
                .collect();
            assert_eq!(
                Layout::of_close(before.len(), &places).unwrap(),
                layout,
                "seed {seed}"
            );
            assert_eq!(
                Layout::from_chunks(
                    layout.len(),
                    &layout
                        .to_chunks()
                        .iter()
                        .map(Vec::as_slice)
                        .collect::<Vec<_>>()
                )
                .unwrap(),
                layout,
                "seed {seed}"
            );
        }
    }

    /// A layout that names bytes "as before" past the file it is laid on
    /// cannot be laid: those bytes do not exist.
    #[test]
    fn an_as_before_range_past_the_before_is_refused() {
        let mut frames = Frames::new(1);
        let mut before = FileMap::empty();
        before
            .apply(&Call::Append { len: 4 }, &frames.add(b"abcd"))
            .unwrap();
        let pos = frames.add(b"XY");
        let layout =
            Layout::of_close(4, &[(Call::Write { offset: 4, len: 2 }, pos.clone())]).unwrap();
        let known = resolver(&[(Call::Append { len: 2 }, pos)]);
        let laid = layout
            .lay(&before, &mut |place| Ok(known.find(place)))
            .unwrap();
        assert_eq!(laid.to_vec().unwrap(), b"abcdXY");
        let mut short = FileMap::empty();
        short
            .apply(&Call::Append { len: 3 }, &frames.add(b"abc"))
            .unwrap();
        assert!(matches!(
            layout.lay(&short, &mut |place| Ok(known.find(place))),
            Err(Error::Format)
        ));
        assert!(
            matches!(layout.lay(&before, &mut |_| Ok(None)), Err(Error::Format)),
            "a place no record holds"
        );
    }

    fn chunk_of(ranges: &[[u64; 4]]) -> Vec<u8> {
        ranges
            .iter()
            .flat_map(|range| range.iter().flat_map(|v| v.to_le_bytes()))
            .collect()
    }

    /// A3: chunks round trip, a layout of more ranges than one chunk holds
    /// split across two, and chunks no layout writes are refused.
    #[test]
    fn chunks_round_trip_and_refuse_what_no_layout_writes() {
        let ranges: Vec<[u64; 4]> = (0..CHUNK_RANGES as u64 + 3)
            .map(|i| [i * 2, 1, 5 + i % 2, i])
            .collect();
        let layout = Layout::of_ranges(CHUNK_RANGES as u64 * 2 + 10, &ranges).unwrap();
        let chunks = layout.to_chunks();
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            layout.outcome(),
            SaveOutcome::Saved {
                len: layout.len(),
                chunks: 2
            }
        );
        let back = Layout::from_chunks(
            layout.len(),
            &chunks.iter().map(Vec::as_slice).collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(back, layout);
        assert_eq!(back.ranges(), ranges);
        let refused = |ranges: &[[u64; 4]]| {
            Layout::from_chunks(20, &[chunk_of(ranges).as_slice()]).unwrap_err()
        };
        assert!(
            matches!(refused(&[[0, 4, 3, 0], [2, 4, 4, 0]]), Error::Format),
            "overlap"
        );
        assert!(
            matches!(refused(&[[8, 2, 3, 0], [0, 4, 4, 0]]), Error::Format),
            "misorder"
        );
        assert!(
            matches!(refused(&[[0, 4, 3, 0], [4, 2, 3, 4]]), Error::Format),
            "contiguous bytes of one record, not merged"
        );
        assert!(
            matches!(refused(&[[0, 4, 0, 0], [4, 2, 0, 0]]), Error::Format),
            "adjacent zeros, not merged"
        );
        assert!(
            matches!(refused(&[[18, 4, 3, 0]]), Error::Format),
            "past the length"
        );
        assert!(matches!(refused(&[[0, 0, 3, 0]]), Error::Format), "empty");
        assert!(
            matches!(refused(&[[0, 2, 0, 1]]), Error::Format),
            "zeros at a place"
        );
        assert!(
            Layout::from_chunks(20, &[&[0; 31]]).is_err(),
            "a partial range"
        );
        // Contiguous in the file but not in one record's tail: two ranges.
        assert!(
            Layout::from_chunks(20, &[chunk_of(&[[0, 4, 3, 0], [4, 2, 3, 9]]).as_slice()]).is_ok()
        );
    }
}
