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
//! twice changes nothing). A duty that fails [`MAX_FAILURES`] times
//! raises [`crate::events::Event::ReconcileFailed`] and stops holding
//! commits back.

use std::collections::BTreeMap;
use std::fs::Metadata;
use std::io;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::paths::{RepoPath, is_dotgit};
use crate::record::{Caller, Origin};

/// How far before a move a file's change makes reconciliation compare
/// its stamp with the last one journaled: well past any request timeout
/// of a front, so a write in flight across the rename is covered.
pub const RECENT: Duration = Duration::from_secs(60);

/// How many failed attempts a duty gets before it is given up.
pub const MAX_FAILURES: u32 = 5;

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
    open: BTreeMap<u64, (Duty, u32)>,
    /// Reconciled moves not yet committed: the move's seq and the last
    /// record of its reconciliation.
    done: BTreeMap<u64, u64>,
}

/// Holds the duties while a move is journaled, so the commit loop never
/// sees a move without its duty.
pub struct Adding<'a>(MutexGuard<'a, Inner>);

impl Adding<'_> {
    pub fn add(&mut self, duty: Duty) {
        self.0.open.insert(duty.seq, (duty, 0));
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

    /// An attempt at the move `seq` failed; returns how many have.
    pub fn failed(&self, seq: u64) -> u32 {
        let mut inner = self.lock();
        match inner.open.get_mut(&seq) {
            Some((_, failures)) => {
                *failures += 1;
                *failures
            }
            None => 0,
        }
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
pub fn leaves(tree: &Path, dir: &RepoPath) -> io::Result<Vec<(RepoPath, Metadata)>> {
    let mut out = Vec::new();
    // Every component must be a real directory (the last may be a leaf).
    let mut at = tree.to_path_buf();
    let parts: Vec<&str> = dir.components().collect();
    let mut meta = None;
    for (i, part) in parts.iter().enumerate() {
        at.push(part);
        let m = match std::fs::symlink_metadata(&at) {
            Ok(m) => m,
            Err(e) if is_gone(&e) => return Ok(out),
            Err(e) => return Err(e),
        };
        if i + 1 < parts.len() && !m.is_dir() {
            return Ok(out);
        }
        meta = Some(m);
    }
    let Some(meta) = meta else {
        return Ok(out);
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
        let entries = match std::fs::read_dir(tree.join(d.as_str())) {
            Ok(entries) => entries,
            // Gone or replaced meanwhile: whoever did that records it.
            Err(e) if is_gone(&e) => continue,
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let Ok(path) = RepoPath::new(format!("{d}/{name}")) else {
                continue;
            };
            let m = match std::fs::symlink_metadata(entry.path()) {
                Ok(m) => m,
                Err(e) if is_gone(&e) => continue,
                Err(e) => return Err(e),
            };
            if m.is_dir() {
                if !is_dotgit(&name) {
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

fn is_gone(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
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
        assert_eq!(d.failed(5), 1);
        assert_eq!(d.failed(5), 2);
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
