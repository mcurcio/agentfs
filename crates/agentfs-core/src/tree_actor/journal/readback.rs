//! An open's calls read back from the journal with the actor unlocked: the
//! journal is their one home, so a handle keeps no call list (docs/trees.md,
//! "File maps"). Under the lock a close takes a [`CallReader`], a snapshot of
//! the linked segments and their lengths (O(segments)); with the lock
//! released it visits the open's `Call` frames in record order. A frame
//! compaction carried keeps its record number in a later segment, and a
//! segment a reader holds stays readable after it is unlinked, so a
//! snapshot reads every call made before it, whatever compaction does next.

#![expect(
    dead_code,
    reason = "frozen interface (agentfs-ln6.1): the close's read-back"
)]

use std::sync::Arc;

use super::Journal;
use super::segment::{FramePos, Segment};
use crate::tree_actor::Result;
use crate::tree_actor::recording::Call;

/// The journal's linked segments as one moment left them, each read only up
/// to its length then. Taken under the actor's lock; read without it.
pub(in crate::tree_actor) struct CallReader {
    segments: Vec<(Arc<Segment>, u64)>,
    salt: u32,
}

impl Journal {
    /// A snapshot of the segments linked now: O(segments), no read.
    pub(in crate::tree_actor) fn call_reader(&self) -> CallReader {
        todo!("agentfs-ln6.1")
    }
}

impl CallReader {
    /// Visits each `Call` frame of the open `handle` in record order, with
    /// where its bytes are: carried frames at their copies. `handle` is the
    /// open's own record number, so no earlier frame is one of its calls.
    /// Reads frame heads only, never a call's bytes.
    ///
    /// # Errors
    ///
    /// [`crate::tree_actor::Error::Format`] for a frame that does not parse
    /// or is missing; an error `visit` returns stops the walk and is
    /// returned.
    pub(in crate::tree_actor) fn for_each_call<F>(&self, handle: u64, visit: F) -> Result<()>
    where
        F: FnMut(u64, Call, FramePos) -> Result<()>,
    {
        let _ = (handle, visit);
        todo!("agentfs-ln6.1")
    }
}
