//! The kernel's one page cache per file: what the view serves it and records
//! from it (ledger R1.4; target git-model §3, §18).
//!
//! Linux FUSE keeps one page cache and one size per inode, whichever open
//! faulted a page in, and writes dirty pages back through the first writable
//! handle on that inode (`__fuse_write_file_get`), setting `FUSE_WRITE_CACHE`.
//! So a page write does not say which program made its stores, and a page fill
//! cannot be private to one open. The journal's order for a file is the only
//! source; the image and a writer's view are caches of folds of it:
//!
//! - a page fill (`FUSE_READ` without `FUSE_READ_LOCKOWNER`) reads the file's
//!   *image*: its saved bytes plus every open's calls and page writes, in
//!   arrival order, which is what a Linux page would hold;
//! - a page write records only the bytes that differ from what that page was
//!   last filled with or written back as, so a stale page never reverts a
//!   descriptor write;
//! - the recorded bytes belong to the file, not to the handle the kernel named:
//!   the next close of any write-open saves them with its own calls
//!   ([`calls_for_close`], the same function live and at recovery).
//!
//! Read-only descriptors keep their snapshot (R1.1). Writable descriptors read
//! their own calls plus the file's page writes (R1.3): the kernel cannot say
//! whose store a page holds. Nothing here is durable except the [`Run`]s in the
//! journal's page-write records; bases and image die with the kernel's pages.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{Call, Draft, Error, Handle, Result, Saved, Volume};

/// The bases' unit. Every kernel page size is a multiple of it, so a base map at
/// this size is exact whatever the kernel's own page size is.
const PAGE: u64 = 4096;

/// One contiguous run of bytes a page write changed, in file offsets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Run {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

impl Run {
    fn end(&self) -> u64 {
        self.offset.saturating_add(self.bytes.len() as u64)
    }

    /// Lays the run over `target`. `page_writeback` has checked the offsets.
    fn overlay(&self, target: &mut Vec<u8>) {
        let start = self.offset as usize;
        let end = start + self.bytes.len();
        if target.len() < end {
            target.resize(end, 0);
        }
        target[start..end].copy_from_slice(&self.bytes);
    }
}

/// The per-file state of a file with a write-open. It lives from the first
/// write-open to the close of the last one.
#[derive(Default)]
pub(super) struct ObjectPages {
    /// Arrival-order length, kept from the first write-open by arithmetic.
    len: u64,
    /// Cache: built at the first fill or page write, dropped at each save.
    image: Option<Vec<u8>>,
    /// Page number to the bytes last filled or written back. Memory only.
    bases: BTreeMap<u64, Vec<u8>>,
    /// The file's page writes with journal numbers, until the state drops.
    writes: Vec<(u64, Run)>,
    /// The number of the last step closing a write-open of this file.
    saved_through: u64,
}

/// The runs of `page`, which starts at file offset `at`, that differ from
/// `base`. Bytes past the end of `base` compare with zero, which is what the
/// kernel fills a page's tail with. Pure; adjacent changed bytes make one run.
pub(super) fn changed_runs(base: &[u8], page: &[u8], at: u64) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for (index, byte) in page.iter().enumerate() {
        if *byte == base.get(index).copied().unwrap_or(0) {
            continue;
        }
        let offset = at + index as u64;
        match runs.last_mut() {
            Some(run) if run.end() == offset => run.bytes.push(*byte),
            _ => runs.push(Run {
                offset,
                bytes: vec![*byte],
            }),
        }
    }
    runs
}

/// The length after `call`, by arithmetic.
fn len_after(len: u64, call: &Call) -> u64 {
    match call {
        Call::Write { bytes, .. } if bytes.is_empty() => len,
        Call::Write { offset, bytes } => len.max(offset.saturating_add(bytes.len() as u64)),
        Call::Append(bytes) => len.saturating_add(bytes.len() as u64),
        Call::Truncate(size) => *size,
    }
}

