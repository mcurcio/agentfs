//! A front's listening stream: the service calls the front (design §14.1,
//! §15).
//!
//! A restore or revert served by the history service writes the backing
//! tree that live fronts serve. Before it plans, the service needs each
//! front to record what is dirty in it (`Settle`) and to say which paths
//! it holds open for writing; after it writes, each front must drop its
//! kernel's cached entries, attributes and pages of the paths written
//! (`Invalidate`). [`HistoryClient::listen`] keeps one `Listen` stream open
//! on the front's connection for that: the service sends calls, which
//! [`FrontHooks`] answer, one at a time, on the listener's own thread
//! (never on a runtime worker).
//!
//! The listener opens the stream again by itself, with a backoff from
//! [`RECONNECT_MIN`] to [`RECONNECT_MAX`], when the service restarts or the
//! stream breaks.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::Notify;

use super::client::Listening;
use super::wire::pb;
use super::{Answer, Call, HistoryClient};
use crate::paths::RepoPath;

/// The first wait before reconnecting.
pub const RECONNECT_MIN: Duration = Duration::from_millis(100);
/// The longest wait before reconnecting.
pub const RECONNECT_MAX: Duration = Duration::from_secs(5);

/// What a front does when the service calls it.
pub trait FrontHooks: Send + Sync {
    /// Records what changed through the front and is not recorded yet
    /// (the sweep); returns how many were recorded and how many paths
    /// still wait for a repair. Returns once those records are journaled.
    fn settle(&self) -> Result<(usize, usize), String>;
    /// Whether each path has a handle open for writing through the front.
    fn open_for_writing(&self, paths: &[RepoPath]) -> Vec<bool>;
    /// The service changed these paths in the backing tree: drop the
    /// kernel's caches of each.
    fn invalidate(&self, paths: &[RepoPath]) -> Result<(), String>;
}

/// A front with nothing to settle, nothing open and no kernel caches:
/// `agentfs record --history-service`.
pub struct NoHooks;

impl FrontHooks for NoHooks {
    fn settle(&self) -> Result<(usize, usize), String> {
        Ok((0, 0))
    }

    fn open_for_writing(&self, paths: &[RepoPath]) -> Vec<bool> {
        vec![false; paths.len()]
    }

    fn invalidate(&self, _: &[RepoPath]) -> Result<(), String> {
        Ok(())
    }
}

/// Answers `call` with `hooks`.
pub fn answer(hooks: &dyn FrontHooks, call: &Call) -> Answer {
    match call {
        Call::Settle => match hooks.settle() {
            Ok((recorded, failed)) => Answer::Settled { recorded, failed },
            Err(message) => Answer::Error { message },
        },
        Call::OpenForWriting { paths } => Answer::Open {
            open: hooks.open_for_writing(paths),
        },
        Call::Invalidate { paths } => match hooks.invalidate(paths) {
            Ok(()) => Answer::Invalidated,
            Err(message) => Answer::Error { message },
        },
        Call::Ping => Answer::Pong,
    }
}

/// The running listener; stops it when dropped.
pub struct ListenerHandle {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

struct Shared {
    stop: AtomicBool,
    /// Wakes the listener from a blocked receive at stop.
    wake: Notify,
    /// Whether the service has acknowledged the current stream.
    listening: Mutex<bool>,
    changed: Condvar,
}

impl ListenerHandle {
    /// Waits up to `timeout` until the service has acknowledged a
    /// listening stream; whether it has.
    pub fn wait_listening(&self, timeout: Duration) -> bool {
        let listening = self
            .shared
            .listening
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *self
            .shared
            .changed
            .wait_timeout_while(listening, timeout, |l| !*l)
            .unwrap_or_else(PoisonError::into_inner)
            .0
    }

