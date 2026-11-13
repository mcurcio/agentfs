//! The release verdict, pure (volume.md, VO1 and VO4): every blocker, from facts
//! asked at judgement time, and a [`Cleared`] only when there is none.

use agentfs_contracts::volume::{Blocker, Disposition, ReleaseFacts, ReleaseVerdict};

/// Proof that nothing blocks release: only [`decide`] builds one, and only a
/// `Cleared` moves the volume to `Released`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cleared {
    disposition: Disposition,
}

impl Cleared {
    pub fn disposition(&self) -> Disposition {
        self.disposition
    }
}

/// Every blocker, in the order of [`Blocker`]'s variants: `Staged` while staged;
/// `Attached` while any client is; `QueuesNotEmpty` while any queue holds an
/// entry; settlement `NoRemotes` never blocks. None: `Cleared` with `Keep`.
///
/// # Errors
///
/// The blockers, never empty.
pub fn decide(facts: &ReleaseFacts) -> Result<Cleared, Vec<Blocker>> {
    let _ = facts;
    todo!("S6")
}

/// The verdict `release_verdict` answers for a decision.
pub fn verdict(decided: &Result<Cleared, Vec<Blocker>>) -> ReleaseVerdict {
    let _ = decided;
    todo!("S6")
}
