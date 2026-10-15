//! A front's lease on a pending slot (design §2.2, §14).
//!
//! Fronts freeze into one shared `pending/`, each in a slot of its own
//! ([`crate::pending::SLOT_SHIFT`]). A front leases its slot with an
//! exclusive `flock` on `meta/fronts/slot-N.lock` for as long as it runs;
//! the kernel releases it when the front exits, even after `kill -9`. The
//! history service treats a leased slot's copies as the front's own and
//! leaves them alone when it prunes `pending/`; an unleased slot's copies
//! are orphans like any other.
//!
//! Leases are [`HeldLock`]s, so a process never leases one slot twice and
//! a probe never drops a lease, also where `flock` is emulated with POSIX
//! locks (NFS, [`crate::lockfile`]). Holder lines name the node, since
//! pids collide across nodes.
//!
//! Slot 0 belongs to the holder of the state-area lock (single-process mode
//! or the history service), so leases run from 1 to [`MAX_SLOT`].

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::lockfile::{self, HeldLock};
use crate::volume::StateDir;

/// The highest slot a front can lease: the number of fronts one volume can
/// have at once.
pub const MAX_SLOT: u16 = 1023;

#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error("all {MAX_SLOT} front slots of {} are in use", state.display())]
    Full { state: PathBuf },
    #[error("leasing a front slot in {}", path.display())]
    Io { path: PathBuf, source: io::Error },
}

/// Held by a front for as long as it runs. Dropping it (or the process
/// exiting) frees the slot.
#[derive(Debug)]
pub struct FrontLease {
    // Held only for the lock it carries.
    _held: HeldLock,
    slot: u16,
}

impl FrontLease {
    /// Leases the lowest free slot of `state`, writing `description` (who
    /// holds it) into the lease file for operators.
    pub fn acquire(state: &StateDir, description: &str) -> Result<Self, LeaseError> {
        Self::acquire_preferring(state, description, None)
    }

    /// Leases `preferred` when it is a front slot (1..=[`MAX_SLOT`]) that
    /// no one holds, else the lowest free slot as [`FrontLease::acquire`]
    /// does.
    ///
    /// A front restarted after a crash prefers its predecessor's slot, so
    /// it drains that slot's repair queue (§3.5) under the same pod
    /// identity (design §14.2).
    pub fn acquire_preferring(
        state: &StateDir,
        description: &str,
        preferred: Option<u16>,
    ) -> Result<Self, LeaseError> {
        let dir = state.fronts_dir();
        fs::create_dir_all(&dir).map_err(|source| LeaseError::Io {
            path: dir.clone(),
            source,
        })?;
        if let Some(slot) = preferred.filter(|s| (1..=MAX_SLOT).contains(s))
            && let Some(lease) = Self::try_slot(state, slot, description)?
        {
            return Ok(lease);
        }
        for slot in 1..=MAX_SLOT {
            if let Some(lease) = Self::try_slot(state, slot, description)? {
                return Ok(lease);
            }
        }
        Err(LeaseError::Full {
            state: state.root().to_path_buf(),
        })
    }

    /// Leases `slot` if no one holds it, writing the holder line.
    fn try_slot(
        state: &StateDir,
        slot: u16,
        description: &str,
    ) -> Result<Option<Self>, LeaseError> {
        let path = state.front_lease(slot);
        let io_err = |source| LeaseError::Io {
            path: path.clone(),
            source,
        };
        let Some(mut held) = HeldLock::try_exclusive(&path, 0o600).map_err(io_err)? else {
            return Ok(None);
        };
        held.set_holder(&lockfile::holder_line(description))
            .map_err(io_err)?;
        Ok(Some(Self { _held: held, slot }))
    }

    /// The leased slot.
    pub fn slot(&self) -> u16 {
        self.slot
    }

    /// When `slot`'s current holder leased it: the lease file's mtime,
    /// which [`FrontLease::acquire`] sets by rewriting the file. Frozen
    /// copies in the slot older than this are a previous holder's.
    pub fn leased_at(state: &StateDir, slot: u16) -> io::Result<SystemTime> {
        fs::metadata(state.front_lease(slot))?.modified()
    }

    /// The slots some process holds a lease on now. A slot being leased or
    /// given up concurrently may land on either side.
    pub fn live(state: &StateDir) -> io::Result<HashSet<u16>> {
        Ok(Self::scan(state)?
            .into_iter()
            .map(|(slot, _)| slot)
            .collect())
    }

    /// The live slots and their holders' descriptions (`pid=… node=…
    /// <description> since=…`), in slot order, for `agentfs status`.
    pub fn holders(state: &StateDir) -> io::Result<Vec<(u16, String)>> {
        let mut holders = Self::scan(state)?;
        holders.sort();
        Ok(holders)
    }

