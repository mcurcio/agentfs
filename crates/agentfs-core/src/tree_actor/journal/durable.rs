//! The journal's one numbering and its durable mark: a call is appended
//! without an fsync, and one leader's fsync makes every record appended before
//! it began durable for all who wait (group commit: DeWitt et al., 1984;
//! PostgreSQL's WAL flush). Waiting never holds the actor's lock
//! (docs/trees.md, "Recording and durability").

use std::fs::File;
use std::io;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};

use serde::{Deserialize, Serialize};

use crate::tree_actor::Error;
use crate::tree_actor::call_path::Then;
use crate::tree_actor::stages::{Timer, Which};

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

    /// The record numbered `n`: the journal's frames and the actor's counter.
    pub(in crate::tree_actor) const fn new(n: u64) -> Self {
        Self(n)
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
    state: Mutex<State>,
    /// Signalled whenever a leader's fsync ends.
    ended: Condvar,
    /// The waiter thread's queue of deferred waits ([`Durability::then`]).
    queue: Mutex<Sender<(RecordNo, Then)>>,
    #[cfg(test)]
    in_fsync: Mutex<Option<Box<dyn Fn() + Send>>>,
}

struct State {
    file: Arc<File>,
    appended: RecordNo,
    mark: RecordNo,
    /// An fsync is running: a waiter it may cover waits for it to end.
    leading: bool,
    /// The errno of a failed fsync: nothing is durable past the mark again.
    failed: Option<i32>,
}

/// Every deferred wait queued since the thread last looked is answered by one
/// [`Durability::make_durable`], through the highest record among them.
fn serve_waits(durability: &Weak<Durability>, queue: &Receiver<(RecordNo, Then)>) {
    while let Ok(first) = queue.recv() {
        let mut batch = vec![first];
        batch.extend(queue.try_iter());
        let through = batch
            .iter()
            .map(|(record, _)| *record)
            .max()
            .unwrap_or(RecordNo::ZERO);
        let result = durability
            .upgrade()
            .map_or(Err(DurableError::Stopped), |d| d.make_durable(through));
        for (_, then) in batch {
            then(result.clone().map(|_| ()).map_err(Error::from));
        }
    }
}

