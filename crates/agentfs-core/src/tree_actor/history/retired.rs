//! Deferred drops: the large values a catch-up with history lets go (replaced
//! saved maps, shapes, a map's base file), handed to History and dropped
//! after the actor's lock is released, on a thread that serves no requests
//! (docs/trees.md, "History trails the files").

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

/// Values let go under the actor's lock, to be dropped after it: a saved map
/// a catch-up or a rebase replaced, a shape a catch-up replaced, and what
/// they hold (pieces, `Arc`s of frames, a map's base file).
///
/// Invariant: nothing in it is read again. It is only dropped, whole, by
/// [`DeferredDrops`].
#[derive(Default)]
#[must_use = "retired values go to `History::defer_drops`, or they drop where they are"]
pub(in crate::tree_actor) struct Retired {
    values: Vec<Box<dyn Send>>,
}

impl Retired {
    /// Adds `value`, to be dropped with the rest. O(1).
    pub(in crate::tree_actor) fn push<T: Send + 'static>(&mut self, value: T) {
        self.values.push(Box::new(value));
    }

    /// Adds every value of `other`. O(other).
    pub(in crate::tree_actor) fn append(&mut self, mut other: Retired) {
        self.values.append(&mut other.values);
    }

    /// Whether it holds no value.
    pub(in crate::tree_actor) fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// History's deferred drops: every [`Retired`] the actor hands over is
