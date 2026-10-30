//! Whether an inode changed since it was last recorded (design §3.1:
//! "FLUSH records only if the inode changed").
//!
//! The daemon does not see every write (writes through a shared mapping
//! reach it only at writeback, and a writer sharing a passthrough backing
//! id not at all: design §17.1), so change is detected from the inode
//! itself. An inode is *armed* once something could
//! have changed it: a write-open, a write the daemon served, or a size
//! change. Only an armed inode is `fstat`ed at FLUSH; its [`Stamp`] (device,
//! inode, size, mtime, ctime, mode) is compared with the stamp taken just
//! before the last freeze.
//!
//! Timestamps come from a coarse clock, so two writes of the same size
//! within one tick leave the stamp unchanged. git's racy-timestamp rule
//! closes that gap: when the last record's stamp is within the clock's
//! granularity of the record time, a write could have followed the freeze
//! without moving the stamp, so the next FLUSH or RELEASE records regardless.
//! RELEASE comes once every handle is closed and every mapping made through
//! a daemon handle is gone; that is every writer's, except a writer sharing
//! a passthrough backing id, whose mapping does not hold the FUSE file
//! open, so its node stays armed until FORGET (design §3.1, §17.1).
//! RELEASE is the only backstop for a same-tick write through a shared
//! mapping, and for a file without one it follows the last close at once,
//! so a file written and closed within the window would be recorded twice. Instead, a racy record pins
//! its frozen copy ([`FrozenRef`]); a boundary that would record again only
//! because of the racy rule compares the file's bytes with that copy, and
//! skips the record if they are equal and the copy is still the last state
//! journaled at the path ([`DirtyState::racy_only`], design §3.1).

use std::fs::File;
use std::sync::Arc;

use agentfs_core::paths::RepoPath;
use agentfs_core::pending::PendingId;
use agentfs_core::record::Caller;
use rustix::fs::Stat;

/// The parts of `stat` that change when content or the executable bit do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub dev: u64,
    pub ino: u64,
    pub size: i64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub mode: u32,
}

impl Stamp {
    pub fn from_stat(st: &Stat) -> Self {
        Self {
            dev: u64::from(st.st_dev),
            ino: u64::from(st.st_ino),
            size: i64::from(st.st_size),
            mtime_ns: coarse(nanos(i64::from(st.st_mtime), u64::from(st.st_mtime_nsec))),
            ctime_ns: coarse(nanos(i64::from(st.st_ctime), u64::from(st.st_ctime_nsec))),
            mode: u32::from(st.st_mode),
        }
    }

    /// The later of mtime and ctime.
    pub fn changed_ns(&self) -> i128 {
        self.mtime_ns.max(self.ctime_ns)
    }
}

fn nanos(secs: i64, nsecs: u64) -> i128 {
    i128::from(secs) * 1_000_000_000 + i128::from(nsecs)
}

#[cfg(test)]
thread_local! {
    /// A coarser timestamp clock for this thread's stamps (tests only): every
    /// change within one tick of it leaves the stamp unchanged, as the
    /// kernel's coarse clock does to writes within one of its ticks.
    pub static TEST_TICK_NS: std::cell::Cell<i128> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn coarse(ns: i128) -> i128 {
    match TEST_TICK_NS.with(std::cell::Cell::get) {
        0 => ns,
        tick => ns - ns.rem_euclid(tick),
    }
}

#[cfg(not(test))]
fn coarse(ns: i128) -> i128 {
    ns
}

/// One inode's recording state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirtyState {
    /// The stamp taken just before the last freeze, or at the first
    /// write-open when the inode had not been recorded yet.
    pub baseline: Option<Stamp>,
    /// Record at the next boundary even if the stamp did not move: the file
    /// was created (an empty `touch` is a change), or truncated at open.
    pub force: bool,
    /// The last record's stamp was within the clock granularity of the
    /// record time (the racy-timestamp rule).
    pub racy: bool,
    /// Something may have changed the inode since it was last recorded, so
    /// boundaries must look at it. Unarmed inodes cost no `fstat`.
    pub armed: bool,
    /// The last process that opened the inode for writing or wrote to it:
    /// the author of late records (RELEASE, sweep), whose requests carry no
    /// useful caller.
    pub last_writer: Option<Caller>,
    /// The frozen copy of the last record, pinned open while that record
    /// is racy (design §3.1).
    pub frozen: Option<FrozenRef>,
}

