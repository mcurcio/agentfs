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
//! journal's page-write records, which the image and the views read from
//! their frames; bases die with the kernel's pages.

use std::collections::BTreeMap;

use super::{Call, FileMap, Handle};
use crate::tree_actor::journal::PagePiece;
use crate::tree_actor::journal::durable::RecordNo;
use crate::tree_actor::journal::segment::{FramePos, Relocations};
use crate::tree_actor::{Error, Result, TreeActor, TreeId};

/// The bases' unit. Every kernel page size is a multiple of it, so a base map at
/// this size is exact whatever the kernel's own page size is.
const PAGE: u64 = 4096;

/// A run of bytes a page write changed, before it is recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) struct Changed {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

impl Changed {
    fn end(&self) -> u64 {
        self.offset.saturating_add(self.bytes.len() as u64)
    }
}

/// One contiguous run of bytes a page write changed, in file offsets, and
/// where its bytes are in the page write's frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tree_actor) struct Run {
    pub offset: u64,
    pub len: u64,
    pub at: FramePos,
}

impl Run {
    fn end(&self) -> u64 {
        self.offset.saturating_add(self.len)
    }

    /// The run's part `start..end` (file offsets inside it).
    fn part(&self, start: u64, end: u64) -> Self {
        Self {
            offset: start,
            len: end - start,
            at: self.at.at(start - self.offset),
        }
    }

    /// The run as the write it is.
    fn call(&self) -> (Call, FramePos) {
        (
            Call::Write {
                offset: self.offset,
                len: self.len,
            },
            self.at.clone(),
        )
    }
}

/// The per-file state of a file with a write-open. It lives from the first
/// write-open to the close of the last one.
#[derive(Default)]
pub(in crate::tree_actor) struct ObjectPages {
    /// Arrival-order length, kept from the first write-open by arithmetic.
    len: u64,
    /// Cache: built at the first fill or page write, dropped at each save.
    image: Option<FileMap>,
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
pub(in crate::tree_actor) fn changed_runs(base: &[u8], page: &[u8], at: u64) -> Vec<Changed> {
    let mut runs: Vec<Changed> = Vec::new();
    for (index, byte) in page.iter().enumerate() {
        if *byte == base.get(index).copied().unwrap_or(0) {
            continue;
        }
        let offset = at + index as u64;
        match runs.last_mut() {
            Some(run) if run.end() == offset => run.bytes.push(*byte),
            _ => runs.push(Changed {
                offset,
                bytes: vec![*byte],
            }),
        }
    }
    runs
}

/// The length after `call`, by arithmetic.
fn len_after(len: u64, call: &Call) -> u64 {
    call.len_after(len).unwrap_or(u64::MAX)
}

/// The parts of `run` inside `touched`, in file order.
fn over_touched(run: &Run, touched: &[std::ops::Range<u64>]) -> Vec<Run> {
    let mut ranges = touched.to_vec();
    ranges.sort_unstable_by_key(|r| (r.start, r.end));
    let mut pieces: Vec<Run> = Vec::new();
    let mut from = run.offset;
    for range in ranges {
        let (start, end) = (range.start.max(from), range.end.min(run.end()));
        if start >= end {
            continue;
        }
        match pieces.last_mut() {
            Some(piece) if piece.end() == start => piece.len += end - start,
            _ => pieces.push(run.part(start, end)),
        }
        from = end;
    }
    pieces
}

/// Which page-write pieces a close of an open applies, each with its page
/// write's journal number: a page write above `saved_through` whole; one at or
/// below it and newer than the open's first call, only over bytes the open's
/// own earlier calls touched. A store that an earlier close saved stays where
/// later saves left it, unless this open's replay would overwrite it (R1.4,
/// R9.16's note: a later mapped write stands against a later close). Pure.
pub(in crate::tree_actor) fn close_pieces(
    saved_len: u64,
    own: &[(u64, Call, FramePos)],
    writes: &[(u64, Run)],
    saved_through: u64,
) -> Vec<(u64, Run)> {
    let first_call = own.first().map(|(seq, _, _)| *seq);
    let mut chosen = Vec::new();
    let mut len = saved_len;
    let mut touched = Vec::new();
    let mut own = own.iter().peekable();
    let mut writes = writes.iter().peekable();
    loop {
        let call_first = match (own.peek(), writes.peek()) {
            (None, None) => return chosen,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some((call, _, _)), Some((write, _))) => call < write,
        };
        if call_first {
            let Some((_, call, _)) = own.next() else {
                return chosen;
            };
            touched.extend(call.touched(len));
            len = len_after(len, call);
            continue;
        }
        let Some((seq, run)) = writes.next() else {
            return chosen;
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
            chosen.push((*seq, piece));
        }
    }
}

