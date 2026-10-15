//! The service's side of listening fronts (design §14.1, §15).
//!
//! A front that opens a `Listen` stream hands it to the service's
//! [`Fronts`] as its link, one per slot (a front's new stream replaces its
//! old one). A restore or revert served by the service then calls every
//! live front through it:
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
//! whose call fails (it died, its stream ended, or it broke the protocol)
//! loses its link, and a front whose lease is no longer held is never
//! called. Calls are made from the caller's thread (a restore's): the
//! link's streams are fed by the service's runtime.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tonic::Status;

use super::lease::FrontLease;
use super::wire::pb;
use super::{Answer, Call, FrontInfo};
use crate::paths::RepoPath;
use crate::volume::StateDir;

/// How long a call may take, at least ([`Fronts::new`] scales it with the
/// commit window: a settle waits for the front's records to be journaled).
pub const MIN_CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// How many paths one `OpenForWriting` or `Invalidate` call carries.
const PATHS_PER_CALL: usize = 4096;

/// A listening front's stream: the calls the service sends it (the
/// response stream of its `Listen`) and the answers it sends back (fed by
/// a task reading its request stream).
pub(crate) struct Link {
    /// Taken (closed) when the link is dropped, which ends the stream.
    calls: Mutex<Option<tokio::sync::mpsc::Sender<Result<pb::FrontCall, Status>>>>,
    answers: Mutex<Receiver<pb::FrontAnswer>>,
}

impl Link {
    pub(crate) fn new(
        calls: tokio::sync::mpsc::Sender<Result<pb::FrontCall, Status>>,
        answers: Receiver<pb::FrontAnswer>,
    ) -> Self {
        Self {
            calls: Mutex::new(Some(calls)),
            answers: Mutex::new(answers),
        }
    }

    fn sender(&self) -> Option<tokio::sync::mpsc::Sender<Result<pb::FrontCall, Status>>> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Alive while the front's stream is open.
    fn alive(&self) -> bool {
        self.sender().is_some_and(|s| !s.is_closed())
    }

    /// Ends the stream (once any call in flight is done with it).
    fn close(&self) {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}

/// One listening front.
struct Linked {
    front: FrontInfo,
    link: Link,
}

/// The service's listening fronts. Cheap to clone.
#[derive(Clone)]
pub struct Fronts {
    inner: Arc<Inner>,
}

struct Inner {
    state: StateDir,
    links: Mutex<HashMap<u16, Arc<Linked>>>,
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

    /// Takes `link`, the stream `front` opened, replacing any older link
    /// of its slot.
    pub(crate) fn add(&self, front: FrontInfo, link: Link) {
        let linked = Arc::new(Linked { front, link });
        let old = self.links().insert(linked.front.slot, linked.clone());
        if let Some(old) = old {
            old.link.close();
        }
        self.inner.changed.notify_all();
    }

    /// Closes every link (the service is stopping): fronts reconnect to
    /// the next service.
    pub(crate) fn close_all(&self) {
        for (_, linked) in self.links().drain() {
            linked.link.close();
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
            links.retain(|slot, linked| {
                // A front that died, or stopped listening, ended its stream.
                let keep = live.contains(slot) && linked.link.alive();
                if !keep {
                    linked.link.close();
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

    fn snapshot(&self) -> Vec<Arc<Linked>> {
        let mut links: Vec<Arc<Linked>> = self.links().values().cloned().collect();
        links.sort_by_key(|l| l.front.slot);
        links
    }

    /// One call and its answer. A failed call drops the link.
    fn call(&self, linked: &Arc<Linked>, call: Call) -> Result<Answer, FrontsError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let result = {
            // One call at a time on a link.
            let answers = linked
                .link
                .answers
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let sent = linked.link.sender().is_some_and(|calls| {
                calls
                    .blocking_send(Ok(pb::FrontCall {
                        id,
                        call: Some((&call).into()),
                    }))
                    .is_ok()
            });
            if sent {
                match answers.recv_timeout(self.inner.timeout) {
                    Ok(answer) => Ok(answer),
                    Err(RecvTimeoutError::Timeout) => {
                        Err(format!("no answer within {:?}", self.inner.timeout))
                    }
                    Err(RecvTimeoutError::Disconnected) => Err("its stream ended".to_owned()),
                }
            } else {
                Err("its stream is closed".to_owned())
            }
        };
        let failed = |message: String| {
            let mut links = self.links();
            if links
                .get(&linked.front.slot)
                .is_some_and(|l| Arc::ptr_eq(l, linked))
            {
                links.remove(&linked.front.slot);
            }
            linked.link.close();
            FrontsError::Call {
                slot: linked.front.slot,
                pod: linked.front.origin.pod.clone(),
                message,
            }
        };
        match result {
            Ok(pb::FrontAnswer { id: got, answer }) if got == id => {
                match answer.map(Answer::try_from) {
                    Some(Ok(Answer::Error { message })) => Err(FrontsError::Call {
                        slot: linked.front.slot,
                        pod: linked.front.origin.pod.clone(),
                        message,
                    }),
                    Some(Ok(answer)) => Ok(answer),
                    Some(Err(e)) => Err(failed(format!("an answer the service cannot read: {e}"))),
                    None => Err(failed(format!("answered call {id} with nothing"))),
                }
            }
            Ok(pb::FrontAnswer { id: got, .. }) => {
                Err(failed(format!("answered call {got}, not call {id}")))
            }
            Err(e) => Err(failed(e)),
        }
    }

    fn links(&self) -> std::sync::MutexGuard<'_, HashMap<u16, Arc<Linked>>> {
        self.inner
            .links
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

fn unexpected(linked: &Linked, answer: &Answer) -> FrontsError {
    FrontsError::Call {
        slot: linked.front.slot,
        pod: linked.front.origin.pod.clone(),
        message: format!("unexpected answer {answer:?}"),
    }
}