/// The frozen copy a racy record journaled, held open so it outlives its
/// removal at commit.
#[derive(Debug, Clone)]
pub struct FrozenRef {
    /// The one name it was recorded at, as the boundary named it.
    pub name: Vec<u8>,
    pub path: RepoPath,
    pub copy: PendingId,
    pub file: Arc<File>,
}

impl PartialEq for FrozenRef {
    fn eq(&self, other: &Self) -> bool {
        self.copy == other.copy && self.name == other.name
    }
}

impl Eq for FrozenRef {}

impl DirtyState {
    /// Something may change the inode from now on (a write-open, a write,
    /// a size change).
    pub fn arm(&mut self, writer: Caller) {
        self.armed = true;
        self.last_writer = Some(writer);
    }

    /// Whether a boundary must record the inode, given its stamp now. Only
    /// meaningful when [`DirtyState::armed`]; callers skip the `fstat` of
    /// an unarmed inode.
    pub fn needs_record(&self, current: &Stamp) -> bool {
        if !self.armed {
            return false;
        }
        if self.force {
            return true;
        }
        match &self.baseline {
            None => true,
            Some(baseline) => baseline != current || self.racy,
        }
    }

    /// The inode was recorded from content frozen after `stamp` was taken,
    /// at `now_ns` (CLOCK_REALTIME). `granularity_ns` is how coarse file
    /// timestamps are.
    pub fn after_record(&mut self, stamp: Stamp, now_ns: i128, granularity_ns: i128) {
        self.baseline = Some(stamp);
        self.force = false;
        self.racy = now_ns - stamp.changed_ns() < granularity_ns;
    }

    /// Whether a boundary that [`DirtyState::needs_record`] needs it only
    /// because of the racy-timestamp rule: the stamp is the last record's,
    /// nothing forces a record. Only then may the record be skipped when
    /// the bytes are proven to be the pinned copy's (design §3.1).
    pub fn racy_only(&self, current: &Stamp) -> bool {
        self.armed && !self.force && self.racy && self.baseline.as_ref() == Some(current)
    }
}

/// Whether the first `len` bytes of `a` and `b` are equal, and neither
/// ends early.
pub fn same_bytes(a: &File, b: &File, len: u64) -> std::io::Result<bool> {
    use std::os::unix::fs::FileExt;
    const CHUNK: usize = 1 << 20;
    let mut x = vec![0u8; CHUNK.min(len as usize).max(1)];
    let mut y = vec![0u8; x.len()];
    let mut at = 0u64;
    while at < len {
        let want = x.len().min((len - at) as usize);
        let (xs, ys) = (&mut x[..want], &mut y[..want]);
        a.read_exact_at(xs, at)?;
        b.read_exact_at(ys, at)?;
        if xs != ys {
            return Ok(false);
        }
        at += want as u64;
    }
    Ok(true)
}

/// Twice the resolution of the coarse realtime clock, which is what file
/// timestamps are taken from: the window in which a write may not move
/// them. At least 1 ms.
pub fn timestamp_granularity_ns() -> i128 {
    let res = rustix::time::clock_getres(rustix::time::ClockId::RealtimeCoarse);
    let ns = i128::from(res.tv_sec) * 1_000_000_000 + i128::from(res.tv_nsec);
    (2 * ns).max(1_000_000)
}