    /// Stops listening and waits for the thread.
    pub fn stop(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // A stored permit if the listener is not waiting now: it checks
        // `stop` before its next wait.
        self.shared.wake.notify_one();
        self.shared.changed.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ListenerHandle {
    fn drop(&mut self) {
        self.halt();
    }
}

impl HistoryClient {
    /// Keeps a stream open on which the service calls this front,
    /// answered by `hooks`, until the handle is dropped.
    pub fn listen(self: &Arc<Self>, hooks: Arc<dyn FrontHooks>) -> std::io::Result<ListenerHandle> {
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            wake: Notify::new(),
            listening: Mutex::new(false),
            changed: Condvar::new(),
        });
        let thread = {
            let (client, shared) = (self.clone(), shared.clone());
            std::thread::Builder::new()
                .name("agentfs-listen".into())
                .spawn(move || run(&client, hooks.as_ref(), &shared))?
        };
        Ok(ListenerHandle {
            shared,
            thread: Some(thread),
        })
    }
}

fn run(client: &HistoryClient, hooks: &dyn FrontHooks, shared: &Shared) {
    let mut backoff = RECONNECT_MIN;
    while !shared.stop.load(Ordering::SeqCst) {
        if let Some(stream) = turn_around(client, shared) {
            backoff = RECONNECT_MIN;
            serve(client, stream, hooks, shared);
            set_listening(shared, false);
        } else {
            backoff = (backoff * 2).min(RECONNECT_MAX);
        }
        // Interruptible by stop.
        let listening = shared
            .listening
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let _ = shared
            .changed
            .wait_timeout_while(listening, backoff, |_| !shared.stop.load(Ordering::SeqCst));
    }
}

fn set_listening(shared: &Shared, value: bool) {
    *shared
        .listening
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = value;
    shared.changed.notify_all();
}

/// A new listening stream: the service sends calls on it now.
fn turn_around(client: &HistoryClient, shared: &Shared) -> Option<Listening> {
    if shared.stop.load(Ordering::SeqCst) {
        return None;
    }
    let stream = client.bridge().block_on(async {
        tokio::select! {
            opened = client.open_listen() => opened.ok(),
            () = shared.wake.notified() => None,
        }
    })?;
    if shared.stop.load(Ordering::SeqCst) {
        return None;
    }
    set_listening(shared, true);
    Some(stream)
}

/// Answers the service's calls until the stream ends or the listener stops.
fn serve(client: &HistoryClient, mut stream: Listening, hooks: &dyn FrontHooks, shared: &Shared) {
    while !shared.stop.load(Ordering::SeqCst) {
        // Calls come when the service has a restore to serve: no timeout.
        let next = client.bridge().block_on(async {
            tokio::select! {
                next = stream.calls.message() => next.ok().flatten(),
                () = shared.wake.notified() => None,
            }
        });
        let Some(pb::FrontCall { id, call }) = next else {
            return;
        };
        let answer = match call.map(Call::try_from) {
            Some(Ok(call)) => answer(hooks, &call),
            Some(Err(e)) => Answer::Error {
                message: format!("a call this front cannot read: {e}"),
            },
            None => Answer::Error {
                message: "a call with nothing to do".into(),
            },
        };
        let reply = pb::FrontAnswer {
            id,
            answer: Some((&answer).into()),
        };
        if client
            .bridge()
            .block_on(stream.answers.send(reply))
            .is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_hooks_answer_every_call() {
        let paths = vec![RepoPath::new("a").unwrap(), RepoPath::new("b").unwrap()];
        assert_eq!(
            answer(&NoHooks, &Call::Settle),
            Answer::Settled {
                recorded: 0,
                failed: 0
            }
        );
        assert_eq!(
            answer(
                &NoHooks,
                &Call::OpenForWriting {
                    paths: paths.clone()
                }
            ),
            Answer::Open {
                open: vec![false, false]
            }
        );
        assert_eq!(
            answer(&NoHooks, &Call::Invalidate { paths }),
            Answer::Invalidated
        );
        assert_eq!(answer(&NoHooks, &Call::Ping), Answer::Pong);
    }
}
