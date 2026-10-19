//! Ending a push at shutdown without leaving a remote broken (design
//! §11.2).
//!
//! As with git's receive-pack, a transfer may be dropped but a ref
//! update either completes or never starts. A push over the network
//! updates the remote's refs on the server, so a client that exits mid
//! push leaves the remote whole. A push to a path or `file://` remote runs
//! libgit2's local transport in this process: it writes the pack and then
//! takes `<remote>/refs/...lock` to move each ref. A process that exits
//! inside that step leaves the lock, and every later push of the ref is
//! refused until someone removes it.
//!
//! So [`Service::shutdown`](crate::service::Service::shutdown) cancels each
//! push at the grace's end. A push that has not finished its negotiation
//! stops there with nothing sent. A local push past its negotiation has no
//! point left where libgit2 can stop it, so [`PushStop::cancel`] says so
//! and the shutdown waits for it.

use std::sync::{Mutex, PoisonError};

use git2::Error as GitError;

/// What a push and the shutdown that may cancel it share. One lock, so a
/// cancel and a push entering its local update never both proceed.
#[derive(Debug, Default)]
pub struct PushStop(Mutex<State>);

#[derive(Debug, Default)]
struct State {
    cancelled: bool,
    /// A local push is past its negotiation: it writes the remote's pack
    /// and refs until it returns.
    updating: bool,
}

impl PushStop {
    /// Stops every attempt from sending anything from now on. Returns
    /// true if a local push is inside its update now: it cannot be
    /// stopped, and the process must not exit before it returns.
    pub fn cancel(&self) -> bool {
        let mut state = self.lock();
        state.cancelled = true;
        state.updating
    }

    /// At the end of a push's negotiation: an error if the push was
    /// cancelled (nothing is sent), else, for a `local` push, the update
    /// begins.
    pub(super) fn enter(&self, local: bool) -> Result<(), GitError> {
        let mut state = self.lock();
        if state.cancelled {
            return Err(GitError::from_str(
                "the push was cancelled at shutdown; nothing was sent",
            ));
        }
        state.updating = local;
        Ok(())
    }

    /// The push returned: its update, if any, is over.
    pub(super) fn leave(&self) {
        self.lock().updating = false;
    }

    /// Whether [`PushStop::cancel`] was called.
    #[cfg(test)]
    pub(crate) fn cancelled(&self) -> bool {
        self.lock().cancelled
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cancel_before_the_update_stops_it_and_one_during_it_waits() {
        let stop = PushStop::default();
        stop.enter(true).unwrap();
        assert!(stop.cancel(), "inside the update: wait for it");
        stop.leave();
        assert!(!stop.cancel());
        assert!(stop.enter(true).is_err(), "cancelled: nothing is sent");
    }

    #[test]
    fn a_network_push_is_never_waited_for() {
        let stop = PushStop::default();
        stop.enter(false).unwrap();
        assert!(!stop.cancel());
    }
}
