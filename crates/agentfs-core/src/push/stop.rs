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
//! and the shutdown waits for it, through the bookkeeping after its update
//! until the push thread says the attempt is over.
//!
//! One [`PushPhase`] under one lock: a cancel and a push changing phase never
//! interleave, and no moment of an attempt is in no phase.

use std::sync::{Mutex, PoisonError};

use git2::Error as GitError;

/// Where a remote's push thread is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PushPhase {
    /// Between attempts: sleeping, or deciding its final push from local
    /// state.
    #[default]
    Idle,
    /// Inside an attempt that may still be dropped: before the end of its
    /// negotiation, or any push over the network.
    Attempting,
    /// A local push past its negotiation, writing the remote's pack and
    /// refs.
    Updating,
    /// That update ended; the attempt's bookkeeping (its push state,
    /// alarms, any further step) runs until the thread ends the attempt.
    Finishing,
}

impl PushPhase {
    /// The shutdown must not exit while the push is here.
    pub fn must_wait(self) -> bool {
        matches!(self, Self::Updating | Self::Finishing)
    }
}

/// What a remote's push thread and the shutdown that may cancel it share.
#[derive(Debug, Default)]
pub struct PushStop(Mutex<State>);

#[derive(Debug, Default)]
struct State {
    cancelled: bool,
    phase: PushPhase,
    /// How many times [`PushStop::cancel`] was called.
    #[cfg(test)]
    cancels: u64,
}

impl PushStop {
    /// The push thread starts an attempt.
    pub fn begin_attempt(&self) {
        self.lock().phase = PushPhase::Attempting;
    }

    /// The push thread's attempt is over, its result handed on.
    pub fn end_attempt(&self) {
        self.lock().phase = PushPhase::Idle;
    }

    /// Where the push thread is.
    pub fn phase(&self) -> PushPhase {
        self.lock().phase
    }

    /// Stops every attempt from sending anything from now on, and says
    /// where the push thread is: if [`PushPhase::must_wait`], the process must
    /// not exit yet.
    pub fn cancel(&self) -> PushPhase {
        let mut state = self.lock();
        state.cancelled = true;
        #[cfg(test)]
        {
            state.cancels += 1;
        }
        state.phase
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
        if local {
            state.phase = PushPhase::Updating;
        }
        Ok(())
    }

    /// The push returned: an update in progress has ended, and its
    /// bookkeeping follows.
    pub(super) fn leave(&self) {
        let mut state = self.lock();
        if state.phase == PushPhase::Updating {
            state.phase = PushPhase::Finishing;
        }
    }

    /// Whether [`PushStop::cancel`] was called.
    #[cfg(test)]
    pub(crate) fn cancelled(&self) -> bool {
        self.lock().cancelled
    }

    /// How many times [`PushStop::cancel`] was called: each is one look
    /// by a shutdown past its deadline.
    #[cfg(test)]
    pub(crate) fn cancels(&self) -> u64 {
        self.lock().cancels
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_attempt_is_waited_for_from_its_update_to_its_end() {
        let stop = PushStop::default();
        stop.begin_attempt();
        assert_eq!(stop.phase(), PushPhase::Attempting);
        stop.enter(true).unwrap();
        assert!(stop.cancel().must_wait(), "inside the update");
        stop.leave();
        assert!(stop.cancel().must_wait(), "its bookkeeping");
        stop.end_attempt();
        assert_eq!(stop.cancel(), PushPhase::Idle);
        assert!(stop.enter(true).is_err(), "cancelled: nothing is sent");
    }

    #[test]
    fn a_network_push_is_never_waited_for() {
        let stop = PushStop::default();
        stop.begin_attempt();
        stop.enter(false).unwrap();
        assert_eq!(stop.cancel(), PushPhase::Attempting);
        stop.leave();
        assert_eq!(stop.cancel(), PushPhase::Attempting);
    }
}
