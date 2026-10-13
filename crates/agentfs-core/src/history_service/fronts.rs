//! The service's side of listening fronts (design §14.1, §15).
//!
//! A front that sends [`super::protocol::Request::Listen`] hands its
//! connection to the service's [`Fronts`], one per slot (a front's new
//! listening connection replaces its old one). A restore or revert served
//! by the service then calls every live front through it:
//!
//! 1. [`Fronts::require_all`]: every slot whose lease is held has a
//!    listening front, waiting up to a grace for one that is reconnecting;
//!    otherwise the restore is refused as busy (a front that cannot be
//!    settled or told to drop its caches would serve stale content).
//! 2. [`Fronts::settle`]: each front records what is dirty in it.
//! 3. [`Fronts::open_for_writing`]: which paths any front holds open for
//!    writing.
//! 4. After writing, [`Fronts::invalidate`]: each front drops its kernel's
//!    caches of the paths written.
//!
//! Calls to one front are made one at a time, each with a timeout; a front
//! whose call fails (it died, or broke the protocol) loses its link, and a
//! front whose lease is no longer held is never called.

use std::collections::{BTreeSet, HashMap};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::lease::FrontLease;
use super::protocol::{Answer, Call, FrontAnswer, FrontCall, FrontInfo, read_frame, write_frame};
use crate::local_socket;
use crate::paths::RepoPath;
use crate::volume::StateDir;

/// How long a call may take, at least ([`Fronts::new`] scales it with the
/// commit window: a settle waits for the front's records to be journaled).
pub const MIN_CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// How many paths one `OpenForWriting` or `Invalidate` call carries.
const PATHS_PER_CALL: usize = 4096;

/// One listening front.
struct Link {
    front: FrontInfo,
    stream: Mutex<UnixStream>,
    /// The same connection, to shut down while a call is in flight.
    handle: Option<UnixStream>,
}

/// The service's listening fronts. Cheap to clone.
#[derive(Clone)]
pub struct Fronts {
    inner: Arc<Inner>,
}

struct Inner {
    state: StateDir,
    links: Mutex<HashMap<u16, Arc<Link>>>,
    changed: Condvar,
    next_id: AtomicU64,
    timeout: Duration,
}

/// Why the live fronts could not all be reached.
#[derive(Debug, thiserror::Error)]
pub enum FrontsError {
    /// A front holds a lease but is not listening (an older front, or one
    /// still reconnecting after the grace): the restore cannot run live.
    #[error(
        "front slot(s) {} are live but not listening for the history service's calls (a front \
         from before live restores, or one still reconnecting); upgrade or restart those fronts, \
         or stop them, then retry",
        list(.0)
    )]
    NotListening(Vec<u16>),
    /// A call to a front failed.
    #[error("front in slot {slot} ({pod}): {message}")]
    Call {
        slot: u16,
        pod: String,
        message: String,
    },
    #[error("reading the front leases: {0}")]
    Leases(#[from] std::io::Error),
}

