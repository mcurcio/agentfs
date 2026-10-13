//! Whether an inode changed since it was last recorded (design §3.1:
//! "FLUSH records only if the inode changed").
//!
//! With kernel passthrough the daemon never sees writes, so change is
//! detected from the inode itself. An inode is *armed* once something could
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
//! RELEASE follows the last close at once, so a file written and closed
//! within the window is recorded twice (design §19.2 item 4): RELEASE is
//! the only backstop for a same-tick write through a shared mapping.

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
            mtime_ns: nanos(i64::from(st.st_mtime), u64::from(st.st_mtime_nsec)),
            ctime_ns: nanos(i64::from(st.st_ctime), u64::from(st.st_ctime_nsec)),
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
}

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
    fn granularity_is_sane() {
        let g = timestamp_granularity_ns();
        assert!((1_000_000..=1_000_000_000).contains(&g), "{g}");
        assert!(now_ns() > 1_600_000_000 * 1_000_000_000);
    }
}
