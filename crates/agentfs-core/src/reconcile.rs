//! Reconciling a moved tree (design §3.1).
//!
//! A directory rename is journaled as one [`crate::record::Change::Move`]:
//! history carries whatever it holds beneath the source to the
//! destination, unchanged, in O(1) during the synchronous step. What that
//! leaves out is what recording every file under the destination (as a
//! rename was recorded before) would have decided differently:
//!
//! - files under the destination that were not tracked at the source (an
//!   ignored directory renamed to where nothing ignores it) and are
//!   recorded at their new path;
//! - moved files the rules exclude at their new path (renamed into an
//!   ignored directory or a nested repository), which are dropped;
//! - files whose journaled state is not the one they have, because a
//!   write raced the rename: another front's write skipped as gone, or a
//!   change made outside the mount. Only files changed within [`RECENT`]
//!   of the move are looked at, and only those whose freeze stamp is not
//!   the one last journaled at the path ([`crate::stamps::LastWrites`]).
//!
//! Each move leaves a [`Duty`]. The commit loop carries them out before
//! each commit pass and commits a move only together with the records
//! that reconcile it: records are committed only below the oldest duty
//! still open, and a move's group extends through its reconciliation's
//! last record when both are one origin's. Duties are rebuilt at open
//! from the uncommitted journal, so a crash between the move and its
//! reconciliation redoes it (the records are absolute state, so doing it
//! twice changes nothing). A file that vanishes between the walk and its
//! record is skipped, not a failure: whoever removed it records that. A
//! duty that fails anyway (an I/O error) is retried with a backoff
//! ([`BACKOFF_FIRST`] doubling to [`BACKOFF_MAX`]); once it has failed
//! [`MAX_FAILURES`] times over at least [`GIVE_UP_AFTER`] it raises
//! [`crate::events::Event::ReconcileFailed`] and stops holding commits
//! back.

use std::collections::BTreeMap;
use std::fs::Metadata;
use std::io;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags};

use crate::confined;
use crate::paths::{RepoPath, is_dotgit};
use crate::record::{Caller, Origin};

/// How far before a move a file's change makes reconciliation compare
/// its stamp with the last one journaled: well past any request timeout
/// of a front, so a write in flight across the rename is covered.
pub const RECENT: Duration = Duration::from_secs(60);

/// How many failed attempts a duty gets before it is given up, at the
/// least ([`GIVE_UP_AFTER`]).
pub const MAX_FAILURES: u32 = 5;

/// How long a duty keeps being retried after its first failure before it
/// is given up, at the least ([`MAX_FAILURES`]): long enough for a burst
/// of changes under the destination (a build still writing) to settle.
pub const GIVE_UP_AFTER: Duration = Duration::from_secs(30);

/// The wait before retrying a failed duty: doubled per failure from
/// [`BACKOFF_FIRST`], at most [`BACKOFF_MAX`].
pub const BACKOFF_FIRST: Duration = Duration::from_millis(500);
pub const BACKOFF_MAX: Duration = Duration::from_secs(8);

/// Most changes per reconciling record.
pub const CHUNK: usize = 4096;

/// A move journaled and not yet reconciled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Duty {
    /// The move's record.
    pub seq: u64,
    pub from: RepoPath,
    pub to: RepoPath,
    /// Who made the move: the author of the reconciling records.
    pub caller: Caller,
    /// The front that made it, journaled with the reconciling records too.
    pub origin: Option<Origin>,
    /// When it was journaled, ms since the epoch.
    pub at_ms: u64,
}

/// The duties of one journal. Safe to share across threads.
#[derive(Debug, Default)]
pub struct Duties {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Open duties by the move's seq, with their failed attempts.
    open: BTreeMap<u64, (Duty, Attempts)>,
    /// Reconciled moves not yet committed: the move's seq and the last
    /// record of its reconciliation.
    done: BTreeMap<u64, u64>,
}

/// A duty's failed attempts.
#[derive(Debug, Default, Clone, Copy)]
struct Attempts {
    failures: u32,
    first_failed: Option<Instant>,
    retry_at: Option<Instant>,
}

/// Holds the duties while a move is journaled, so the commit loop never
/// sees a move without its duty.
pub struct Adding<'a>(MutexGuard<'a, Inner>);