/// The calls a close applies, in journal order, each with where its bytes
/// are: the open's own calls merged with the page-write pieces
/// [`close_pieces`] chose. The actor and the builder both run this, then
/// [`FileMap::apply`]. Pure.
pub(in crate::tree_actor) fn close_calls(
    own: &[(u64, Call, FramePos)],
    pieces: &[(u64, Run)],
) -> Vec<(Call, FramePos)> {
    let mut calls = Vec::with_capacity(own.len() + pieces.len());
    let mut own = own.iter().peekable();
    let mut pieces = pieces.iter().peekable();
    loop {
        let call_first = match (own.peek(), pieces.peek()) {
            (None, None) => return calls,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some((call, _, _)), Some((write, _))) => call < write,
        };
        if call_first {
            if let Some((_, call, pos)) = own.next() {
                calls.push((call.clone(), pos.clone()));
            }
        } else if let Some((_, piece)) = pieces.next() {
            calls.push(piece.call());
        }
    }
}

/// The calls a close of an open applies to the saved bytes: [`close_calls`]
/// over [`close_pieces`]. The same function live and at recovery.
pub(in crate::tree_actor) fn calls_for_close(
    saved_len: u64,
    own: &[(u64, Call, FramePos)],
    writes: &[(u64, Run)],
    saved_through: u64,
) -> Vec<(Call, FramePos)> {
    close_calls(own, &close_pieces(saved_len, own, writes, saved_through))
}

/// The calls a recorded save applies: the open's own calls merged with its
/// page-write pieces, each resolved in the file's page writes; `None` when a
/// piece names no run of its record.
fn recorded_close_calls(
    own: &[(u64, Call, FramePos)],
    writes: &[(u64, Run)],
    pieces: &[PagePiece],
) -> Option<Vec<(Call, FramePos)>> {
    let mut resolved = Vec::with_capacity(pieces.len());
    for piece in pieces {
        let record = piece.record.get();
        let runs: Vec<Run> = writes
            .iter()
            .filter(|(n, _)| *n == record)
            .map(|(_, run)| run.clone())
            .collect();
        resolved.push((record, resolve_piece(&runs, piece.offset, piece.len)?));
    }
    Some(close_calls(own, &resolved))
}

/// A recorded save replayed: the map it leaves and the calls it applied.
pub(in crate::tree_actor) type Replayed = (FileMap, Vec<(Call, FramePos)>);

/// What a recorded save leaves on `saved`: [`recorded_close_calls`] applied
/// by [`FileMap::apply`], with those calls. The builder and recovery's replay
/// both run it; `None` when a piece names no run of its record.
///
/// # Errors
///
/// A call that does not apply.
pub(in crate::tree_actor) fn replay_close(
    saved: &FileMap,
    own: &[(u64, Call, FramePos)],
    writes: &[(u64, Run)],
    pieces: &[PagePiece],
) -> Result<Option<Replayed>> {
    let Some(calls) = recorded_close_calls(own, writes, pieces) else {
        return Ok(None);
    };
    let mut map = saved.clone();
    for (call, pos) in &calls {
        map.apply(call, pos)?;
    }
    Ok(Some((map, calls)))
}

/// A piece of a page write, from its runs: the part of the run that holds
/// the range. `None` when no run does (the record is not that write).
pub(in crate::tree_actor) fn resolve_piece(runs: &[Run], offset: u64, len: u64) -> Option<Run> {
    let end = offset.checked_add(len)?;
    runs.iter()
        .find_map(|run| (run.offset <= offset && end <= run.end()).then(|| run.part(offset, end)))
}

