//! An open's calls read back from the journal with the actor unlocked: the
//! journal is their one home, so a handle keeps no call list (docs/trees.md,
//! "File maps"). Under the lock a close takes a [`CallReader`], a snapshot of
//! the linked segments and their lengths (O(segments)); with the lock
//! released it visits the open's `Call` frames in record order. A frame
//! compaction carried keeps its record number in a later segment, and a
//! segment a reader holds stays readable after it is unlinked, so a
//! snapshot reads every call made before it, whatever compaction does next.

use std::collections::BTreeMap;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

use super::durable::RecordNo;
use super::segment::{FramePos, Segment};
use super::{Entry, HEADER, Journal, Record, read_header};
use crate::tree_actor::recording::Call;
use crate::tree_actor::{Error, Result};

/// The longest head a `Call` record has: its time, the handle and the call,
/// each number at most 20 digits, as JSON (about 130 bytes). A longer head is
/// another record's, skipped unparsed.
const CALL_HEAD: usize = 256;

/// The journal's linked segments as one moment left them, each read only up
/// to its length then. Taken under the actor's lock; read without it.
pub(in crate::tree_actor) struct CallReader {
    segments: Vec<(Arc<Segment>, u64)>,
    salt: u32,
}

#[cfg(test)]
thread_local! {
    /// The readers this thread took, for tests: a close that reads no
    /// journal takes none.
    pub(in crate::tree_actor) static READERS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl Journal {
    /// A snapshot of the segments linked now: O(segments), no read.
    pub(in crate::tree_actor) fn call_reader(&self) -> CallReader {
        #[cfg(test)]
        READERS.with(|n| n.set(n.get() + 1));
        CallReader {
            segments: self.segments.clone(),
            salt: self.salt,
        }
    }
}

impl CallReader {
    /// Visits each `Call` frame of the open `handle` in record order, with
    /// where its bytes are: carried frames at their copies. `handle` is the
    /// open's own record number, so no earlier frame is one of its calls.
    /// Reads frame heads only: a call's bytes are never copied out, and a
    /// long tail is stepped over unread (short ones share a read with the
    /// heads around them).
    ///
    /// # Errors
    ///
    /// [`crate::tree_actor::Error::Format`] for a frame that does not parse
    /// or is missing; an error `visit` returns stops the walk and is
    /// returned.
    pub(in crate::tree_actor) fn for_each_call<F>(&self, handle: u64, mut visit: F) -> Result<()>
    where
        F: FnMut(u64, Call, FramePos) -> Result<()>,
    {
        // A frame and its carried copy may both be linked (a compaction that
        // failed before it dropped the original): one each, by record.
        let mut calls: BTreeMap<u64, (Call, FramePos)> = BTreeMap::new();
        for (segment, length) in &self.segments {
            let mut window = Window::new(segment, *length);
            let mut at = 0;
            while at < *length {
                let bytes = window.at(at)?;
                let header = read_header(bytes, self.salt).ok_or(Error::Format)?;
                let frame = header.frame_len();
                if at + frame > *length {
                    return Err(Error::Format);
                }
                if header.record.get() > handle && header.head <= CALL_HEAD {
                    let head = bytes
                        .get(HEADER..HEADER + header.head)
                        .ok_or(Error::Format)?;
                    let record = Record::parse(header.record, head).map_err(|_| Error::Format)?;
                    if let Entry::Call { handle: of, call } = record.entry
                        && of == handle
                    {
                        call.check_tail(header.tail as u64)?;
                        let tail = at + (HEADER + header.head) as u64;
                        let pos = FramePos::new(Arc::clone(segment), header.record, tail);
                        calls.entry(header.record.get()).or_insert((call, pos));
                    }
                }
                at += frame;
            }
        }
        for (seq, (call, pos)) in calls {
            visit(seq, call, pos)?;
        }
        Ok(())
    }

    /// Every call of `handle` in record order: what a close replays.
    ///
    /// # Errors
    ///
    /// As [`CallReader::for_each_call`].
    pub(in crate::tree_actor) fn calls(&self, handle: u64) -> Result<Vec<(u64, Call, FramePos)>> {
        let mut calls = Vec::new();
        self.for_each_call(handle, |seq, call, pos| {
            calls.push((seq, call, pos));
            Ok(())
        })?;
        Ok(calls)
    }

    /// The frame of record `seq`, carried or not: what `agentfs verify`
    /// reads a kept `Save` step from. `None` when no linked segment holds it.
    ///
    /// # Errors
    ///
    /// As [`CallReader::for_each_call`].
    #[allow(dead_code, reason = "frozen interface of agentfs-ln6.11")]
    pub(in crate::tree_actor) fn record(&self, seq: RecordNo) -> Result<Option<super::Frame>> {
        let _ = seq;
        todo!("agentfs-ln6.11")
    }

    /// The runs of each `PageWrite` record in `records` of `tree`'s
    /// `object`, by record, as the builder holds them (`runs_from_frame`):
    /// the page writes a kept save's pieces name.
    ///
    /// # Errors
    ///
    /// As [`CallReader::for_each_call`]; [`crate::tree_actor::Error::Format`]
    /// for a record in `records` that is not such a page write.
    #[allow(dead_code, reason = "frozen interface of agentfs-ln6.11")]
    pub(in crate::tree_actor) fn page_writes(
        &self,
        tree: crate::tree_actor::TreeId,
        object: u64,
        records: &std::collections::BTreeSet<RecordNo>,
    ) -> Result<Vec<(u64, crate::tree_actor::recording::pages::Run)>> {
        let _ = (tree, object, records);
        todo!("agentfs-ln6.11")
    }
}

/// How much of a segment one read brings in: many small frames' heads at
/// once, while a frame with a long tail is stepped over, its bytes unread.
const WINDOW: usize = 64 << 10;

/// A segment read forward through one buffer.
struct Window<'a> {
    segment: &'a Segment,
    length: u64,
    buf: Vec<u8>,
    /// The segment offset of `buf[0]`, and how much of `buf` is read.
    from: u64,
    held: usize,
}

