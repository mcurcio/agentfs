//! A view's save worker: the one thread that decides and lands the view's
//! pending saves, in the order their RELEASEs were begun, so the view's
//! request thread never waits for a save
//! (docs/target-architecture/future/view-save-worker.md).
//!
//! The worker knows nothing of saves: it runs jobs, first in first out,
//! each caught on its own, so a panicking job leaves its save a failed
//! pending save (`agentfs_core::tree_actor::SaveToDecide`'s drop) and the
//! worker goes on with the next.
#![expect(
    dead_code,
    reason = "the view's RELEASE submits to it once view-save-1 lands"
)]

use std::io;
use std::sync::{Mutex, mpsc};
use std::thread::JoinHandle;

use agentfs_core::tree_actor::TreeId;

/// One unit of the worker's work: the view's finish of one pending save,
/// with its logging.
pub(super) type SaveJob = Box<dyn FnOnce() + Send + 'static>;

/// A view's save worker, shared by every clone of the view and by the
/// thread that ends it.
pub(super) struct SaveWorker {
    /// None once drained: a job submitted then runs on the caller's thread.
    queue: Mutex<Option<mpsc::Sender<SaveJob>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl SaveWorker {
    /// Starts the worker thread for the view of `tree` (named
    /// `tree-saves`).
    ///
    /// # Errors
    ///
    /// A failed thread spawn.
    pub(super) fn start(tree: TreeId) -> io::Result<Self> {
        let _ = tree;
        todo!("view-save-1")
    }

    /// Queues `job` behind every job submitted before it. Never blocks.
    /// After [`SaveWorker::drain`], runs `job` on the caller's thread.
    pub(super) fn submit(&self, job: SaveJob) {
        let _ = (&self.queue, job);
        todo!("view-save-1")
    }

    /// Takes no more jobs and returns once every queued job has run: what
    /// the view's thread calls after its session ends and before
    /// `HostEvent::ViewEnded`. Idempotent.
    pub(super) fn drain(&self) {
        let _ = &self.thread;
        todo!("view-save-1")
    }
}
