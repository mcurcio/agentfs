//! The release verdict, pure (docs/library/domains.md, VO1 and VO4): every blocker, from facts
//! asked at judgement time, and a [`Cleared`] only when there is none.

use agentfs_contracts::volume::{Blocker, Disposition, ReleaseFacts, ReleaseVerdict, VolumeState};
use alloc::vec::Vec;

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
    let mut blockers = Vec::new();
    if facts.state == VolumeState::Staged {
        blockers.push(Blocker::Staged);
    }
    if !facts.attached.is_empty() {
        blockers.push(Blocker::Attached(facts.attached.clone()));
    }
    if !facts.queued.is_empty() {
        blockers.push(Blocker::QueuesNotEmpty(facts.queued.clone()));
    }
    // `facts.settlement` is `NoRemotes` until M4 builds replication: settled.
    if blockers.is_empty() {
        Ok(Cleared {
            disposition: Disposition::Keep,
        })
    } else {
        Err(blockers)
    }
}

/// The verdict `release_verdict` answers for a decision.
pub fn verdict(decided: &Result<Cleared, Vec<Blocker>>) -> ReleaseVerdict {
    match decided {
        Ok(cleared) => ReleaseVerdict::Release(cleared.disposition()),
        Err(blockers) => ReleaseVerdict::Wait(blockers.clone()),
    }
}

#[cfg(test)]
mod tests {
    use agentfs_contracts::clients::{ClientId, ClientSet, Ordinal};
    use agentfs_contracts::name::{ClientLabel, LauncherId};
    use agentfs_contracts::tree::TreeId;
    use agentfs_contracts::volume::{Drain, Settlement};

    use super::*;

    fn client() -> ClientSet {
        ClientSet::from([ClientId {
            launcher: LauncherId::parse("cli").unwrap(),
            tree: TreeId::Root,
            label: ClientLabel::parse("root-client").unwrap(),
            n: Ordinal(3),
        }])
    }

    /// Every state, with and without a client attached and an entry queued:
    /// the exact blockers, in `Blocker`'s order, and a release only when
    /// there is none.
    #[test]
    fn decide_over_every_combination() {
        let states = [
            VolumeState::Staged,
            VolumeState::Draining(Drain::Stop),
            VolumeState::Released(Disposition::Keep),
        ];
        for state in states {
            for attached in [ClientSet::new(), client()] {
                for queued in [Vec::new(), vec![TreeId::Root]] {
                    let facts = ReleaseFacts {
                        state,
                        attached: attached.clone(),
                        queued: queued.clone(),
                        settlement: Settlement::NoRemotes,
                    };
                    let mut expected = Vec::new();
                    if state == VolumeState::Staged {
                        expected.push(Blocker::Staged);
                    }
                    if !attached.is_empty() {
                        expected.push(Blocker::Attached(attached.clone()));
                    }
                    if !queued.is_empty() {
                        expected.push(Blocker::QueuesNotEmpty(queued.clone()));
                    }
                    let decided = decide(&facts);
                    if expected.is_empty() {
                        assert_eq!(
                            decided.as_ref().map(Cleared::disposition),
                            Ok(Disposition::Keep)
                        );
                        assert_eq!(
                            verdict(&decided),
                            ReleaseVerdict::Release(Disposition::Keep)
                        );
                    } else {
                        assert_eq!(decided, Err(expected.clone()), "{facts:?}");
                        assert_eq!(verdict(&decided), ReleaseVerdict::Wait(expected));
                    }
                }
            }
        }
    }
}