impl Durability {
    /// A journal file's group commit, with its records up to `appended` in the
    /// file and up to `mark` known durable; starts its waiter thread.
    pub(in crate::tree_actor) fn new(
        file: Arc<File>,
        appended: RecordNo,
        mark: RecordNo,
    ) -> Arc<Self> {
        let (sender, receiver) = mpsc::channel();
        Arc::new_cyclic(|weak: &Weak<Self>| {
            let weak = weak.clone();
            // Ends when the last handle drops its sender: the queue disconnects.
            std::thread::Builder::new()
                .name("tree-durable".into())
                .spawn(move || serve_waits(&weak, &receiver))
                .map_err(|e| eprintln!("tree DurableWaiter: {e}"))
                .ok();
            Self {
                state: Mutex::new(State {
                    file,
                    appended,
                    mark: mark.min(appended),
                    leading: false,
                    failed: None,
                }),
                ended: Condvar::new(),
                queue: Mutex::new(sender),
                #[cfg(test)]
                in_fsync: Mutex::new(None),
            }
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Every holder leaves the state whole: a poisoned lock is still sound.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The highest record every finished fsync covers: the durable mark.
    pub fn mark(&self) -> RecordNo {
        self.state().mark
    }

    /// The highest record appended so far (in the kernel, maybe not on disk).
    pub fn appended(&self) -> RecordNo {
        self.state().appended
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
        let mut state = self.state();
        // Nothing past the last append can be asked for: a wait for it would lead forever.
        let through = through.min(state.appended);
        loop {
            if state.mark >= through {
                return Ok(state.mark);
            }
            if let Some(errno) = state.failed {
                return Err(DurableError::Io(errno));
            }
            if !state.leading {
                break;
            }
            state = self
                .ended
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.leading = true;
        let target = state.appended;
        let file = Arc::clone(&state.file);
        drop(state);
        let synced = self.fsync(&file);
        let mut state = self.state();
        state.leading = false;
        let result = match synced {
            Ok(()) => {
                state.mark = state.mark.max(target);
                Ok(state.mark)
            }
            Err(e) => {
                let errno = e.raw_os_error().unwrap_or(libc::EIO);
                eprintln!("tree JournalFsyncFailed: {e}");
                state.failed = Some(errno);
                Err(DurableError::Io(errno))
            }
        };
        drop(state);
        self.ended.notify_all();
        result
    }

    /// The leader's fsync, outside the lock: `fdatasync` covers the length a
    /// read needs.
    fn fsync(&self, file: &File) -> io::Result<()> {
        crate::failpoints::hit("tree.wal_before_fsync")?;
        #[cfg(test)]
        if let Some(hook) = &*self.in_fsync.lock().unwrap_or_else(PoisonError::into_inner) {
            hook();
        }
        {
            let _timer = Timer::start(Which::JournalFsync);
            file.sync_data()?;
        }
        crate::failpoints::hit("tree.wal_after_fsync")
    }

    /// [`Durability::make_durable`] through [`Durability::appended`]: FSYNCDIR,
    /// a read-only descriptor's fsync, recovery before serving.
    ///
    /// # Errors
    ///
    /// As [`Durability::make_durable`].
    pub fn make_all_durable(&self) -> Result<RecordNo, DurableError> {
        let appended = self.appended();
        self.make_durable(appended)
    }

    /// Under the actor lock, after the frame's write returned.
    pub(in crate::tree_actor) fn appended_to(&self, record: RecordNo) {
        let mut state = self.state();
        state.appended = state.appended.max(record);
    }

    /// Compaction, under the actor lock, after the new file is fsynced and
    /// renamed: every record through `last` is in it, so the mark rises to it.
    pub(in crate::tree_actor) fn replace_file(&self, file: Arc<File>, last: RecordNo) {
        let mut state = self.state();
        state.file = file;
        state.appended = state.appended.max(last);
        state.mark = state.mark.max(last);
        drop(state);
        self.ended.notify_all();
    }

    /// Queues `then` on the waiter thread (started by `new`): one fsync
    /// answers every queued wait it covers. Covered already: runs at once, on
    /// this thread.
    pub(in crate::tree_actor) fn then(&self, through: RecordNo, then: Then) {
        let covered = {
            let state = self.state();
            state.mark >= through.min(state.appended)
        };
        if covered {
            then(Ok(()));
            return;
        }
        let queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(mpsc::SendError((through, then))) = queue.send((through, then)) {
            // No waiter thread: wait here rather than drop the reply.
            drop(queue);
            then(self.make_durable(through).map(|_| ()).map_err(Error::from));
        }
    }

    /// An fsync of this journal failed: the actor stops.
    pub(in crate::tree_actor) fn has_failed(&self) -> bool {
        self.state().failed.is_some()
    }

    /// Runs `hook` inside every leader's fsync, before it: a test holds the
    /// leader there on an event, never a timer.
    #[cfg(test)]
    pub(in crate::tree_actor) fn hold_fsync_with(&self, hook: impl Fn() + Send + 'static) {
        *self.in_fsync.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(hook));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::sync::mpsc::sync_channel;

    use super::*;
    use crate::tree_actor::stages::Stages;

    fn durability(appended: u64) -> (tempfile::NamedTempFile, Arc<Durability>) {
        let file = tempfile::NamedTempFile::new().unwrap();
        let handle = Arc::new(file.reopen().unwrap());
        (
            file,
            Durability::new(handle, RecordNo::new(appended), RecordNo::ZERO),
        )
    }

    /// Process-wide counters: the tests that read them run one at a time.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    #[test]
    fn concurrent_waits_share_one_fsync() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        let (_file, d) = durability(0);
        // The first leader is held inside its fsync until every thread has
        // appended and asked: they all join the one fsync that follows.
        let (entered, inside) = sync_channel::<()>(0);
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        let first = std::sync::atomic::AtomicBool::new(true);
        d.hold_fsync_with(move || {
            if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                entered.send(()).unwrap();
                released.lock().unwrap().recv().unwrap();
            }
        });
        let before = Stages::current().journal_fsync.count;
        let threads = 8;
        let all_asked = Arc::new(Barrier::new(threads + 1));
        std::thread::scope(|scope| {
            d.appended_to(RecordNo::new(1));
            let leader = scope.spawn(|| d.make_durable(RecordNo::new(1)).unwrap());
            inside.recv().unwrap();
            let mut handles = Vec::new();
            for i in 0..threads {
                let d = &d;
                let all_asked = Arc::clone(&all_asked);
                handles.push(scope.spawn(move || {
                    let record = RecordNo::new(2 + i as u64);
                    d.appended_to(record);
                    all_asked.wait();
                    let mark = d.make_durable(record).unwrap();
                    assert!(mark >= record);
                }));
            }
            all_asked.wait();
            release.send(()).unwrap();
            assert!(leader.join().unwrap() >= RecordNo::new(1));
            for handle in handles {
                handle.join().unwrap();
            }
        });
        let fsyncs = Stages::current().journal_fsync.count - before;
        assert!(fsyncs < 8, "{fsyncs} fsyncs for 9 waits");
        assert_eq!(d.mark(), RecordNo::new(1 + threads as u64));
    }

    #[test]
    fn the_mark_never_passes_a_record_written_after_the_fsync_began() {
        let (_file, d) = durability(0);
        let (entered, inside) = sync_channel::<()>(0);
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        d.hold_fsync_with(move || {
            entered.send(()).unwrap();
            released.lock().unwrap().recv().unwrap();
        });
        d.appended_to(RecordNo::new(3));
        std::thread::scope(|scope| {
            let leader = scope.spawn(|| d.make_durable(RecordNo::new(3)).unwrap());
            inside.recv().unwrap();
            // Appended while the leader's fsync runs: not covered by it.
            d.appended_to(RecordNo::new(4));
            release.send(()).unwrap();
            assert_eq!(leader.join().unwrap(), RecordNo::new(3));
        });
        assert_eq!(d.mark(), RecordNo::new(3));
        assert_eq!(d.appended(), RecordNo::new(4));
    }

    #[test]
    fn a_deferred_wait_is_answered_after_its_fsync() {
        let (_file, d) = durability(0);
        d.appended_to(RecordNo::new(2));
        let (answered, answer) = mpsc::channel();
        d.then(
            RecordNo::new(2),
            Box::new(move |result| answered.send(result.is_ok()).unwrap()),
        );
        assert!(answer.recv().unwrap());
        assert_eq!(d.mark(), RecordNo::new(2));
        // Covered already: answered at once on this thread.
        let (answered, answer) = mpsc::channel();
        d.then(
            RecordNo::new(1),
            Box::new(move |result| answered.send(result.is_ok()).unwrap()),
        );
        assert_eq!(answer.try_recv(), Ok(true));
    }

    #[test]
    fn compaction_raises_the_mark_to_its_last_record() {
        let (file, d) = durability(5);
        d.replace_file(Arc::new(file.reopen().unwrap()), RecordNo::new(7));
        assert_eq!(d.mark(), RecordNo::new(7));
        assert_eq!(d.appended(), RecordNo::new(7));
        assert_eq!(d.make_all_durable(), Ok(RecordNo::new(7)));
    }
}
