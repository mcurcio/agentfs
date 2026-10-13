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
//! it journaled at each path ([`StampBook`]) and skips a write whose stamp
//! is older ([`crate::rules::SkipReason::Superseded`]): the newer state is
//! already journaled, and the front that froze it recorded it.
//!
//! Stamps are ordered by change time (`ctime`), which every write, chmod,
//! link count change and rename of the inode moves forward, from the same
//! clock for every file of the filesystem; a replaced file (a rename over
//! the path) is ordered against its predecessor the same way. Taken before
//! the freeze, a stamp is never newer than the content frozen after it, so
//! a skip never drops a state newer than the one journaled.
//!
//! Two stamps with the same `ctime` (one tick of a coarse clock) cannot be
//! ordered by it. Identical stamps are both journaled, as before (the
//! racy-timestamp rule records a file twice on purpose, §3.1). Stamps that
//! tie on `ctime` but differ otherwise are compared with the backing file
//! now ([`StampBook::judge`] returns [`Verdict::Tie`]): the one the file
//! still has is the newer.
//!
//! Entries are kept for [`HORIZON`]: far longer than any request may take,
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
    /// The last journaled stamp has the same `ctime` but differs otherwise:
    /// ask the backing file which one it still is ([`resolve_tie`]).
    Tie(FreezeStamp),
}

/// The stamp of the last write journaled at each path.
#[derive(Default)]
pub struct StampBook {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    last: HashMap<RepoPath, (FreezeStamp, Instant)>,
    pruned: Option<Instant>,
}

impl StampBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Judges a write of `path` frozen with `stamp`. Only meaningful under
    /// the path's claim, with the note of every journaled write made under
    /// the same claim.
    pub fn judge(&self, path: &RepoPath, stamp: &FreezeStamp) -> Verdict {
        let inner = self.lock();
        let Some((last, _)) = inner.last.get(path) else {
            return Verdict::Journal;
        };
        judge(last, stamp)
    }

    /// Notes that a write of `path` with `stamp` was journaled. An older
    /// stamp than the one noted (an identical-`ctime` write journaled by a
    /// tie) does not replace it.
    pub fn note(&self, path: &RepoPath, stamp: FreezeStamp) {
        let now = Instant::now();
        let mut inner = self.lock();
        if inner
            .pruned
            .is_none_or(|p| now.duration_since(p) >= PRUNE_EVERY)
        {
            inner
                .last
                .retain(|_, (_, at)| now.duration_since(*at) < HORIZON);
            inner.pruned = Some(now);
        }
        match inner.last.get_mut(path) {
            Some((last, at)) if last.dev == stamp.dev && last.ctime_ns > stamp.ctime_ns => {
                *at = now;
            }
            Some(entry) => *entry = (stamp, now),
            None => {
                inner.last.insert(path.clone(), (stamp, now));
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

/// `incoming` against the `last` stamp journaled at its path.
fn judge(last: &FreezeStamp, incoming: &FreezeStamp) -> Verdict {
    // Another filesystem (the backing tree was remounted): no order.
    if last.dev != incoming.dev {
        return Verdict::Journal;
    }
    match incoming.ctime_ns.cmp(&last.ctime_ns) {
        std::cmp::Ordering::Greater => Verdict::Journal,
        std::cmp::Ordering::Less => Verdict::Superseded,
        std::cmp::Ordering::Equal if incoming == last => Verdict::Journal,
        std::cmp::Ordering::Equal => Verdict::Tie(*last),
    }
}

/// Settles a [`Verdict::Tie`] with `now`, the backing file's stamp at the
/// path (`None`: not a file any more). The file still being the journaled
/// state means the incoming one is older; anything else journals it, as
/// before stamps (the next boundary of whoever changed it records it).
pub fn resolve_tie(journaled: &FreezeStamp, now: Option<&FreezeStamp>) -> Verdict {
    match now {
        Some(now) if now == journaled => Verdict::Superseded,
        _ => Verdict::Journal,
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
        assert_eq!(book.judge(&a, &stamp(5, 100, 1)), Verdict::Journal);
        book.note(&a, stamp(5, 100, 1));
        assert_eq!(book.judge(&a, &stamp(5, 90, 1)), Verdict::Superseded);
        assert_eq!(book.judge(&a, &stamp(5, 110, 2)), Verdict::Journal);
        // Identical: the racy-timestamp re-record is journaled again.
        assert_eq!(book.judge(&a, &stamp(5, 100, 1)), Verdict::Journal);
        // Same ctime, different state: the backing file decides.
        assert_eq!(
            book.judge(&a, &stamp(5, 100, 9)),
            Verdict::Tie(stamp(5, 100, 1))
        );
        // A replacement (another inode) is ordered by ctime too.
        assert_eq!(book.judge(&a, &stamp(6, 99, 1)), Verdict::Superseded);
        assert_eq!(book.judge(&a, &stamp(6, 101, 1)), Verdict::Journal);
        // Another device: no order.
        let other_dev = FreezeStamp {
            dev: 2,
            ..stamp(5, 1, 1)
        };
        assert_eq!(book.judge(&a, &other_dev), Verdict::Journal);
        // Other paths are independent.
        assert_eq!(book.judge(&path("b"), &stamp(5, 1, 1)), Verdict::Journal);
    }

    #[test]
    fn an_older_note_does_not_replace_a_newer_one() {
        let book = StampBook::new();
        let a = path("a");
        book.note(&a, stamp(5, 200, 1));
        book.note(&a, stamp(5, 100, 1));
        assert_eq!(book.judge(&a, &stamp(5, 150, 1)), Verdict::Superseded);
        book.note(&a, stamp(5, 300, 1));
        assert_eq!(book.judge(&a, &stamp(5, 250, 1)), Verdict::Superseded);
        assert_eq!(book.len(), 1);
    }

    #[test]
    fn ties_are_settled_by_the_backing_file() {
        let journaled = stamp(5, 100, 1);
        let incoming = stamp(5, 100, 2);
        assert_eq!(
            resolve_tie(&journaled, Some(&journaled)),
            Verdict::Superseded
        );
        assert_eq!(resolve_tie(&journaled, Some(&incoming)), Verdict::Journal);
        assert_eq!(resolve_tie(&journaled, None), Verdict::Journal);
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
