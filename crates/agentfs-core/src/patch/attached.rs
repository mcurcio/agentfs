//! Whether a workload may be attached when a daemon starts, so the patch
//! queue waits (design §12 "When it runs", §19.1 "No live landing yet"
//! and "Patches land after a clean stop").
//!
//! Patches land only at a start with nothing attached. A start lands them
//! only on evidence that nothing is: anything else defers the queue with
//! a [`Deferred`] reason, and the patches stay queued for the next start.
//!
//! - **A single-process mount** judges by its own stops. It writes the
//!   attached mark ([`mark`], `meta/attached.json`) before the mount can
//!   be seen and removes it ([`clear`]) only after an unmount that was not
//!   busy, with no open file left and no stale layer below. The mark is
//!   left by a crash, a kill, a lazy detach or files still open at
//!   shutdown, so a start that finds it defers ([`Deferred::UncleanStop`]).
//!   A state area that was never mounted has no mark.
//! - **A history service** defers while fronts hold leases, or when its
//!   launcher says a workload is attached (the CSI node plugin, while a
//!   pod of the volume is published on its node).

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::fsutil::write_json_atomic;
use crate::volume::StateDir;

/// Why a start does not land the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deferred {
    /// The previous mount did not stop with nothing attached: it was
    /// killed or crashed, its unmount was busy (a lazy detach), files were
    /// still open, or stale layers were left under it.
    UncleanStop,
    /// This mount stacks over a stale mount a crashed daemon left, which
    /// workloads may still hold.
    StaleMount,
    /// Fronts hold leases on the history service's volume.
    FrontsRunning,
    /// The launcher said a workload is attached (`serve-history
    /// --workload-attached`).
    WorkloadAttached,
}

impl Deferred {
    /// The reason's label, in the `PatchQueueDeferred` event.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UncleanStop => "unclean_stop",
            Self::StaleMount => "stale_mount",
            Self::FrontsRunning => "fronts_running",
            Self::WorkloadAttached => "workload_attached",
        }
    }

    /// Why, in words.
    pub fn describe(self) -> &'static str {
        match self {
            Self::UncleanStop => {
                "the previous mount of this volume did not stop with nothing attached (killed, \
                 crashed, a busy unmount or open files), so a workload may still hold it"
            }
            Self::StaleMount => "this mount stacks over a stale mount a workload may still hold",
            Self::FrontsRunning => "fronts are running",
            Self::WorkloadAttached => "the launcher says a workload is attached",
        }
    }
}

/// The attached mark's content: who wrote it, for the operator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachedMark {
    pub pid: u32,
    /// When the mount started, in milliseconds since the epoch.
    pub since_ms: u64,
}

/// Whether the attached mark is there: the volume's last mount did not
/// stop with nothing attached. A mark that cannot be checked counts as
/// there.
pub fn marked(state: &StateDir) -> bool {
    !matches!(
        std::fs::symlink_metadata(state.attached_mark()),
        Err(e) if e.kind() == io::ErrorKind::NotFound
    )
}

/// Writes the attached mark, durably, before the mount can be seen.
pub fn mark(state: &StateDir) -> io::Result<()> {
    std::fs::create_dir_all(state.meta())?;
    let since_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    write_json_atomic(
        &state.attached_mark(),
        &AttachedMark {
            pid: std::process::id(),
            since_ms,
        },
    )
}

/// Removes the attached mark, durably: the mount stopped with nothing
/// attached.
pub fn clear(state: &StateDir) -> io::Result<()> {
    match std::fs::remove_file(state.attached_mark()) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }
    std::fs::File::open(state.meta())?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_state_area_has_no_mark_and_a_mark_lasts_until_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path());
        assert!(!marked(&state), "never mounted: nothing attached");
        mark(&state).unwrap();
        assert!(marked(&state));
        let back: AttachedMark =
            serde_json::from_slice(&std::fs::read(state.attached_mark()).unwrap()).unwrap();
        assert_eq!(back.pid, std::process::id());
        clear(&state).unwrap();
        assert!(!marked(&state));
        clear(&state).unwrap();
    }

    #[test]
    fn a_mark_that_cannot_be_checked_counts_as_there() {
        use std::os::unix::fs::PermissionsExt;
        if rustix::process::geteuid().is_root() {
            return; // root reads through the mode
        }
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path());
        std::fs::create_dir_all(state.meta()).unwrap();
        std::fs::set_permissions(state.meta(), std::fs::Permissions::from_mode(0o000)).unwrap();
        let seen = marked(&state);
        std::fs::set_permissions(state.meta(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(seen);
    }
}