fn list(slots: &[u16]) -> String {
    slots
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

impl Fronts {
    /// No fronts yet, for the service of the state area `state`. Each call
    /// may take `timeout` (at least [`MIN_CALL_TIMEOUT`]).
    pub fn new(state: &StateDir, timeout: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: state.clone(),
                links: Mutex::new(HashMap::new()),
                changed: Condvar::new(),
                next_id: AtomicU64::new(1),
                timeout: timeout.max(MIN_CALL_TIMEOUT),
            }),
        }
    }

    /// Takes `stream`, a connection `front` turned around, as its link,
    /// replacing any older link of its slot.
    pub(crate) fn add(&self, front: FrontInfo, stream: UnixStream) {
        let _ = stream.set_read_timeout(Some(self.inner.timeout));
        let _ = stream.set_write_timeout(Some(self.inner.timeout));
        let link = Arc::new(Link {
            front,
            handle: stream.try_clone().ok(),
            stream: Mutex::new(stream),
        });
        let old = self.links().insert(link.front.slot, link.clone());
        if let Some(old) = old {
            shut(&old);
        }
        self.inner.changed.notify_all();
    }

    /// Closes every link (the service is stopping): fronts reconnect to
    /// the next service.
    pub(crate) fn close_all(&self) {
        for (_, link) in self.links().drain() {
            shut(&link);
        }
    }

    /// The slots with a listening front now.
    pub fn listening(&self) -> BTreeSet<u16> {
        self.links().keys().copied().collect()
    }

    /// The slots whose lease is held now, after waiting up to `grace` for
    /// each to have a listening front. Links of slots no longer leased, and
    /// links their front closed, are dropped. Not while a call is in flight
    /// (one restore at a time calls fronts).
    pub fn require_all(&self, grace: Duration) -> Result<Vec<u16>, FrontsError> {
        let deadline = Instant::now() + grace;
        let mut links = self.links();
        loop {
            let live = FrontLease::live(&self.inner.state)?;
            links.retain(|slot, link| {
                // A front that died, or stopped listening, closed its end.
                let keep = live.contains(slot)
                    && !link.handle.as_ref().is_none_or(local_socket::is_closed);
                if !keep {
                    shut(link);
                }
                keep
            });
            let mut missing: Vec<u16> = live
                .iter()
                .copied()
                .filter(|s| !links.contains_key(s))
                .collect();
            if missing.is_empty() {
                let mut slots: Vec<u16> = live.into_iter().collect();
                slots.sort_unstable();
                return Ok(slots);
            }
            let now = Instant::now();
            if now >= deadline {
                missing.sort_unstable();
                return Err(FrontsError::NotListening(missing));
            }
            // Leases change without a notification: look again soon.
            let wait = (deadline - now).min(Duration::from_millis(100));
            links = self
                .inner
                .changed
                .wait_timeout(links, wait)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Asks every listening front to record what is dirty in it; returns
    /// how many paths are still waiting for a repair, over all fronts.
    pub fn settle(&self) -> Result<usize, FrontsError> {
        let mut failed = 0;
        for link in self.snapshot() {
            match self.call(&link, Call::Settle)? {
                Answer::Settled { failed: f, .. } => failed += f,
                other => return Err(unexpected(&link, &other)),
            }
        }
        Ok(failed)
    }

    /// Whether any listening front holds each of `paths` open for writing.
    pub fn open_for_writing(&self, paths: &[RepoPath]) -> Result<Vec<bool>, FrontsError> {
        let mut open = vec![false; paths.len()];
        for link in self.snapshot() {
            for (n, chunk) in paths.chunks(PATHS_PER_CALL).enumerate() {
                let call = Call::OpenForWriting {
                    paths: chunk.to_vec(),
                };
                match self.call(&link, call)? {
                    Answer::Open { open: answer } if answer.len() == chunk.len() => {
                        for (i, o) in answer.into_iter().enumerate() {
                            open[n * PATHS_PER_CALL + i] |= o;
                        }
                    }
                    other => return Err(unexpected(&link, &other)),
                }
            }
        }
        Ok(open)
    }

    /// Tells every listening front to drop its kernel's caches of
    /// `paths`. Every front is told even if one fails; the first failure
    /// is returned.
    pub fn invalidate(&self, paths: &[RepoPath]) -> Result<(), FrontsError> {
        let mut first = None;
        for link in self.snapshot() {
            for chunk in paths.chunks(PATHS_PER_CALL) {
                let call = Call::Invalidate {
                    paths: chunk.to_vec(),
                };
                let result = match self.call(&link, call) {
                    Ok(Answer::Invalidated) => Ok(()),
                    Ok(other) => Err(unexpected(&link, &other)),
                    Err(e) => Err(e),
                };
                if let Err(e) = result {
                    first.get_or_insert(e);
                    break;
                }
            }
        }
        first.map_or(Ok(()), Err)
    }

    fn snapshot(&self) -> Vec<Arc<Link>> {
        let mut links: Vec<Arc<Link>> = self.links().values().cloned().collect();
        links.sort_by_key(|l| l.front.slot);
        links
    }

    /// One call and its answer. A failed call drops the link.
    fn call(&self, link: &Arc<Link>, call: Call) -> Result<Answer, FrontsError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let result = {
            let mut stream = link.stream.lock().unwrap_or_else(PoisonError::into_inner);
            write_frame(&mut *stream, &FrontCall { id, call })
                .map_err(|e| e.to_string())
                .and_then(|()| read_frame::<FrontAnswer>(&mut *stream).map_err(|e| e.to_string()))
        };
        let failed = |message: String| {
            let mut links = self.links();
            if links
                .get(&link.front.slot)
                .is_some_and(|l| Arc::ptr_eq(l, link))
            {
                links.remove(&link.front.slot);
            }
            shut(link);
            FrontsError::Call {
                slot: link.front.slot,
                pod: link.front.origin.pod.clone(),
                message,
            }
        };
        match result {
            Ok(FrontAnswer { id: got, answer }) if got == id => match answer {
                Answer::Error { message } => Err(FrontsError::Call {
                    slot: link.front.slot,
                    pod: link.front.origin.pod.clone(),
                    message,
                }),
                answer => Ok(answer),
            },
            Ok(FrontAnswer { id: got, .. }) => {
                Err(failed(format!("answered call {got}, not call {id}")))
            }
            Err(e) => Err(failed(e)),
        }
    }

    fn links(&self) -> std::sync::MutexGuard<'_, HashMap<u16, Arc<Link>>> {
        self.inner
            .links
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

fn shut(link: &Link) {
    if let Some(handle) = &link.handle {
        let _ = handle.shutdown(Shutdown::Both);
    }
}

fn unexpected(link: &Link, answer: &Answer) -> FrontsError {
    FrontsError::Call {
        slot: link.front.slot,
        pod: link.front.origin.pod.clone(),
        message: format!("unexpected answer {answer:?}"),
    }
}
