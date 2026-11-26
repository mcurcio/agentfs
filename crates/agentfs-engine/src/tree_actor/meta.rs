//! The times each tree keeps for itself of an entry, which history never
//! records (docs/trees.md, "What a commit holds"); the metadata history does
//! record is `agentfs_contracts::meta`.

#[cfg(test)]
use agentfs_contracts::meta::{Kind, Meta, Perm};
use serde::{Deserialize, Serialize};

/// Each tree's own times of an entry, in nanoseconds since the epoch; never
/// in history.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Times {
    pub atime_ns: u64,
    pub mtime_ns: u64,
    pub ctime_ns: u64,
}

impl Times {
    /// A new entry's times: all `at_ns`.
    pub(super) const fn at(at_ns: u64) -> Self {
        Self {
            atime_ns: at_ns,
            mtime_ns: at_ns,
            ctime_ns: at_ns,
        }
    }

    /// Its content changed: mtime and ctime move. atime stays, as Linux
    /// leaves it at a write: it moves only when the entry is made or a time
    /// is set (reads are not tracked).
    pub(super) fn modified(&mut self, at_ns: u64) {
        self.mtime_ns = at_ns;
        self.ctime_ns = at_ns;
    }

    /// Its metadata or name changed: ctime moves.
    pub(super) fn changed(&mut self, at_ns: u64) {
        self.ctime_ns = at_ns;
    }
}

/// A time an explicit set gives: now (the step's own time) or a given time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SetTime {
    Now,
    At(u64),
}

impl SetTime {
    /// The time it names, `Now` being `at_ns`.
    pub(super) const fn resolve(self, at_ns: u64) -> u64 {
        match self {
            Self::Now => at_ns,
            Self::At(ns) => ns,
        }
    }
}

/// A plain file's metadata, for unit tests.
#[cfg(test)]
pub(super) const TEST_FILE: Meta = Meta {
    kind: Kind::File,
    mode: Perm::from_mode(0o644),
    uid: 1000,
    gid: 1000,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_save_keeps_the_atime() {
        let mut times = Times::at(5);
        times.atime_ns = 1;
        times.modified(9);
        assert_eq!((times.atime_ns, times.mtime_ns, times.ctime_ns), (1, 9, 9));
    }
}