/// The range `call` touches over a file `len` bytes long: an append's at
/// replay, a truncate's from its size up.
fn touched_by(len: u64, call: &Call) -> Option<(u64, u64)> {
    match call {
        Call::Write { bytes, .. } if bytes.is_empty() => None,
        Call::Write { offset, bytes } => Some((*offset, offset.saturating_add(bytes.len() as u64))),
        Call::Append(bytes) => Some((len, len.saturating_add(bytes.len() as u64))),
        Call::Truncate(size) => Some((*size, u64::MAX)),
    }
}

/// The parts of `run` inside `touched`, in file order.
fn over_touched(run: &Run, touched: &[(u64, u64)]) -> Vec<Run> {
    let mut ranges = touched.to_vec();
    ranges.sort_unstable();
    let mut pieces: Vec<Run> = Vec::new();
    let mut from = run.offset;
    for (start, end) in ranges {
        let (start, end) = (start.max(from), end.min(run.end()));
        if start >= end {
            continue;
        }
        let bytes = &run.bytes[(start - run.offset) as usize..(end - run.offset) as usize];
        match pieces.last_mut() {
            Some(piece) if piece.end() == start => piece.bytes.extend_from_slice(bytes),
            _ => pieces.push(Run {
                offset: start,
                bytes: bytes.to_vec(),
            }),
        }
        from = end;
    }
    pieces
}

/// The calls a close of an open applies to the saved bytes, by journal number:
/// every own call; a page write above `saved_through` whole; a page write at or
/// below it and newer than the open's first call, only over bytes the open's own
/// earlier calls touched. A store that an earlier close saved stays where later
/// saves left it, unless this open's replay would overwrite it (R1.4, R9.16's
/// note: a later mapped write stands against a later close). Pure.
pub(super) fn calls_for_close(
    saved_len: u64,
    own: &[(u64, Call)],
    writes: &[(u64, Run)],
    saved_through: u64,
) -> Vec<Call> {
    let first_call = own.first().map(|(seq, _)| *seq);
    let mut calls = Vec::with_capacity(own.len());
    let mut len = saved_len;
    let mut touched = Vec::new();
    let mut own = own.iter().peekable();
    let mut writes = writes.iter().peekable();
    loop {
        let call_first = match (own.peek(), writes.peek()) {
            (None, None) => return calls,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some((call, _)), Some((write, _))) => call < write,
        };
        if call_first {
            let Some((_, call)) = own.next() else {
                return calls;
            };
            touched.extend(touched_by(len, call));
            len = len_after(len, call);
            calls.push(call.clone());
            continue;
        }
        let Some((seq, run)) = writes.next() else {
            return calls;
        };
        let pieces = if *seq > saved_through {
            vec![run.clone()]
        } else if first_call.is_some_and(|first| *seq > first) {
            over_touched(run, &touched)
        } else {
            Vec::new()
        };
        for piece in pieces {
            len = len.max(piece.end());
            calls.push(Call::Write {
                offset: piece.offset,
                bytes: piece.bytes,
            });
        }
    }
}

