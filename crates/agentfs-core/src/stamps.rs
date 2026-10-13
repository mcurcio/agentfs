//! Per-path freeze stamps (design §14.1): ordering front records that
//! race for one path.
//!
//! A front freezes a file's content under its own path claim, but the
//! history service journals it under the service's: two fronts writing
//! one path at nearly the same instant can reach the service in the
//! opposite order to their freezes, and HEAD would then hold the earlier
//! state until that path's next boundary. Each front therefore sends, with
//! every write, the stat stamp of the backing file taken just before the
//! freeze ([`FreezeStamp`]). The service keeps the stamp of the last write
//! it journaled at each path, and the slot it came from ([`StampBook`]),
//! and skips a write whose stamp is older ([`crate::rules::SkipReason::Superseded`]):
//! the newer state is already journaled, and the front that froze it
//! recorded it.
//!
//! Stamps are ordered by change time (`ctime`), which every write, chmod,
//! link count change and rename of the inode moves forward, from the same
//! clock for every file of the filesystem; a replaced file (a rename over
//! the path) is ordered against its predecessor the same way. Taken before
//! the freeze, a stamp is never newer than the content frozen after it, so
//! a skip never drops a state newer than the one journaled.
//!
//! [`StampBook::judge`] also looks at the backing file now:
//!
//! - A write whose stamp is the file's current one is never skipped: it
//!   froze the file as it still is. This keeps a backward step of the
//!   wall clock (which `ctime` follows) from making every later write of
//!   a recently journaled path look older than it.
//! - Two stamps with the same `ctime` that differ otherwise are settled
//!   by the file: if it still has the journaled stamp, the incoming write
//!   is older.
//! - Identical stamps from one slot are a front's own re-record (the
//!   racy-timestamp rule records a file twice on purpose, §3.1), ordered
//!   by that front's claim: journaled. Identical stamps from two slots
//!   (two fronts' same-size writes within one tick of a coarse timestamp
//!   clock) cannot be ordered at all: the write is journaled, and the
//!   service then records the backing file as it is now
//!   ([`Verdict::Resync`]), under the path's claim, so HEAD ends on the
//!   tree's state whichever record arrived last.
//!
//! Entries count for [`HORIZON`]: far longer than any request may take,
//! so a record frozen before an entry was dropped has long timed out.

use std::collections::HashMap;
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::paths::RepoPath;

/// How long the book keeps a path's last stamp.
pub const HORIZON: Duration = Duration::from_secs(300);

/// How often, at most, [`StampBook::note`] drops old entries.
const PRUNE_EVERY: Duration = Duration::from_secs(30);

/// A backing file's stat stamp, taken just before its content was frozen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FreezeStamp {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl FreezeStamp {
    pub fn from_metadata(m: &Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
            mtime_ns: m.mtime().saturating_mul(1_000_000_000) + m.mtime_nsec(),
            ctime_ns: m.ctime().saturating_mul(1_000_000_000) + m.ctime_nsec(),
        }
    }
}

/// What the book says about a write at a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing newer is journaled: journal it.
    Journal,
    /// A newer state of the path is already journaled: skip it.
    Superseded,
    /// The journaled state cannot be ordered against this one (identical
    /// stamps from two slots): journal it, then record the backing file
    /// as it is now.
    Resync,
}

/// The stamp of the last write journaled at each path.
#[derive(Default)]
pub struct StampBook {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    last: HashMap<RepoPath, Entry>,
    pruned: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    stamp: FreezeStamp,
    /// The pending slot of the record that journaled it (0: the service).
    slot: u16,
    at: Instant,
}