/// Now, in nanoseconds since the epoch (CLOCK_REALTIME, the clock file
/// timestamps follow).
pub fn now_ns() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i128)
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Caller = Caller {
        uid: 1,
        gid: 1,
        pid: 1,
    };

    fn stamp(size: i64, mtime_ns: i128) -> Stamp {
        Stamp {
            dev: 1,
            ino: 2,
            size,
            mtime_ns,
            ctime_ns: mtime_ns,
            mode: 0o100644,
        }
    }

    const GRAN: i128 = 8_000_000;

    #[test]
    fn unarmed_never_records() {
        let d = DirtyState::default();
        assert!(!d.needs_record(&stamp(1, 1)));
        let d = DirtyState {
            force: true,
            ..DirtyState::default()
        };
        assert!(!d.needs_record(&stamp(1, 1)), "force needs arming too");
    }

    #[test]
    fn armed_without_baseline_records() {
        let mut d = DirtyState::default();
        d.arm(W);
        assert!(d.needs_record(&stamp(0, 0)));
        assert_eq!(d.last_writer, Some(W));
    }

    #[test]
    fn a_changed_stamp_records_an_equal_one_does_not() {
        let mut d = DirtyState::default();
        d.arm(W);
        let s = stamp(3, 1_000_000_000);
        d.after_record(s, 2_000_000_000, GRAN);
        assert!(!d.racy);
        assert!(!d.needs_record(&s));
        assert!(d.needs_record(&stamp(4, 1_000_000_000)), "size");
        assert!(d.needs_record(&stamp(3, 1_500_000_000)), "mtime");
        let mut ctime = s;
        ctime.ctime_ns += 1;
        assert!(d.needs_record(&ctime), "ctime");
        let mut mode = s;
        mode.mode |= 0o111;
        assert!(d.needs_record(&mode), "mode");
    }

    #[test]
    fn racy_stamps_record_once_more() {
        let mut d = DirtyState::default();
        d.arm(W);
        let s = stamp(3, 1_000_000_000);
        // Recorded 1 ms after the last change: within the granularity.
        d.after_record(s, 1_001_000_000, GRAN);
        assert!(d.racy);
        assert!(d.needs_record(&s), "an equal stamp still records");
        // Recorded again later: no longer racy.
        d.after_record(s, 1_100_000_000, GRAN);
        assert!(!d.racy);
        assert!(!d.needs_record(&s));
    }

    #[test]
    fn force_records_even_unchanged_and_clears() {
        let mut d = DirtyState {
            force: true,
            ..DirtyState::default()
        };
        d.arm(W);
        let s = stamp(0, 1_000_000_000);
        d.baseline = Some(s);
        assert!(d.needs_record(&s), "an empty created file is recorded");
        d.after_record(s, 2_000_000_000, GRAN);
        assert!(!d.force);
        assert!(!d.needs_record(&s));
    }

    #[test]
    fn racy_only_needs_racy_unforced_and_unchanged() {
        let mut d = DirtyState::default();
        d.arm(W);
        let s = stamp(3, 1_000_000_000);
        d.after_record(s, 1_001_000_000, GRAN);
        assert!(d.racy_only(&s));
        assert!(!d.racy_only(&stamp(4, 1_000_000_000)), "changed");
        d.force = true;
        assert!(!d.racy_only(&s), "forced");
        d.force = false;
        d.after_record(s, 2_000_000_000, GRAN);
        assert!(!d.racy_only(&s), "not racy");
    }

    #[test]
    fn same_bytes_compares_every_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, data: &[u8]| {
            let p = dir.path().join(name);
            std::fs::write(&p, data).unwrap();
            File::open(p).unwrap()
        };
        let big: Vec<u8> = (0..(3 << 20) + 7).map(|i| (i % 251) as u8).collect();
        let mut other = big.clone();
        *other.last_mut().unwrap() ^= 1;
        let (a, b, c) = (write("a", &big), write("b", &big), write("c", &other));
        let len = big.len() as u64;
        assert!(same_bytes(&a, &b, len).unwrap());
        assert!(!same_bytes(&a, &c, len).unwrap(), "the last byte differs");
        assert!(same_bytes(&a, &c, len - 1).unwrap());
        assert!(same_bytes(&a, &b, 0).unwrap());
        let short = write("d", &big[..10]);
        assert!(same_bytes(&a, &short, len).is_err(), "ends early");
    }

    #[test]
    fn granularity_is_sane() {
        let g = timestamp_granularity_ns();
        assert!((1_000_000..=1_000_000_000).contains(&g), "{g}");
        assert!(now_ns() > 1_600_000_000 * 1_000_000_000);
    }
}
