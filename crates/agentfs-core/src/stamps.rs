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
//!
//! [`LastWrites`] is the other per-path memory of whoever journals: the
//! frozen copy of the last write at each recently written path, which
//! lets a RELEASE skip re-recording a file whose close was just recorded
//! with the same bytes (design §3.1).

use std::collections::{BTreeMap, HashMap};
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::paths::RepoPath;
use crate::pending::PendingId;

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

/// How long [`LastWrites`] remembers a path's last frozen copy: RELEASE
/// follows the last close at once, so far longer than it ever waits.
pub const LAST_WRITE_HORIZON: Duration = Duration::from_secs(60);

/// The most paths [`LastWrites`] holds; past it, it forgets everything
/// (a forgotten path only costs one record more).
const LAST_WRITE_CAP: usize = 1 << 16;

/// The frozen copy of the last change journaled at each recently written
/// path (design §3.1): what lets RELEASE skip re-recording a file whose
/// close was just recorded, when the bytes it holds are that copy's. Kept
/// by whoever journals (a single-process daemon, or the history service
/// for its fronts); any later change at the path, or above or below it,
/// forgets it, so [`LastWrites::is_last`] is true only while the path's
/// last journaled state is that copy. Forgetting is always safe: the
/// caller then records again.
#[derive(Default)]
pub struct LastWrites {
    inner: Mutex<LastInner>,
}

#[derive(Default)]
struct LastInner {
    last: BTreeMap<RepoPath, (PendingId, Instant)>,
    pruned: Option<Instant>,
}

impl LastWrites {
    pub fn new() -> Self {
        Self::default()
    }

    /// A change at `path` was journaled: `Some(copy)` for a write of that
    /// frozen copy, `None` for anything else (a symlink, a removal).
    /// Entries above and below `path` are forgotten: their paths changed
    /// too.
    pub fn journaled(&self, path: &RepoPath, copy: Option<PendingId>) {
        let now = Instant::now();
        let mut inner = self.lock();
        inner.prune(now);
        inner.forget_around(path);
        if let Some(copy) = copy {
            inner.last.insert(path.clone(), (copy, now));
        }
    }

    /// Whether the last change journaled at `path` is the write of `copy`.
    pub fn is_last(&self, path: &RepoPath, copy: PendingId) -> bool {
        self.lock()
            .last
            .get(path)
            .is_some_and(|(c, at)| *c == copy && at.elapsed() < LAST_WRITE_HORIZON)
    }

    /// How many paths are held (for tests).
    pub fn len(&self) -> usize {
        self.lock().last.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LastInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl LastInner {
    fn prune(&mut self, now: Instant) {
        if self
            .pruned
            .is_none_or(|p| now.duration_since(p) >= PRUNE_EVERY)
        {
            self.last
                .retain(|_, (_, at)| now.duration_since(*at) < LAST_WRITE_HORIZON);
            self.pruned = Some(now);
        }
        if self.last.len() >= LAST_WRITE_CAP {
            self.last.clear();
        }
    }

    /// Forgets `path`, its ancestors and everything below it.
    pub(crate) fn forget_around(&mut self, path: &RepoPath) {
        let s = path.as_str();
        self.last.remove(path);
        for (i, b) in s.bytes().enumerate() {
            if b == b'/'
                && let Ok(ancestor) = RepoPath::new(&s[..i])
            {
                self.last.remove(&ancestor);
            }
        }
        let below: Vec<RepoPath> = self
            .last
            .range((
                std::ops::Bound::Excluded(path.clone()),
                std::ops::Bound::Unbounded,
            ))
            .map(|(p, _)| p)
            .take_while(|p| p.as_str().starts_with(s))
            .filter(|p| p.as_str().as_bytes().get(s.len()) == Some(&b'/'))
            .cloned()
            .collect();
        for p in below {
            self.last.remove(&p);
        }
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

    #[test]
    fn last_writes_forget_any_later_change_around_a_path() {
        let last = LastWrites::new();
        let p = |s: &str| RepoPath::new(s).unwrap();
        let (a, b, c) = (PendingId(1), PendingId(2), PendingId(3));
        last.journaled(&p("d/f"), Some(a));
        assert!(last.is_last(&p("d/f"), a));
        assert!(!last.is_last(&p("d/f"), b), "another copy");
        assert!(!last.is_last(&p("d/g"), a), "another path");
        // A later write of the path replaces it.
        last.journaled(&p("d/f"), Some(b));
        assert!(!last.is_last(&p("d/f"), a));
        assert!(last.is_last(&p("d/f"), b));
        // A removal forgets it.
        last.journaled(&p("d/f"), None);
        assert!(!last.is_last(&p("d/f"), b));
        // A change of an ancestor forgets everything below it, but not
        // siblings that only share a prefix.
        last.journaled(&p("d/f"), Some(a));
        last.journaled(&p("d-x"), Some(c));
        last.journaled(&p("d"), None);
        assert!(!last.is_last(&p("d/f"), a));
        assert!(last.is_last(&p("d-x"), c));
        // A change below a path forgets the path.
        last.journaled(&p("e"), Some(a));
        last.journaled(&p("e/f"), Some(b));
        assert!(!last.is_last(&p("e"), a));
        assert!(last.is_last(&p("e/f"), b));
        assert_eq!(last.len(), 2);
    }
}