impl<'a> Window<'a> {
    fn new(segment: &'a Segment, length: u64) -> Self {
        Self {
            segment,
            length,
            buf: vec![0; WINDOW],
            from: 0,
            held: 0,
        }
    }

    /// The bytes from `at`: a frame's header and as much of its head as a
    /// call's could be, or up to the segment's end.
    fn at(&mut self, at: u64) -> Result<&[u8]> {
        let want = usize::try_from(self.length - at)
            .unwrap_or(usize::MAX)
            .min(HEADER + CALL_HEAD);
        let end = at + want as u64;
        if at < self.from || end > self.from + self.held as u64 {
            let fill = usize::try_from(self.length - at)
                .unwrap_or(usize::MAX)
                .min(WINDOW);
            read_full(self.segment, &mut self.buf[..fill], at)?;
            self.from = at;
            self.held = fill;
        }
        let start = (at - self.from) as usize;
        Ok(&self.buf[start..start + want])
    }
}

/// Fills `buf` from byte `at` of `segment`, which holds at least that much.
fn read_full(segment: &Segment, buf: &mut [u8], at: u64) -> Result<()> {
    let mut done = 0;
    while done < buf.len() {
        match segment.file().read_at(&mut buf[done..], at + done as u64)? {
            0 => return Err(Error::Format),
            n => done += n,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::tree_actor::TreeId;
    use crate::tree_actor::test_support::{ATTRS, new_actor, writer};

    /// The reader finds an open's calls in record order, with their bytes,
    /// across a segment roll and a compaction that carried them, and none of
    /// another open's.
    #[test]
    fn reads_an_opens_calls_in_order_wherever_they_are() {
        let tmp = tempfile::tempdir().unwrap();
        let mut v = new_actor(tmp.path());
        let h = v
            .write_open(TreeId::Root, "a", &writer("w"), Some(ATTRS))
            .unwrap();
        let other = v
            .write_open(TreeId::Root, "b", &writer("w"), Some(ATTRS))
            .unwrap();
        v.write(h, 0, b"one", false).unwrap();
        v.write(other, 0, b"not mine", false).unwrap();
        v.write(h, 3, b"two", true).unwrap();
        v.truncate(h, 4).unwrap();
        // Steps after the calls, built, then a compaction: the calls are
        // live, kept or carried.
        for n in 0..3 {
            let f = v
                .write_open(TreeId::Root, &format!("f{n}"), &writer("w"), Some(ATTRS))
                .unwrap();
            v.write(f, 0, b"x", false).unwrap();
            v.close(f).unwrap();
        }
        v.settle().unwrap();
        v.compact().unwrap();
        v.write(h, 4, b"three", false).unwrap();
        let calls = v.journal.call_reader().calls(h).unwrap();
        let seen: Vec<(super::Call, Vec<u8>)> = calls
            .iter()
            .map(|(_, call, pos)| (call.clone(), pos.bytes(0, call.carried()).unwrap()))
            .collect();
        assert_eq!(
            seen,
            vec![
                (super::Call::Write { offset: 0, len: 3 }, b"one".to_vec()),
                (super::Call::Append { len: 3 }, b"two".to_vec()),
                (super::Call::Truncate(4), Vec::new()),
                (super::Call::Write { offset: 4, len: 5 }, b"three".to_vec()),
            ]
        );
        assert!(calls.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(calls.iter().all(|(seq, _, _)| *seq > h));
    }
}
