//! A view's save worker: the one thread that decides and lands the view's
//! pending saves, in the order their RELEASEs were begun, so the view's
//! request thread never waits for a save (docs/library/filesystem.md, "The
//! view host").
//!
//! The worker knows nothing of saves: it runs jobs, first in first out,
//! each caught on its own, so a panicking job leaves its save a failed
//! pending save (`agentfs_core::tree_actor::SaveToDecide`'s drop) and the
//! worker goes on with the next.

use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Mutex, PoisonError, mpsc};
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
        let (queue, jobs) = mpsc::channel::<SaveJob>();
        let owner = agentfs_core::failpoints::owner();
        let thread = std::thread::Builder::new()
            .name("tree-saves".into())
            .spawn(move || {
                agentfs_core::failpoints::adopt(owner);
                for job in jobs {
                    if catch_unwind(AssertUnwindSafe(job)).is_err() {
                        eprintln!(
                            "tree SaveFailed: tree={tree}: a save panicked; it stays pending"
                        );
                    }
                }
            })?;
        Ok(Self {
            queue: Mutex::new(Some(queue)),
            thread: Mutex::new(Some(thread)),
        })
    }

    /// Queues `job` behind every job submitted before it. Never blocks.
    /// After [`SaveWorker::drain`], runs `job` on the caller's thread.
    pub(super) fn submit(&self, job: SaveJob) {
        let queued = match &*self.queue.lock().unwrap_or_else(PoisonError::into_inner) {
            Some(queue) => queue.send(job).map_err(|returned| returned.0),
            None => Err(job),
        };
        if let Err(job) = queued {
            job();
        }
    }

    /// Takes no more jobs and returns once every queued job has run: what
    /// the view's thread calls after its session ends and before
    /// `HostEvent::ViewEnded`. Idempotent.
    pub(super) fn drain(&self) {
        drop(
            self.queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        let thread = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(thread) = thread
            && thread.join().is_err()
        {
            eprintln!("tree SaveFailed: a view's save worker panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, mpsc};

    use super::*;

    #[test]
    fn jobs_run_in_submission_order() {
        let worker = SaveWorker::start(TreeId::Root).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        for n in 0..100 {
            let seen = Arc::clone(&seen);
            worker.submit(Box::new(move || seen.lock().unwrap().push(n)));
        }
        worker.drain();
        assert_eq!(*seen.lock().unwrap(), (0..100).collect::<Vec<_>>());
    }

    #[test]
    fn a_panicking_job_does_not_stop_the_worker() {
        let worker = SaveWorker::start(TreeId::Root).unwrap();
        let (ran, heard) = mpsc::channel();
        worker.submit(Box::new(|| panic!("a save's panic")));
        worker.submit(Box::new(move || ran.send(()).unwrap()));
        worker.drain();
        assert_eq!(heard.try_recv(), Ok(()), "the job after the panic ran");
    }

    #[test]
    fn drain_runs_every_queued_job_and_later_submits_run_inline() {
        let worker = SaveWorker::start(TreeId::Root).unwrap();
        let (release, released) = mpsc::channel::<()>();
        let (ran, heard) = mpsc::channel();
        // The first job holds the worker, so the others queue behind it.
        worker.submit(Box::new(move || released.recv().unwrap()));
        for n in 0..10 {
            let ran = ran.clone();
            worker.submit(Box::new(move || ran.send(n).unwrap()));
        }
        release.send(()).unwrap();
        worker.drain();
        assert_eq!(
            heard.try_iter().collect::<Vec<_>>(),
            (0..10).collect::<Vec<_>>()
        );
        let here = std::thread::current().id();
        let (on, thread) = mpsc::channel();
        worker.submit(Box::new(move || {
            on.send(std::thread::current().id()).unwrap()
        }));
        assert_eq!(
            thread.try_recv(),
            Ok(here),
            "run inline, before submit returned"
        );
        worker.drain();
    }
}