impl Adding<'_> {
    pub fn add(&mut self, duty: Duty) {
        self.0.open.insert(duty.seq, (duty, Attempts::default()));
    }
}

impl Duties {
    pub fn new() -> Self {
        Self::default()
    }

    /// Locks the duties for journaling a move: take it before the record
    /// is appended and [`Adding::add`] its duty once the record has a seq.
    pub fn adding(&self) -> Adding<'_> {
        Adding(self.lock())
    }

    /// Adds a duty found at open.
    pub fn add(&self, duty: Duty) {
        self.adding().add(duty);
    }

    /// The open duties, oldest first.
    pub fn open(&self) -> Vec<Duty> {
        self.lock().open.values().map(|(d, _)| d.clone()).collect()
    }

    /// The oldest move still unreconciled: nothing from it on may be
    /// committed.
    pub fn gate(&self) -> Option<u64> {
        self.lock().open.keys().next().copied()
    }

    /// The move `seq` is reconciled, its last record `last` (`None`: it
    /// needed none, or was given up).
    pub fn finished(&self, seq: u64, last: Option<u64>) {
        let mut inner = self.lock();
        inner.open.remove(&seq);
        if let Some(last) = last {
            inner.done.insert(seq, last);
        }
    }

    /// An attempt at the move `seq` failed at `now`: it is retried after a
    /// backoff ([`Duties::due`]). Returns how many attempts have failed
    /// and whether it is time to give up: [`MAX_FAILURES`] failures over
    /// at least [`GIVE_UP_AFTER`].
    pub fn failed(&self, seq: u64, now: Instant) -> (u32, bool) {
        let mut inner = self.lock();
        let Some((_, attempts)) = inner.open.get_mut(&seq) else {
            return (0, false);
        };
        attempts.failures += 1;
        let first = *attempts.first_failed.get_or_insert(now);
        let shift = attempts.failures.saturating_sub(1).min(16);
        let wait = BACKOFF_FIRST.saturating_mul(1 << shift).min(BACKOFF_MAX);
        attempts.retry_at = Some(now + wait);
        let give_up =
            attempts.failures >= MAX_FAILURES && now.duration_since(first) >= GIVE_UP_AFTER;
        (attempts.failures, give_up)
    }

    /// Whether the move `seq` may be attempted at `now`: not while it backs
    /// off after a failure.
    pub fn due(&self, seq: u64, now: Instant) -> bool {
        self.lock()
            .open
            .get(&seq)
            .and_then(|(_, a)| a.retry_at)
            .is_none_or(|at| now >= at)
    }

    /// The last record reconciling the move `seq`, if it needed any.
    pub fn reconciled_through(&self, seq: u64) -> Option<u64> {
        self.lock().done.get(&seq).copied()
    }

    /// Everything through `seq` is in history.
    pub fn committed_through(&self, seq: u64) {
        self.lock().done.retain(|&m, _| m > seq);
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Every regular file and symlink at or beneath `dir` in the backing tree
/// at `tree`, with its metadata, never inside a `.git` directory, never
/// through a symlink. Names that are not repository paths (not UTF-8) are
/// left out. Nothing if `dir` is gone, or not under real directories.
///
/// The workload may change the tree throughout: every directory is
/// opened with `openat2` beneath the root, never through a symlink
/// ([`confined::Dir`]), and every entry is examined (`lstat`) by its one
/// name in its opened directory. Nothing is opened for reading: the
/// records that follow open each leaf confined again
/// ([`crate::volume::Input::Tree`]).
pub fn leaves(tree: &Path, dir: &RepoPath) -> io::Result<Vec<(RepoPath, Metadata)>> {
    let root = confined::Dir::open(tree)?;
    let mut out = Vec::new();
    let (parent, name) = match dir.as_str().rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", dir.as_str()),
    };
    let parent_fd = match root.open_at(parent, OFlags::PATH | OFlags::DIRECTORY, Mode::empty()) {
        Ok(fd) => fd,
        Err(e) if confined::is_gone(&e) => return Ok(out),
        Err(e) => return Err(e),
    };
    let meta = match lstat_in(&parent_fd, name) {
        Ok(m) => m,
        Err(e) if confined::is_gone(&e) => return Ok(out),
        Err(e) => return Err(e),
    };
    if meta.is_file() || meta.is_symlink() {
        out.push((dir.clone(), meta));
        return Ok(out);
    }
    if !meta.is_dir() {
        return Ok(out);
    }
    let mut stack = vec![dir.clone()];
    while let Some(d) = stack.pop() {
        let fd = match root.open_at(
            d.as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            // Gone or replaced meanwhile: whoever did that records it.
            Err(e) if confined::is_gone(&e) => continue,
            Err(e) => return Err(e),
        };
        for entry in rustix::fs::Dir::read_from(&fd)? {
            let entry = entry?;
            let Ok(name) = std::str::from_utf8(entry.file_name().to_bytes()) else {
                continue;
            };
            if name == "." || name == ".." {
                continue;
            }
            let Ok(path) = RepoPath::new(format!("{d}/{name}")) else {
                continue;
            };
            let m = match lstat_in(&fd, name) {
                Ok(m) => m,
                Err(e) if confined::is_gone(&e) => continue,
                Err(e) => return Err(e),
            };
            if m.is_dir() {
                if !is_dotgit(name) {
                    stack.push(path);
                }
            } else if m.is_file() || m.is_symlink() {
                out.push((path, m));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// The status of `name` in the opened directory `dir`, never following a
/// final symlink: `lstat` of one name, through the directory's
/// `/proc/self/fd` link, so no path beneath the root is resolved again.
fn lstat_in(dir: &OwnedFd, name: &str) -> io::Result<Metadata> {
    std::fs::symlink_metadata(confined::proc_path(dir).join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn duty(seq: u64) -> Duty {
        Duty {
            seq,
            from: p("a"),
            to: p("b"),
            caller: Caller {
                uid: 1,
                gid: 1,
                pid: 1,
            },
            origin: None,
            at_ms: 0,
        }
    }

    #[test]
    fn duties_gate_commits_until_reconciled() {
        let d = Duties::new();
        assert_eq!(d.gate(), None);
        d.adding().add(duty(5));
        d.add(duty(9));
        assert_eq!(d.gate(), Some(5));
        assert_eq!(d.open().len(), 2);
        let t0 = Instant::now();
        assert!(d.due(5, t0));
        assert_eq!(d.failed(5, t0), (1, false));
        assert!(!d.due(5, t0), "backing off");
        assert!(d.due(5, t0 + BACKOFF_FIRST));
        assert_eq!(d.failed(5, t0 + BACKOFF_FIRST), (2, false));
        // Five failures in quick succession are not enough to give up.
        for n in 3..=MAX_FAILURES {
            assert_eq!(d.failed(5, t0 + BACKOFF_FIRST), (n, false));
        }
        // Only once they span GIVE_UP_AFTER.
        assert_eq!(d.failed(5, t0 + GIVE_UP_AFTER), (MAX_FAILURES + 1, true));
        assert!(!d.due(5, t0 + GIVE_UP_AFTER), "capped backoff");
        assert!(d.due(5, t0 + GIVE_UP_AFTER + BACKOFF_MAX));
        d.finished(5, Some(7));
        assert_eq!(d.gate(), Some(9));
        assert_eq!(d.reconciled_through(5), Some(7));
        d.finished(9, None);
        assert_eq!(d.gate(), None);
        assert_eq!(d.reconciled_through(9), None);
        d.committed_through(7);
        assert_eq!(d.reconciled_through(5), None);
    }

    #[test]
    fn leaves_walk_real_directories_only() {
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path();
        std::fs::create_dir_all(t.join("m/sub/.git")).unwrap();
        std::fs::write(t.join("m/f"), "x").unwrap();
        std::fs::write(t.join("m/sub/g"), "y").unwrap();
        std::fs::write(t.join("m/sub/.git/HEAD"), "z").unwrap();
        std::os::unix::fs::symlink("f", t.join("m/l")).unwrap();
        std::os::unix::fs::symlink("m", t.join("via")).unwrap();
        let names = |d: &str| -> Vec<String> {
            leaves(t, &p(d))
                .unwrap()
                .into_iter()
                .map(|(p, _)| p.to_string())
                .collect()
        };
        assert_eq!(names("m"), ["m/f", "m/l", "m/sub/g"]);
        assert_eq!(names("m/f"), ["m/f"]);
        assert!(names("missing").is_empty());
        assert!(names("via/sub").is_empty(), "never through a symlink");
        assert_eq!(names("via"), ["via"], "a symlink is a leaf");
    }
}