impl TreeActor {
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
        tree: TreeId,
        object: u64,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>> {
        let key = (tree, object);
        if !self.pages.contains_key(&key) {
            let saved = self.tree(tree)?.bytes.get(&object).ok_or(Error::NotFound)?;
            return read_at(saved, offset, size as u64);
        }
        self.pages_build_image(&key)?;
        let state = self.pages.get_mut(&key).ok_or(Error::NotFound)?;
        let image = state.image.as_ref().ok_or(Error::NotFound)?;
        let bytes = read_at(image, offset, size as u64)?;
        if !bytes.is_empty() {
            let last = (offset + bytes.len() as u64 - 1) / PAGE;
            for page in offset / PAGE..=last {
                state.bases.insert(page, read_at(image, page * PAGE, PAGE)?);
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
    /// [`TreeActor::write`], which also stops the actor when the journal cannot be
    /// trusted. A failure changes nothing here.
    pub fn page_writeback(&mut self, handle: u64, offset: u64, bytes: &[u8]) -> Result<()> {
        let h = self.handles.get(&handle).ok_or(Error::NotFound)?;
        let key = (h.tree, h.object);
        let end = offset
            .checked_add(bytes.len() as u64)
            .filter(|end| i64::try_from(*end).is_ok())
            .ok_or(Error::TooLarge)?;
        self.pages_build_image(&key)?;
        let state = self.pages.get(&key).ok_or(Error::NotFound)?;
        let image = state.image.as_ref().ok_or(Error::NotFound)?;
        let mut changed = Vec::new();
        let mut bases = Vec::new();
        let mut at = offset;
        while at < end {
            let page = at / PAGE;
            let within = (at - page * PAGE) as usize;
            let stop = end.min((page + 1) * PAGE);
            let chunk = &bytes[(at - offset) as usize..(stop - offset) as usize];
            let mut base = match state.bases.get(&page) {
                Some(base) => base.clone(),
                None => read_at(image, page * PAGE, PAGE)?,
            };
            changed.extend(changed_runs(
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
        let recorded = if changed.is_empty() {
            None
        } else {
            Some(self.record_page_write(key.0, key.1, &changed)?)
        };
        let mut runs = Vec::with_capacity(changed.len());
        if let Some((_, pos)) = &recorded {
            let mut tail = 0;
            for run in &changed {
                runs.push(Run {
                    offset: run.offset,
                    len: run.bytes.len() as u64,
                    at: pos.at(tail),
                });
                tail += run.bytes.len() as u64;
            }
        }
        // On record, so memory follows. Nothing below can fail: each run was
        // checked against `i64` above.
        for handle in self
            .handles
            .values_mut()
            .filter(|h| h.tree == key.0 && h.object == key.1)
        {
            for run in &runs {
                let (call, pos) = run.call();
                let _ = handle.bytes.apply(&call, &pos);
            }
        }
        let state = self.pages.get_mut(&key).ok_or(Error::NotFound)?;
        state.bases.extend(bases);
        if let Some((seq, _)) = recorded {
            for run in runs {
                state.len = state.len.max(run.end());
                if let Some(image) = &mut state.image {
                    let (call, pos) = run.call();
                    let _ = image.apply(&call, &pos);
                }
                state.writes.push((seq, run));
            }
        }
        Ok(())
    }

    /// The last page write journalled for `object` while the file has a
    /// write-open: what a FLUSH of any of its opens must make durable.
    pub(in crate::tree_actor) fn pages_last_write(&self, tree: TreeId, object: u64) -> Option<u64> {
        self.pages
            .get(&(tree, object))
            .and_then(|state| state.writes.last().map(|(seq, _)| *seq))
    }

    /// The size every reply must give the kernel for `object` while the file has
    /// a write-open: the arrival-order length, because the kernel keeps one size
    /// per inode and drops cached pages past any smaller size it is told
    /// (`fuse_change_attributes`). `None` with no write-open; the view then
    /// answers as it does for any other file.
    pub fn inode_size(&self, tree: TreeId, object: u64) -> Option<u64> {
        self.pages.get(&(tree, object)).map(|state| state.len)
    }

    /// The objects queue takes changed since the last call. The view drops the
    /// kernel's pages of each with the actor unlocked, because invalidation can
    /// wait on a page fill. Takes wait only for write-opens, so a read-only
    /// mapping would otherwise keep old pages and refault new ones.
    pub fn drain_changed(&mut self) -> Vec<(TreeId, u64)> {
        std::mem::take(&mut self.changed)
    }

    /// Builds the image if it has none: the saved map plus every live open's
    /// calls and the file's page writes, by the one close rule.
    fn pages_build_image(&mut self, key: &(TreeId, u64)) -> Result<()> {
        let state = self.pages.get(key).ok_or(Error::NotFound)?;
        if state.image.is_some() {
            return Ok(());
        }
        let saved = self.tree(key.0)?.bytes.get(&key.1).ok_or(Error::NotFound)?;
        let mut own: Vec<(u64, Call, FramePos)> = self
            .handles
            .values()
            .filter(|h| h.tree == key.0 && h.object == key.1)
            .flat_map(|h| h.calls.iter().cloned())
            .collect();
        own.sort_by_key(|(seq, _, _)| *seq);
        let mut image = saved.clone();
        for (call, pos) in calls_for_close(saved.len(), &own, &state.writes, state.saved_through) {
            image.apply(&call, &pos)?;
        }
        if let Some(state) = self.pages.get_mut(key) {
            state.image = Some(image);
        }
        Ok(())
    }

    /// Hook: a write-open of `object` in `tree` is on record. Recovery calls it
    /// too. No I/O. A later open's view starts with the file's unsaved page
    /// writes (R1.3).
    pub(in crate::tree_actor) fn pages_opened(&mut self, tree: TreeId, object: u64) {
        let key = (tree, object);
        let Some(state) = self.pages.get(&key) else {
            let len = self
                .trees
                .get(&tree)
                .and_then(|t| t.bytes.get(&object))
                .map_or(0, FileMap::len);
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
                let (call, pos) = run.call();
                let _ = newest.bytes.apply(&call, &pos);
            }
        }
    }

    /// Hook: a descriptor call on `object` is in the journal, its bytes at
    /// `pos`. No I/O.
    pub(in crate::tree_actor) fn pages_note_call(
        &mut self,
        tree: TreeId,
        object: u64,
        call: &Call,
        pos: &FramePos,
    ) {
        if let Some(state) = self.pages.get_mut(&(tree, object)) {
            state.len = len_after(state.len, call);
            if let Some(image) = &mut state.image
                && image.apply(call, pos).is_err()
            {
                state.image = None;
            }
        }
    }

    /// Hook: `closing` is about to save: which page-write pieces its save
    /// applies, by journal number. Changes nothing, because the save may
    /// still fail.
    pub(in crate::tree_actor) fn pages_close_pieces(&self, closing: &Handle) -> Vec<(u64, Run)> {
        let saved_len = self
            .trees
            .get(&closing.tree)
            .and_then(|t| t.bytes.get(&closing.object))
            .map_or(0, FileMap::len);
        match self.pages.get(&(closing.tree, closing.object)) {
            Some(state) => close_pieces(
                saved_len,
                &closing.calls,
                &state.writes,
                state.saved_through,
            ),
            None => Vec::new(),
        }
    }

    /// Hook: the step `seq` that closed a write-open of `object` in `tree` is
    /// past its commit point and installed. Drops the image, so the next fill
    /// rebuilds it over the saved bytes, or the whole state with the last
    /// write-open. No I/O.
    pub(in crate::tree_actor) fn pages_saved(&mut self, tree: TreeId, object: u64, seq: u64) {
        let key = (tree, object);
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
    pub(in crate::tree_actor) fn pages_saved_through(&self) -> Vec<(String, u64, u64)> {
        self.pages
            .iter()
            .filter(|(_, state)| state.saved_through > 0)
            .map(|((tree, object), state)| (tree.to_string(), *object, state.saved_through))
            .collect()
    }

    /// Every page write memory holds, in record order: what a checkpoint
    /// names for recovery to read from the segments before it.
    pub(in crate::tree_actor) fn pages_held(&self) -> Vec<RecordNo> {
        let mut held: Vec<RecordNo> = self
            .pages
            .values()
            .flat_map(|state| state.writes.iter().map(|(seq, _)| RecordNo::new(*seq)))
            .collect();
        held.sort_unstable();
        held.dedup();
        held
    }

    /// Hook: recovery found these page writes and this `saved_through` for a
    /// file with a live write-open, before its closes run. No I/O.
    pub(in crate::tree_actor) fn pages_restore(
        &mut self,
        tree: TreeId,
        object: u64,
        writes: Vec<(u64, Run)>,
        saved_through: u64,
    ) {
        let state = self.pages.entry((tree, object)).or_default();
        state.writes = writes;
        state.saved_through = saved_through;
    }
}

/// Moves the page writes `pages` holds, and each image built of them, to
/// where compaction carried their frames.
pub(in crate::tree_actor) fn relocate(
    pages: &mut BTreeMap<(TreeId, u64), ObjectPages>,
    moved: &Relocations,
) {
    for state in pages.values_mut() {
        relocate_runs(&mut state.writes, moved);
        if let Some(image) = &mut state.image {
            image.relocate(moved);
        }
    }
}

/// Moves each run to where compaction carried its frame.
pub(in crate::tree_actor) fn relocate_runs(runs: &mut [(u64, Run)], moved: &Relocations) {
    for (_, run) in runs {
        run.at.relocate(moved);
    }
}

/// Whether memory holds the page write numbered `seq` of `object` in `tree`: what
/// a compaction keeps. `writes` is in journal order.
pub(in crate::tree_actor) fn holds(
    pages: &BTreeMap<(TreeId, u64), ObjectPages>,
    tree: TreeId,
    object: u64,
    seq: u64,
) -> bool {
    pages.get(&(tree, object)).is_some_and(|state| {
        state
            .writes
            .binary_search_by_key(&seq, |(number, _)| *number)
            .is_ok()
    })
}

/// Up to `size` bytes of `map` from `offset`, short at the end.
fn read_at(map: &FileMap, offset: u64, size: u64) -> Result<Vec<u8>> {
    let size = size.min(map.len().saturating_sub(offset));
    let mut bytes = vec![0; usize::try_from(size).map_err(|_| Error::TooLarge)?];
    let read = map.read(offset, &mut bytes)?;
    bytes.truncate(read);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree_actor::journal::segment::Scratch;

    struct T {
        frames: Scratch,
    }

    impl T {
        fn new() -> Self {
            Self {
                frames: Scratch::new(),
            }
        }

        fn run(&mut self, offset: u64, bytes: &[u8]) -> Run {
            Run {
                offset,
                len: bytes.len() as u64,
                at: self.frames.add(bytes),
            }
        }

        fn write(&mut self, seq: u64, offset: u64, bytes: &[u8]) -> (u64, Call, FramePos) {
            (
                seq,
                Call::Write {
                    offset,
                    len: bytes.len() as u64,
                },
                self.frames.add(bytes),
            )
        }

        fn append(&mut self, seq: u64, bytes: &[u8]) -> (u64, Call, FramePos) {
            (
                seq,
                Call::Append {
                    len: bytes.len() as u64,
                },
                self.frames.add(bytes),
            )
        }

        fn truncate(&mut self, seq: u64, size: u64) -> (u64, Call, FramePos) {
            (seq, Call::Truncate(size), self.frames.add(b""))
        }

        fn map(&mut self, bytes: &[u8]) -> FileMap {
            let mut map = FileMap::empty();
            map.apply(
                &Call::Append {
                    len: bytes.len() as u64,
                },
                &self.frames.add(bytes),
            )
            .unwrap();
            map
        }
    }

    fn replay(mut map: FileMap, calls: &[(Call, FramePos)]) -> Vec<u8> {
        for (call, pos) in calls {
            map.apply(call, pos).unwrap();
        }
        map.to_vec().unwrap()
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
            let expected: Vec<Changed> = expected
                .iter()
                .map(|(o, b)| Changed {
                    offset: *o,
                    bytes: b.to_vec(),
                })
                .collect();
            assert_eq!(changed_runs(base, page, *at), expected, "{page:?}");
        }
    }

    #[test]
    fn calls_for_close_table() {
        const SEED: &[u8] = b"abcdefgh";
        let mut t = T::new();
        // (saved, own, writes, saved_through, resulting bytes, why)
        type Case<'a> = (
            &'a [u8],
            Vec<(u64, Call, FramePos)>,
            Vec<(u64, Run)>,
            u64,
            &'a [u8],
            &'a str,
        );
        let cases: Vec<Case> = vec![
            (SEED, vec![], vec![], 0, SEED, "nothing"),
            (
                SEED,
                vec![t.write(1, 0, b"WX")],
                vec![],
                0,
                b"WXcdefgh",
                "own call",
            ),
            (
                SEED,
                vec![],
                vec![(1, t.run(2, b"12"))],
                0,
                b"ab12efgh",
                "a page write not saved yet, whatever handle closes",
            ),
            (
                SEED,
                vec![t.write(1, 0, b"WXYZ"), t.write(3, 2, b"!")],
                vec![(2, t.run(2, b"12"))],
                0,
                b"WX!2efgh",
                "merged by journal number",
            ),
            (
                b"ab12efgh",
                vec![t.truncate(1, 1)],
                vec![(2, t.run(0, b"Q"))],
                3,
                b"a",
                "a saved store under a truncate's size stays out",
            ),
            (
                b"12cdefgh",
                vec![t.write(1, 0, b"WXYZ")],
                vec![(2, t.run(0, b"12"))],
                3,
                b"12YZefgh",
                "third clause: a saved store over bytes this open wrote",
            ),
            (
                b"WXcdefgh",
                vec![t.write(1, 7, b"Q")],
                vec![(2, t.run(0, b"12"))],
                4,
                b"WXcdefgQ",
                "third clause: a saved store touching none of this open's bytes",
            ),
            (
                b"12cdefgh",
                vec![t.write(5, 0, b"WXYZ")],
                vec![(2, t.run(0, b"12"))],
                3,
                b"WXYZefgh",
                "a saved store older than this open's first call stays older",
            ),
            (
                b"abcd12",
                vec![t.append(1, b"XY")],
                vec![(2, t.run(4, b"12"))],
                3,
                b"abcd12XY",
                "an append's range is its place at replay, past the saved store",
            ),
            (
                b"abcd",
                vec![t.append(1, b"XY")],
                vec![(2, t.run(4, b"12"))],
                3,
                b"abcd12",
                "a saved store over the bytes an append wrote at replay",
            ),
            (
                b"abcd",
                vec![t.append(1, b"XY")],
                vec![(2, t.run(4, b"12"))],
                0,
                b"abcd12",
                "above saved_through the store is whole, after the append",
            ),
            (
                b"abcdef",
                vec![t.truncate(1, 2)],
                vec![(2, t.run(3, b"99"))],
                3,
                b"ab\0\x39\x39",
                "a truncate touches from its size up, so a saved store above it returns",
            ),
            (
                b"abcdef",
                vec![t.write(1, 1, b"XX"), t.write(4, 5, b"YY")],
                vec![(2, t.run(0, b"12345678")), (6, t.run(6, b"Z"))],
                3,
                b"a23deYZ",
                "several pieces and a later whole write",
            ),
        ];
        for (saved, own, writes, saved_through, expected, why) in cases {
            let calls = calls_for_close(saved.len() as u64, &own, &writes, saved_through);
            let map = t.map(saved);
            assert_eq!(replay(map, &calls), expected, "{why}");
        }
    }

    #[test]
    fn over_touched_keeps_only_the_overlap_once() {
        let mut t = T::new();
        let store = t.run(4, b"abcdef");
        let touched = [0..5, 4..6, 8..u64::MAX];
        let pieces = over_touched(&store, &touched);
        let read: Vec<(u64, Vec<u8>)> = pieces
            .iter()
            .map(|p| (p.offset, p.at.bytes(0, p.len).unwrap()))
            .collect();
        assert_eq!(read, vec![(4, b"ab".to_vec()), (8, b"ef".to_vec())]);
        assert!(over_touched(&store, &[0..4, 10..12]).is_empty());
    }

    #[test]
    fn reads_are_short_at_the_end() {
        let mut t = T::new();
        let map = t.map(b"abc");
        assert_eq!(read_at(&map, 1, 9).unwrap(), b"bc");
        assert_eq!(read_at(&map, 3, 9).unwrap(), b"");
        assert_eq!(read_at(&map, u64::MAX, 9).unwrap(), b"");
    }

    #[test]
    fn lengths_go_by_arithmetic() {
        let write = |offset, len| Call::Write { offset, len };
        assert_eq!(len_after(8, &write(100, 0)), 8);
        assert_eq!(len_after(8, &write(6, 4)), 10);
        assert_eq!(len_after(8, &Call::Append { len: 4 }), 12);
        assert_eq!(len_after(8, &Call::Truncate(2)), 2);
    }
}