impl StampBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Judges a write of `path` frozen with `stamp` by the front in
    /// `slot`, given `now`, the backing file's stamp at the path now
    /// (`None`: unknown, or not a file). Only meaningful under the path's
    /// claim, with the note of every journaled write made under the same
    /// claim.
    pub fn judge(
        &self,
        path: &RepoPath,
        stamp: &FreezeStamp,
        slot: u16,
        now: Option<&FreezeStamp>,
    ) -> Verdict {
        let inner = self.lock();
        match inner.last.get(path) {
            Some(entry) if entry.at.elapsed() < HORIZON => judge(entry, stamp, slot, now),
            _ => Verdict::Journal,
        }
    }

    /// Notes that a write of `path` with `stamp`, from `slot`, was
    /// journaled. With `current` (the stamp is the backing file's at the
    /// path now) it replaces whatever is noted; otherwise an older stamp
    /// than the one noted does not.
    pub fn note(&self, path: &RepoPath, stamp: FreezeStamp, slot: u16, current: bool) {
        let now = Instant::now();
        let mut inner = self.lock();
        if inner
            .pruned
            .is_none_or(|p| now.duration_since(p) >= PRUNE_EVERY)
        {
            inner.last.retain(|_, e| now.duration_since(e.at) < HORIZON);
            inner.pruned = Some(now);
        }
        let entry = Entry {
            stamp,
            slot,
            at: now,
        };
        match inner.last.get_mut(path) {
            Some(last)
                if !current
                    && last.stamp.dev == stamp.dev
                    && last.stamp.ctime_ns > stamp.ctime_ns
                    && now.duration_since(last.at) < HORIZON =>
            {
                last.at = now;
            }
            Some(last) => *last = entry,
            None => {
                inner.last.insert(path.clone(), entry);
            }
        }
    }

    /// How many paths the book holds (for tests).
    pub fn len(&self) -> usize {
        self.lock().last.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// `incoming`, from `slot`, against the `last` entry journaled at its
/// path, with `now` the backing file's stamp.
fn judge(last: &Entry, incoming: &FreezeStamp, slot: u16, now: Option<&FreezeStamp>) -> Verdict {
    // Another filesystem (the backing tree was remounted): no order.
    if last.stamp.dev != incoming.dev {
        return Verdict::Journal;
    }
    if *incoming == last.stamp {
        return if slot == last.slot {
            Verdict::Journal
        } else {
            Verdict::Resync
        };
    }
    // The file as it still is: whatever the clock did, nothing newer
    // exists.
    if now == Some(incoming) {
        return Verdict::Journal;
    }
    match incoming.ctime_ns.cmp(&last.stamp.ctime_ns) {
        std::cmp::Ordering::Greater => Verdict::Journal,
        std::cmp::Ordering::Less => Verdict::Superseded,
        // The same tick, another state: if the file still has the
        // journaled stamp, the incoming one is older; anything else
        // journals it, as before stamps (the next boundary of whoever
        // changed it records it).
        std::cmp::Ordering::Equal if now == Some(&last.stamp) => Verdict::Superseded,
        std::cmp::Ordering::Equal => Verdict::Journal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(ino: u64, ctime_ns: i64, size: u64) -> FreezeStamp {
        FreezeStamp {
            dev: 1,
            ino,
            size,
            mtime_ns: ctime_ns,
            ctime_ns,
        }
    }

    fn path(p: &str) -> RepoPath {
        RepoPath::new(p).unwrap()
    }

    #[test]
    fn newer_stamps_win_and_older_ones_are_superseded() {
        let book = StampBook::new();
        let a = path("a");
        assert_eq!(book.judge(&a, &stamp(5, 100, 1), 1, None), Verdict::Journal);
        book.note(&a, stamp(5, 100, 1), 1, false);
        assert_eq!(
            book.judge(&a, &stamp(5, 90, 1), 2, None),
            Verdict::Superseded
        );
        assert_eq!(book.judge(&a, &stamp(5, 110, 2), 2, None), Verdict::Journal);
        // A replacement (another inode) is ordered by ctime too.
        assert_eq!(
            book.judge(&a, &stamp(6, 99, 1), 2, None),
            Verdict::Superseded
        );
        assert_eq!(book.judge(&a, &stamp(6, 101, 1), 2, None), Verdict::Journal);
        // Another device: no order.
        let other_dev = FreezeStamp {
            dev: 2,
            ..stamp(5, 1, 1)
        };
        assert_eq!(book.judge(&a, &other_dev, 2, None), Verdict::Journal);
        // Other paths are independent.
        assert_eq!(
            book.judge(&path("b"), &stamp(5, 1, 1), 2, None),
            Verdict::Journal
        );
    }

    #[test]
    fn identical_stamps_from_another_slot_resync() {
        let book = StampBook::new();
        let a = path("a");
        book.note(&a, stamp(5, 100, 4), 1, false);
        // The same front's racy-timestamp re-record: its own claim
        // ordered it.
        assert_eq!(book.judge(&a, &stamp(5, 100, 4), 1, None), Verdict::Journal);
        // Another front (or the service) in the same tick, the same size:
        // no order, whatever the file says.
        for now in [None, Some(stamp(5, 100, 4)), Some(stamp(5, 200, 4))] {
            assert_eq!(
                book.judge(&a, &stamp(5, 100, 4), 2, now.as_ref()),
                Verdict::Resync
            );
            assert_eq!(
                book.judge(&a, &stamp(5, 100, 4), 0, now.as_ref()),
                Verdict::Resync
            );
        }
    }

    #[test]
    fn the_files_current_stamp_is_never_superseded() {
        // The wall clock stepped back after `a` was journaled at 1000: a
        // later write has an older ctime, but is the file as it is.
        let book = StampBook::new();
        let a = path("a");
        book.note(&a, stamp(5, 1000, 1), 1, false);
        let after_step = stamp(5, 400, 2);
        assert_eq!(
            book.judge(&a, &after_step, 1, Some(&after_step)),
            Verdict::Journal
        );
        // Journaled as the file's current state, it replaces the note.
        book.note(&a, after_step, 1, true);
        let next = stamp(5, 410, 3);
        assert_eq!(book.judge(&a, &next, 1, Some(&next)), Verdict::Journal);
        book.note(&a, next, 1, true);
        // A late record of a state between them is older.
        assert_eq!(
            book.judge(&a, &stamp(5, 405, 9), 2, Some(&next)),
            Verdict::Superseded
        );
        // Not current, an older stamp does not replace a newer note.
        book.note(&a, stamp(5, 300, 1), 2, false);
        assert_eq!(
            book.judge(&a, &stamp(5, 405, 9), 2, None),
            Verdict::Superseded
        );
    }

    #[test]
    fn entries_past_the_horizon_do_not_count() {
        let book = StampBook::new();
        let a = path("a");
        book.note(&a, stamp(5, 1000, 1), 1, false);
        book.lock().last.get_mut(&a).unwrap().at = Instant::now() - HORIZON;
        assert_eq!(book.judge(&a, &stamp(5, 10, 1), 2, None), Verdict::Journal);
        // And an expired entry is replaced by an older note.
        book.note(&a, stamp(5, 10, 1), 2, false);
        assert_eq!(
            book.judge(&a, &stamp(5, 5, 1), 1, None),
            Verdict::Superseded
        );
    }

    #[test]
    fn an_older_note_does_not_replace_a_newer_one() {
        let book = StampBook::new();
        let a = path("a");
        book.note(&a, stamp(5, 200, 1), 1, false);
        book.note(&a, stamp(5, 100, 1), 1, false);
        assert_eq!(
            book.judge(&a, &stamp(5, 150, 1), 1, None),
            Verdict::Superseded
        );
        book.note(&a, stamp(5, 300, 1), 1, false);
        assert_eq!(
            book.judge(&a, &stamp(5, 250, 1), 1, None),
            Verdict::Superseded
        );
        assert_eq!(book.len(), 1);
    }

    #[test]
    fn ties_are_settled_by_the_backing_file() {
        let book = StampBook::new();
        let a = path("a");
        let journaled = stamp(5, 100, 1);
        let incoming = stamp(5, 100, 2);
        book.note(&a, journaled, 1, false);
        assert_eq!(
            book.judge(&a, &incoming, 2, Some(&journaled)),
            Verdict::Superseded
        );
        assert_eq!(
            book.judge(&a, &incoming, 2, Some(&incoming)),
            Verdict::Journal
        );
        assert_eq!(book.judge(&a, &incoming, 2, None), Verdict::Journal);
    }

    #[test]
    fn stamps_come_from_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, "abc").unwrap();
        let m = std::fs::metadata(&file).unwrap();
        let s = FreezeStamp::from_metadata(&m);
        assert_eq!(s.size, 3);
        assert_eq!(s.ino, m.ino());
        assert!(s.ctime_ns > 0 && s.mtime_ns > 0);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<FreezeStamp>(&json).unwrap(), s);
    }
}