    /// Every slot some process (this one included) holds, with its holder
    /// line. Never opens a lease this process holds, which would drop it
    /// where `flock` is emulated with POSIX locks ([`crate::lockfile`]).
    fn scan(state: &StateDir) -> io::Result<Vec<(u16, String)>> {
        let mut live = Vec::new();
        let entries = match fs::read_dir(state.fronts_dir()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(live),
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(slot) = name
                .to_str()
                .and_then(|n| n.strip_prefix("slot-"))
                .and_then(|n| n.strip_suffix(".lock"))
                .and_then(|n| n.parse::<u16>().ok())
            else {
                continue;
            };
            if let Some(line) = lockfile::probe(&entry.path())? {
                live.push((slot, line));
            }
        }
        Ok(live)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (tempfile::TempDir, StateDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path().join("state"));
        (dir, state)
    }

    #[test]
    fn fronts_lease_distinct_slots_from_one() {
        let (_dir, state) = state();
        let a = FrontLease::acquire(&state, "pod=a").unwrap();
        let b = FrontLease::acquire(&state, "pod=b").unwrap();
        assert_eq!((a.slot(), b.slot()), (1, 2));
        let text = fs::read_to_string(state.front_lease(1)).unwrap();
        assert!(text.contains("pod=a"), "{text}");
        assert!(text.contains(&format!("pid={}", std::process::id())));
    }

    #[test]
    fn live_lists_held_slots_and_a_dropped_slot_is_reused() {
        let (_dir, state) = state();
        assert!(FrontLease::live(&state).unwrap().is_empty());
        let a = FrontLease::acquire(&state, "a").unwrap();
        let b = FrontLease::acquire(&state, "b").unwrap();
        assert_eq!(FrontLease::live(&state).unwrap(), HashSet::from([1, 2]));

        drop(a);
        assert_eq!(FrontLease::live(&state).unwrap(), HashSet::from([2]));
        let c = FrontLease::acquire(&state, "c").unwrap();
        assert_eq!(c.slot(), 1);
        drop(b);
    }

    #[test]
    fn holders_name_who_holds_each_slot() {
        let (_dir, state) = state();
        let _a = FrontLease::acquire(&state, "pod=a").unwrap();
        let _b = FrontLease::acquire(&state, "pod=b").unwrap();
        let holders = FrontLease::holders(&state).unwrap();
        assert_eq!(holders.len(), 2);
        assert_eq!(holders[0].0, 1);
        assert!(holders[0].1.contains("pod=a"), "{holders:?}");
        assert!(holders[1].1.contains("pod=b"), "{holders:?}");
    }

    #[test]
    fn a_free_preferred_slot_is_taken() {
        let (_dir, state) = state();
        let a = FrontLease::acquire_preferring(&state, "a", Some(7)).unwrap();
        assert_eq!(a.slot(), 7);
        // A plain lease still starts from the lowest free slot.
        let b = FrontLease::acquire(&state, "b").unwrap();
        assert_eq!(b.slot(), 1);
        assert_eq!(FrontLease::live(&state).unwrap(), HashSet::from([1, 7]));
    }

    #[test]
    fn a_held_preferred_slot_falls_back_to_the_lowest_free() {
        let (_dir, state) = state();
        let _a = FrontLease::acquire(&state, "a").unwrap();
        let _b = FrontLease::acquire(&state, "b").unwrap();
        let c = FrontLease::acquire_preferring(&state, "c", Some(1)).unwrap();
        assert_eq!(c.slot(), 3);
    }

    #[test]
    fn out_of_range_preferences_are_ignored() {
        let (_dir, state) = state();
        let a = FrontLease::acquire_preferring(&state, "a", Some(0)).unwrap();
        assert_eq!(a.slot(), 1);
        let b = FrontLease::acquire_preferring(&state, "b", Some(MAX_SLOT + 1)).unwrap();
        assert_eq!(b.slot(), 2);
        assert!(!state.front_lease(0).exists());
        assert!(!state.front_lease(MAX_SLOT + 1).exists());
    }

    #[test]
    fn posix_leases_are_distinct_and_survive_probes() {
        // NFS emulates flock with POSIX locks (crate::lockfile).
        crate::lockfile::use_posix_locks_on_this_thread(true);
        let (_dir, state) = state();
        let a = FrontLease::acquire(&state, "pod=a").unwrap();
        let b = FrontLease::acquire(&state, "pod=b").unwrap();
        assert_eq!((a.slot(), b.slot()), (1, 2));
        for _ in 0..3 {
            assert_eq!(FrontLease::live(&state).unwrap(), HashSet::from([1, 2]));
        }
        let holders = FrontLease::holders(&state).unwrap();
        assert!(holders[0].1.contains(" node="), "{holders:?}");
        // The probes did not drop a's lease: slot 1 is still taken.
        let c = FrontLease::acquire_preferring(&state, "pod=c", Some(1)).unwrap();
        assert_eq!(c.slot(), 3);
        drop(a);
        assert_eq!(FrontLease::live(&state).unwrap(), HashSet::from([2, 3]));
        crate::lockfile::use_posix_locks_on_this_thread(false);
    }

    #[test]
    fn live_ignores_other_files() {
        let (_dir, state) = state();
        fs::create_dir_all(state.fronts_dir()).unwrap();
        fs::write(state.front_repairs(3), "x").unwrap();
        fs::write(state.fronts_dir().join("slot-x.lock"), "").unwrap();
        assert!(FrontLease::live(&state).unwrap().is_empty());
    }
}
