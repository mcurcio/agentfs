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
//! Slot 0 belongs to the holder of the state-area lock (single-process mode
//! or the history service), so leases run from 1 to [`MAX_SLOT`].

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{FlockOperation, flock};

use crate::push::utc_basic;
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
    _file: File,
    slot: u16,
}

impl FrontLease {
    /// Leases the lowest free slot of `state`, writing `description` (who
    /// holds it) into the lease file for operators.
    pub fn acquire(state: &StateDir, description: &str) -> Result<Self, LeaseError> {
        let dir = state.fronts_dir();
        fs::create_dir_all(&dir).map_err(|source| LeaseError::Io {
            path: dir.clone(),
            source,
        })?;
        for slot in 1..=MAX_SLOT {
            let path = state.front_lease(slot);
            let io_err = |source| LeaseError::Io {
                path: path.clone(),
                source,
            };
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&path)
                .map_err(io_err)?;
            if !try_lock(&file, FlockOperation::NonBlockingLockExclusive).map_err(io_err)? {
                continue;
            }
            let since = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let line = format!(
                "pid={} {description} since={}\n",
                std::process::id(),
                utc_basic(since)
            );
            (|| {
                file.set_len(0)?;
                file.seek(SeekFrom::Start(0))?;
                file.write_all(line.as_bytes())
            })()
            .map_err(io_err)?;
            return Ok(Self { _file: file, slot });
        }
        Err(LeaseError::Full {
            state: state.root().to_path_buf(),
        })
    }

    /// The leased slot.
    pub fn slot(&self) -> u16 {
        self.slot
    }

    /// The slots some process holds a lease on now. A slot being leased or
    /// given up concurrently may land on either side.
    pub fn live(state: &StateDir) -> io::Result<HashSet<u16>> {
        Ok(Self::scan(state)?
            .into_iter()
            .map(|(slot, _)| slot)
            .collect())
    }

    /// The live slots and their holders' descriptions (`pid=… <description>
    /// since=…`), in slot order, for `agentfs status`.
    pub fn holders(state: &StateDir) -> io::Result<Vec<(u16, String)>> {
        let mut holders: Vec<(u16, String)> = Self::scan(state)?
            .into_iter()
            .map(|(slot, mut file)| {
                let mut text = String::new();
                let _ = (&mut file).take(512).read_to_string(&mut text);
                (slot, text.trim().to_owned())
            })
            .collect();
        holders.sort();
        Ok(holders)
    }

    fn scan(state: &StateDir) -> io::Result<Vec<(u16, File)>> {
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
            let file = match File::open(entry.path()) {
                Ok(file) => file,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            // A shared probe: it fails only against a holder's exclusive
            // lock, and never blocks a front's own acquire for long.
            if try_lock(&file, FlockOperation::NonBlockingLockShared)? {
                flock(&file, FlockOperation::Unlock)?;
            } else {
                live.push((slot, file));
            }
        }
        Ok(live)
    }
}

/// Takes `op` on `file`: `true` if taken, `false` if someone else holds a
/// conflicting lock.
fn try_lock(file: &File, op: FlockOperation) -> io::Result<bool> {
    loop {
        match flock(file, op) {
            Ok(()) => return Ok(true),
            Err(rustix::io::Errno::INTR) => continue,
            Err(rustix::io::Errno::WOULDBLOCK) => return Ok(false),
            Err(e) => return Err(e.into()),
        }
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
    fn live_ignores_other_files() {
        let (_dir, state) = state();
        fs::create_dir_all(state.fronts_dir()).unwrap();
        fs::write(state.front_repairs(3), "x").unwrap();
        fs::write(state.fronts_dir().join("slot-x.lock"), "").unwrap();
        assert!(FrontLease::live(&state).unwrap().is_empty());
    }
}
