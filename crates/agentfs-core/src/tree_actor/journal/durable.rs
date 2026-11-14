//! The journal's one numbering and its durable mark: a call is appended
//! without an fsync, and one leader's fsync makes every record appended before
//! it began durable for all who wait (group commit: DeWitt et al., 1984;
//! PostgreSQL's WAL flush). Waiting never holds the actor's lock
//! (docs/trees.md, "Recording and durability").
//!
//! Slice B2a builds the bodies; the signatures are frozen on its page.

use serde::{Deserialize, Serialize};

/// A record's place in the journal: one numbering for calls, steps, the
/// durable mark and the built point. A step is named by its record number
/// (its event number). Never reused: a checkpoint carries the next one across
/// compaction and restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RecordNo(u64);

impl RecordNo {
    /// The number before every record: nothing is durable, nothing is built.
    pub const ZERO: Self = Self(0);

    /// The number as stored in a frame and printed in metrics.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Why records could not be made durable.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DurableError {
    /// An fsync of the journal failed: the page cache says nothing about the
    /// disk any more, so the actor stops and every waiter gets this errno.
    #[error("journal fsync failed: errno {0}")]
    Io(i32),
    /// The actor stopped before these records were durable; reopen to recover.
    #[error("the tree actor has stopped")]
    Stopped,
}

impl DurableError {
    /// The errno a file call (FLUSH, FSYNC, FSYNCDIR) answers with.
    pub fn errno(&self) -> i32 {
        match self {
            Self::Io(errno) => *errno,
            Self::Stopped => libc::EIO,
        }
    }
}

/// The journal's group commit, shared by the actor (the one appender) and by
/// every caller that waits for durability: FLUSH, FSYNC, FSYNCDIR, an
/// `O_SYNC` write, the builder before it reads a batch. Callable from any
/// thread; it takes the actor's lock never.
///
/// Invariants: [`Durability::mark`] only rises; it rises to a record only
/// after an fsync that began after that record's `write(2)` returned; at most
/// one fsync of the journal runs at a time.
pub struct Durability {
    _private: (),
}

impl Durability {
    /// The highest record every finished fsync covers: the durable mark.
    pub fn mark(&self) -> RecordNo {
        todo!("B2a")
    }

    /// The highest record appended so far (in the kernel, maybe not on disk).
    pub fn appended(&self) -> RecordNo {
        todo!("B2a")
    }

    /// Returns once every record up to `through` is on disk, with the mark
    /// then. Already covered: returns at once, no fsync. Else joins the fsync
    /// in flight if it began after `through` was appended, or leads the next
    /// one, which covers everything appended before it began.
    ///
    /// # Errors
    ///
    /// [`DurableError::Io`] when that fsync failed (the actor stops),
    /// [`DurableError::Stopped`] once it has stopped.
    pub fn make_durable(&self, through: RecordNo) -> Result<RecordNo, DurableError> {
        let _ = through;
        todo!("B2a")
    }

    /// [`Durability::make_durable`] through [`Durability::appended`]: FSYNCDIR,
    /// a read-only descriptor's fsync, recovery before serving.
    ///
    /// # Errors
    ///
    /// As [`Durability::make_durable`].
    pub fn make_all_durable(&self) -> Result<RecordNo, DurableError> {
        todo!("B2a")
    }
}