impl Volume {
    /// The bytes for a page fill of `object` in `tree`: up to `size` bytes from
    /// `offset`, short at the end. While the file has a write-open they come
    /// from its image and are recorded as the base of each page they cover; with
    /// none they are the saved bytes, and no state is kept.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when `tree` holds no such object.
    pub fn page_fill(
        &mut self,
        tree: &str,
        object: u64,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>> {
        let key = (tree.to_owned(), object);
        if !self.pages.contains_key(&key) {
            let saved = self.tree(tree)?.bytes.get(&object).ok_or(Error::NotFound)?;
            return Ok(slice_at(saved, offset, size).to_vec());
        }
        self.pages_build_image(&key)?;
        let state = self.pages.get_mut(&key).ok_or(Error::NotFound)?;
        let image = state.image.as_deref().ok_or(Error::NotFound)?;
        let bytes = slice_at(image, offset, size).to_vec();
        if !bytes.is_empty() {
            let last = (offset + bytes.len() as u64 - 1) / PAGE;
            for page in offset / PAGE..=last {
                state
                    .bases
                    .insert(page, slice_at(image, page * PAGE, PAGE as u32).to_vec());
            }
        }
        Ok(bytes)
    }

    /// Records a page write the kernel sent through `handle`: only the bytes
    /// that differ from each page's base, or from the image for a page with no
    /// base, in one journal record before this returns. A write that changed
    /// nothing records nothing. The record names the file, never the handle.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown handle; a journal failure as from
    /// [`Volume::write`], which also stops the actor when the journal cannot be
    /// trusted. A failure changes nothing here.
    pub fn page_writeback(&mut self, handle: u64, offset: u64, bytes: &[u8]) -> Result<()> {
        let h = self.handles.get(&handle).ok_or(Error::NotFound)?;
        let key = (h.tree.clone(), h.object);
        let end = offset
            .checked_add(bytes.len() as u64)
            .filter(|end| usize::try_from(*end).is_ok())
            .ok_or(Error::TooLarge)?;
        self.pages_build_image(&key)?;
        let state = self.pages.get(&key).ok_or(Error::NotFound)?;
        let image = state.image.as_deref().ok_or(Error::NotFound)?;
        let mut runs = Vec::new();
        let mut bases = Vec::new();
        let mut at = offset;
        while at < end {
            let page = at / PAGE;
            let within = (at - page * PAGE) as usize;
            let stop = end.min((page + 1) * PAGE);
            let chunk = &bytes[(at - offset) as usize..(stop - offset) as usize];
            let mut base = state
                .bases
                .get(&page)
                .cloned()
                .unwrap_or_else(|| slice_at(image, page * PAGE, PAGE as u32).to_vec());
            runs.extend(changed_runs(
                base.get(within..).unwrap_or_default(),
                chunk,
                at,
            ));
            if base.len() < within + chunk.len() {
                base.resize(within + chunk.len(), 0);
            }
            base[within..within + chunk.len()].copy_from_slice(chunk);
            bases.push((page, base));
            at = stop;
        }
        let seq = if runs.is_empty() {
            None
        } else {
            Some(self.record_page_write(&key.0, key.1, &runs)?)
        };
        // On record, so memory follows. Nothing below can fail.
        for handle in self
            .handles
            .values_mut()
            .filter(|h| h.tree == key.0 && h.object == key.1)
        {
            for run in &runs {
                run.overlay(&mut handle.bytes);
            }
        }
        let state = self.pages.get_mut(&key).ok_or(Error::NotFound)?;
        state.bases.extend(bases);
        if let Some(seq) = seq {
            for run in runs {
                state.len = state.len.max(run.end());
                if let Some(image) = &mut state.image {
                    run.overlay(image);
                }
                state.writes.push((seq, run));
            }
        }
        Ok(())
    }

    /// The size every reply must give the kernel for `object` while the file has
    /// a write-open: the arrival-order length, because the kernel keeps one size
    /// per inode and drops cached pages past any smaller size it is told
    /// (`fuse_change_attributes`). `None` with no write-open; the view then
    /// answers as it does for any other file.
    pub fn inode_size(&self, tree: &str, object: u64) -> Option<u64> {
        self.pages
            .get(&(tree.to_owned(), object))
            .map(|state| state.len)
    }

    /// The objects queue takes changed since the last call. The view drops the
    /// kernel's pages of each with the actor unlocked, because invalidation can
    /// wait on a page fill. Takes wait only for write-opens, so a read-only
    /// mapping would otherwise keep old pages and refault new ones.
    pub fn drain_changed(&mut self) -> Vec<(String, u64)> {
        std::mem::take(&mut self.changed)
    }

    /// A queue take: [`Volume::step`] plus a note of the files it changed.
    pub(super) fn pages_take<F>(&mut self, tree: &str, build: F) -> Result<(u64, Saved)>
    where
        F: FnOnce(&Self, u64) -> Result<Draft>,
    {
        let before = self.tree(tree)?.bytes.clone();
        let result = self.step(tree, build)?;
        if let Some(after) = self.trees.get(tree) {
            let changed = after.bytes.iter().filter(|(object, bytes)| {
                before
                    .get(object)
                    .is_some_and(|old| !Arc::ptr_eq(old, bytes) && old != *bytes)
            });
            self.changed
                .extend(changed.map(|(object, _)| (tree.to_owned(), *object)));
        }
        Ok(result)
    }

    /// Builds the image if it has none: saved bytes plus every live open's calls
    /// and the file's page writes, by the one close rule.
    fn pages_build_image(&mut self, key: &(String, u64)) -> Result<()> {
        let state = self.pages.get(key).ok_or(Error::NotFound)?;
        if state.image.is_some() {
            return Ok(());
        }
        let saved = self
            .tree(&key.0)?
            .bytes
            .get(&key.1)
            .ok_or(Error::NotFound)?;
        let mut own: Vec<(u64, Call)> = self
            .handles
            .values()
            .filter(|h| h.tree == key.0 && h.object == key.1)
            .flat_map(|h| h.calls.iter().cloned())
            .collect();
        own.sort_by_key(|(seq, _)| *seq);
        let mut image = saved.to_vec();
        for call in calls_for_close(saved.len() as u64, &own, &state.writes, state.saved_through) {
            call.apply(&mut image)?;
        }
        if let Some(state) = self.pages.get_mut(key) {
            state.image = Some(image);
        }
        Ok(())
    }

    /// Hook: a write-open of `object` in `tree` is on record. Recovery calls it
    /// too. No I/O. A later open's view starts with the file's unsaved page
    /// writes (R1.3).
    pub(super) fn pages_opened(&mut self, tree: &str, object: u64) {
        let key = (tree.to_owned(), object);
        let Some(state) = self.pages.get(&key) else {
            let len = self
                .trees
                .get(tree)
                .and_then(|t| t.bytes.get(&object))
                .map_or(0, |bytes| bytes.len() as u64);
            self.pages.insert(
                key,
                ObjectPages {
                    len,
                    ..ObjectPages::default()
                },
            );
            return;
        };
        let newest = self
            .handles
            .values_mut()
            .rev()
            .find(|h| h.tree == tree && h.object == object);
        if let Some(newest) = newest {
            for (_, run) in state
                .writes
                .iter()
                .filter(|(seq, _)| *seq > state.saved_through)
            {
                run.overlay(&mut newest.bytes);
            }
        }
    }

    /// Hook: a descriptor call on `object` is in the journal. No I/O.
    pub(super) fn pages_note_call(&mut self, tree: &str, object: u64, call: &Call) {
        if let Some(state) = self.pages.get_mut(&(tree.to_owned(), object)) {
            state.len = len_after(state.len, call);
            if let Some(image) = &mut state.image
                && call.apply(image).is_err()
            {
                state.image = None;
            }
        }
    }

    /// Hook: `closing` is about to save, on a copy of its tree: this cannot
    /// change anything, because the save may still fail. Returns the calls its
    /// save applies to the saved bytes, by journal number.
    pub(super) fn pages_calls_for_close(&self, closing: &Handle) -> Vec<Call> {
        let saved_len = self
            .trees
            .get(&closing.tree)
            .and_then(|t| t.bytes.get(&closing.object))
            .map_or(0, |bytes| bytes.len() as u64);
        match self.pages.get(&(closing.tree.clone(), closing.object)) {
            Some(state) => calls_for_close(
                saved_len,
                &closing.calls,
                &state.writes,
                state.saved_through,
            ),
            None => calls_for_close(saved_len, &closing.calls, &[], 0),
        }
    }

    /// Hook: the step `seq` that closed a write-open of `object` in `tree` is
    /// past its commit point and installed. Drops the image, so the next fill
    /// rebuilds it over the saved bytes, or the whole state with the last
    /// write-open. No I/O.
    pub(super) fn pages_saved(&mut self, tree: &str, object: u64, seq: u64) {
        let key = (tree.to_owned(), object);
        if self
            .handles
            .values()
            .any(|h| h.tree == tree && h.object == object)
        {
            if let Some(state) = self.pages.get_mut(&key) {
                state.saved_through = seq;
                state.image = None;
            }
        } else {
            self.pages.remove(&key);
        }
    }

    /// The files with a live write-open whose earlier close moved `saved_through`,
    /// for a checkpoint. This is the only home of that number while serving.
    pub(super) fn pages_saved_through(&self) -> Vec<(String, u64, u64)> {
        self.pages
            .iter()
            .filter(|(_, state)| state.saved_through > 0)
            .map(|((tree, object), state)| (tree.clone(), *object, state.saved_through))
            .collect()
    }

    /// Hook: recovery found these page writes and this `saved_through` for a
    /// file with a live write-open, before its closes run. No I/O.
    pub(super) fn pages_restore(
        &mut self,
        tree: &str,
        object: u64,
        writes: Vec<(u64, Run)>,
        saved_through: u64,
    ) {
        let state = self.pages.entry((tree.to_owned(), object)).or_default();
        state.writes = writes;
        state.saved_through = saved_through;
    }
}

/// Whether memory holds the page write numbered `seq` of `object` in `tree`: what
/// a compaction keeps. `writes` is in journal order.
pub(super) fn holds(
    pages: &BTreeMap<(String, u64), ObjectPages>,
    tree: &str,
    object: u64,
    seq: u64,
) -> bool {
    pages.get(&(tree.to_owned(), object)).is_some_and(|state| {
        state
            .writes
            .binary_search_by_key(&seq, |(number, _)| *number)
            .is_ok()
    })
}

/// Up to `size` bytes of `bytes` from `offset`, short at the end.
fn slice_at(bytes: &[u8], offset: u64, size: u32) -> &[u8] {
    let start = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(bytes.len());
    let end = start.saturating_add(size as usize).min(bytes.len());
    &bytes[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(offset: u64, bytes: &[u8]) -> Run {
        Run {
            offset,
            bytes: bytes.to_vec(),
        }
    }

    fn write(offset: u64, bytes: &[u8]) -> Call {
        Call::Write {
            offset,
            bytes: bytes.to_vec(),
        }
    }

    fn replay(seed: &[u8], calls: &[Call]) -> Vec<u8> {
        let mut bytes = seed.to_vec();
        for call in calls {
            call.apply(&mut bytes).unwrap();
        }
        bytes
    }

    #[test]
    fn changed_runs_table() {
        // (base, page, at, expected)
        type Case<'a> = (&'a [u8], &'a [u8], u64, &'a [(u64, &'a [u8])]);
        let cases: &[Case] = &[
            (b"abcdefgh", b"abcdefgh", 0, &[]),
            (b"abcdefgh", b"abXYefgh", 0, &[(2, b"XY")]),
            // Split runs stay split: the unchanged byte between is not recorded.
            (
                b"abcdefgh",
                b"XbcdYfgZ",
                4096,
                &[(4096, b"X"), (4100, b"Y"), (4103, b"Z")],
            ),
            (b"abcdefgh", b"XbYdefgh", 0, &[(0, b"X"), (2, b"Y")]),
            // Past the base's end compares with zero, so a tail of zeros is no change.
            (b"ab", b"ab\0\0", 0, &[]),
            (b"ab", b"ab\0Z", 8, &[(11, b"Z")]),
            (b"", b"\0\0Q\0", 0, &[(2, b"Q")]),
            // A page cropped shorter than its base ignores the rest of the base.
            (b"abcdefgh", b"abX", 0, &[(2, b"X")]),
        ];
        for (base, page, at, expected) in cases {
            let expected: Vec<Run> = expected.iter().map(|(o, b)| run(*o, b)).collect();
            assert_eq!(changed_runs(base, page, *at), expected, "{page:?}");
        }
    }

    #[test]
    fn calls_for_close_table() {
        const SEED: &[u8] = b"abcdefgh";
        // (saved, own, writes, saved_through, resulting bytes, why)
        type Case<'a> = (
            &'a [u8],
            Vec<(u64, Call)>,
            Vec<(u64, Run)>,
            u64,
            &'a [u8],
            &'a str,
        );
        let cases: Vec<Case> = vec![
            (SEED, vec![], vec![], 0, SEED, "nothing"),
            (
                SEED,
                vec![(1, write(0, b"WX"))],
                vec![],
                0,
                b"WXcdefgh",
                "own call",
            ),
            (
                SEED,
                vec![],
                vec![(1, run(2, b"12"))],
                0,
                b"ab12efgh",
                "a page write not saved yet, whatever handle closes",
            ),
            (
                SEED,
                vec![(1, write(0, b"WXYZ")), (3, write(2, b"!"))],
                vec![(2, run(2, b"12"))],
                0,
                b"WX!2efgh",
                "merged by journal number",
            ),
            (
                b"ab12efgh",
                vec![(1, Call::Truncate(1))],
                vec![(2, run(0, b"Q"))],
                3,
                b"a",
                "a saved store under a truncate's size stays out",
            ),
            (
                b"12cdefgh",
                vec![(1, write(0, b"WXYZ"))],
                vec![(2, run(0, b"12"))],
                3,
                b"12YZefgh",
                "third clause: a saved store over bytes this open wrote",
            ),
            (
                b"WXcdefgh",
                vec![(1, write(7, b"Q"))],
                vec![(2, run(0, b"12"))],
                4,
                b"WXcdefgQ",
                "third clause: a saved store touching none of this open's bytes",
            ),
            (
                b"12cdefgh",
                vec![(5, write(0, b"WXYZ"))],
                vec![(2, run(0, b"12"))],
                3,
                b"WXYZefgh",
                "a saved store older than this open's first call stays older",
            ),
            (
                b"abcd12",
                vec![(1, Call::Append(b"XY".to_vec()))],
                vec![(2, run(4, b"12"))],
                3,
                b"abcd12XY",
                "an append's range is its place at replay, past the saved store",
            ),
            (
                b"abcd",
                vec![(1, Call::Append(b"XY".to_vec()))],
                vec![(2, run(4, b"12"))],
                3,
                b"abcd12",
                "a saved store over the bytes an append wrote at replay",
            ),
            (
                b"abcd",
                vec![(1, Call::Append(b"XY".to_vec()))],
                vec![(2, run(4, b"12"))],
                0,
                b"abcd12",
                "above saved_through the store is whole, after the append",
            ),
            (
                b"abcdef",
                vec![(1, Call::Truncate(2))],
                vec![(2, run(3, b"99"))],
                3,
                b"ab\0\x39\x39",
                "a truncate touches from its size up, so a saved store above it returns",
            ),
            (
                b"abcdef",
                vec![(1, write(1, b"XX")), (4, write(5, b"YY"))],
                vec![(2, run(0, b"12345678")), (6, run(6, b"Z"))],
                3,
                b"a23deYZ",
                "several pieces and a later whole write",
            ),
        ];
        for (saved, own, writes, saved_through, expected, why) in cases {
            let calls = calls_for_close(saved.len() as u64, &own, &writes, saved_through);
            assert_eq!(replay(saved, &calls), expected, "{why}");
        }
    }

    #[test]
    fn over_touched_keeps_only_the_overlap_once() {
        let store = run(4, b"abcdef");
        let touched = [(0, 5), (4, 6), (8, u64::MAX)];
        assert_eq!(
            over_touched(&store, &touched),
            vec![run(4, b"ab"), run(8, b"ef")]
        );
        assert!(over_touched(&store, &[(0, 4), (10, 12)]).is_empty());
    }

    #[test]
    fn slices_are_short_at_the_end() {
        assert_eq!(slice_at(b"abc", 1, 9), b"bc");
        assert_eq!(slice_at(b"abc", 3, 9), b"");
        assert_eq!(slice_at(b"abc", u64::MAX, 9), b"");
    }

    #[test]
    fn lengths_go_by_arithmetic() {
        assert_eq!(len_after(8, &write(100, b"")), 8);
        assert_eq!(len_after(8, &write(6, b"abcd")), 10);
        assert_eq!(len_after(8, &Call::Append(b"abcd".to_vec())), 12);
        assert_eq!(len_after(8, &Call::Truncate(2)), 2);
    }
}