/// dropped off the caller's thread, in the order handed over.
///
/// Contract: [`DeferredDrops::defer`] is O(1) and never waits for a drop; a
/// deferred value is dropped on a thread (`tree-drops`) that answers no FUSE
/// request and holds no lock the call path takes across a drop's I/O (a
/// base file's drop takes the open-bases cache's lock for O(256) and unlinks
/// outside it); [`DeferredDrops::finish`] returns once every value deferred
/// before it is dropped and that thread has ended, so a base file's link is
/// gone before the state area's lease is released. The thread holds the
/// queue only, never History, so no cycle keeps either alive. The same path
/// serves the actor behind [`Recording`] and a `TreeActor` used directly:
/// there is no second mode.
///
/// [`Recording`]: crate::tree_actor::Recording
pub(in crate::tree_actor) struct DeferredDrops {
    queue: Arc<Queue>,
    /// The drop thread, until [`DeferredDrops::finish`] joins it. Held across
    /// the join, so a second `finish` returns only after the first.
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// What [`DeferredDrops::defer`] hands the drop thread.
#[derive(Default)]
struct Queue {
    pending: Mutex<Pending>,
    /// Signalled on a value queued and on finishing.
    wake: Condvar,
}

#[derive(Default)]
struct Pending {
    values: Vec<Retired>,
    /// Set by [`DeferredDrops::finish`]: the thread drops what is queued and
    /// ends.
    finishing: bool,
}

impl Queue {
    fn lock(&self) -> MutexGuard<'_, Pending> {
        // No value is dropped under the lock, so no holder panics midway: a
        // poisoned lock is still sound.
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The drop thread: takes the whole queue under the lock, releases it,
    /// then drops each value in the order deferred; ends once finishing
    /// finds the queue empty.
    fn run(&self) {
        loop {
            let batch = {
                let mut pending = self.lock();
                while pending.values.is_empty() && !pending.finishing {
                    pending = self
                        .wake
                        .wait(pending)
                        .unwrap_or_else(PoisonError::into_inner);
                }
                std::mem::take(&mut pending.values)
            };
            // Empty only when finishing: everything deferred before it is
            // dropped.
            if batch.is_empty() {
                return;
            }
            drop(batch);
        }
    }
}

impl DeferredDrops {
    /// Deferred drops for one volume's History, with the thread that drops
    /// them (named `tree-drops`).
    ///
    /// # Errors
    ///
    /// The thread could not be spawned.
    pub(in crate::tree_actor) fn start() -> std::io::Result<Self> {
        let queue = Arc::new(Queue::default());
        let served = Arc::clone(&queue);
        let thread = std::thread::Builder::new()
            .name("tree-drops".into())
            .spawn(move || served.run())?;
        Ok(Self {
            queue,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// Takes `values` to drop them on the drop thread: one push and a notify
    /// under the queue's lock, never a drop and never a wait for one. Values
    /// deferred after [`DeferredDrops::finish`] stay queued and drop with
    /// `self`.
    pub(in crate::tree_actor) fn defer(&self, values: Retired) {
        if values.is_empty() {
            return;
        }
        self.queue.lock().values.push(values);
        self.queue.wake.notify_one();
    }

    /// Returns once every value deferred so far is dropped and the drop
    /// thread has ended: what the actor's shutdown runs after the builder
    /// has stopped, before the lease is released. A second call is a no-op.
    /// A drop that panicked is logged (`tree DropsFailed`), never raised.
    pub(in crate::tree_actor) fn finish(&self) {
        let mut thread = self.thread.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(handle) = thread.take() else {
            return;
        };
        self.queue.lock().finishing = true;
        self.queue.wake.notify_one();
        if let Err(panic) = handle.join() {
            let why = panic
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a drop panicked".to_owned());
            eprintln!("tree DropsFailed: {why}");
        }
    }
}

impl Drop for DeferredDrops {
    /// [`DeferredDrops::finish`], if it has not run.
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
    use std::time::Duration;

    use super::*;
    use crate::tree_actor::journal::durable::RecordNo;
    use crate::tree_actor::recording::file_map::BaseFile;

    /// The bound that fails a test rather than hanging it.
    const BOUND: Duration = Duration::from_secs(10);

    /// Reports the name of the thread that drops it.
    struct Named(Sender<Option<String>>);

    impl Drop for Named {
        fn drop(&mut self) {
            let name = std::thread::current().name().map(str::to_owned);
            let _ = self.0.send(name);
        }
    }

    /// Says it has started dropping, then waits for its release (at most
    /// [`BOUND`]).
    struct Blocked {
        started: Sender<()>,
        release: Receiver<()>,
    }

    impl Drop for Blocked {
        fn drop(&mut self) {
            let _ = self.started.send(());
            let _ = self.release.recv_timeout(BOUND);
        }
    }

    /// A blocked value, what hears its drop start, and its release.
    fn blocked() -> (Blocked, Receiver<()>, Sender<()>) {
        let (started, on_start) = mpsc::channel();
        let (release, on_release) = mpsc::channel();
        let value = Blocked {
            started,
            release: on_release,
        };
        (value, on_start, release)
    }

    fn retired<T: Send + 'static>(value: T) -> Retired {
        let mut retired = Retired::default();
        retired.push(value);
        retired
    }

    /// Runs `f` on a thread of its own; the receiver hears when it returns.
    fn on_thread(f: impl FnOnce() + Send + 'static) -> Receiver<()> {
        let (done, returned) = mpsc::channel();
        std::thread::spawn(move || {
            f();
            let _ = done.send(());
        });
        returned
    }

    #[test]
    fn a_deferred_value_drops_on_the_drop_thread() {
        let drops = DeferredDrops::start().unwrap();
        let (tx, rx) = mpsc::channel();
        drops.defer(retired(Named(tx)));
        let name = rx.recv_timeout(BOUND).expect("the value is dropped");
        assert_eq!(name.as_deref(), Some("tree-drops"));
    }

    #[test]
    fn defer_returns_while_a_drop_runs() {
        let drops = Arc::new(DeferredDrops::start().unwrap());
        let (first, started, release) = blocked();
        let deferring = Arc::clone(&drops);
        let deferred = on_thread(move || deferring.defer(retired(first)));
        let first_ran = started.recv_timeout(BOUND).is_ok();
        let first_returned = deferred.recv_timeout(BOUND).is_ok();
        let deferring = Arc::clone(&drops);
        let (tx, named) = mpsc::channel();
        let second = on_thread(move || deferring.defer(retired(Named(tx))));
        let second_returned = second.recv_timeout(BOUND).is_ok();
        let _ = release.send(());
        assert!(first_ran, "the first value's drop started");
        assert!(
            first_returned,
            "the first defer returned while its drop ran"
        );
        assert!(
            second_returned,
            "the second defer returned while a drop ran"
        );
        let name = named.recv_timeout(BOUND).expect("the second value drops");
        assert_eq!(name.as_deref(), Some("tree-drops"));
    }

    #[test]
    fn finish_waits_for_every_drop() {
        let dir = tempfile::tempdir().unwrap();
        let bases = dir.path().join("bases");
        std::fs::create_dir(&bases).unwrap();
        let path = dir.path().join("checkout");
        std::fs::write(&path, b"built").unwrap();
        let base = BaseFile::open(&bases, &path, RecordNo::new(1), None).unwrap();
        let links = || std::fs::read_dir(&bases).unwrap().count();
        assert_eq!(links(), 1);

        let drops = Arc::new(DeferredDrops::start().unwrap());
        let (value, started, release) = blocked();
        let mut values = retired(value);
        values.push(base);
        drops.defer(values);
        started.recv_timeout(BOUND).expect("the drop started");
        let finishing = Arc::clone(&drops);
        let finished = on_thread(move || finishing.finish());
        // The one timed wait: finish must not return while the drop is held.
        let early = finished.recv_timeout(Duration::from_millis(200));
        let _ = release.send(());
        assert_eq!(
            early,
            Err(RecvTimeoutError::Timeout),
            "finish returned while a drop was blocked"
        );
        finished.recv_timeout(BOUND).expect("finish returns");
        assert_eq!(links(), 0, "the base file's link is gone after finish");
        drops.finish();
    }
}
