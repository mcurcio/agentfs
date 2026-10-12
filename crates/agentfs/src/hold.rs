//! `mount --on-stale-mount hold`: what a restarted sidecar does when its
//! previous daemon died and left the mount point stale (design §17.1).
//!
//! Phase 1 recovers a crashed daemon by restarting the pod. Remounting in
//! place is not safe: workload containers still hold the dead mount
//! (`ENOTCONN`), and detaching it while they run would let their writes
//! land in the bare directory beneath, where nothing records them. So the
//! restarted sidecar mounts nothing and takes no lock. It reports the
//! situation and waits:
//!
//! - its readiness probe (`agentfs probe`) fails, so the pod shows
//!   NotReady, and the workload's writes keep failing with `ENOTCONN`;
//! - its startup probe (`agentfs probe --allow-stale`) passes, so the
//!   kubelet does not kill it;
//! - at pod termination it gets SIGTERM after the workload containers have
//!   exited (native sidecars stop last), detaches the stale mount so the
//!   kubelet can tear the pod's volumes down, and exits 1. The next pod
//!   mounts afresh and recovers the journal.

use std::path::Path;
use std::process::ExitCode;

use agentfs_fuse::{MountState, detach, mount_state};

use crate::daemon::Daemon;

/// Holds until SIGINT or SIGTERM, then detaches `mountpoint` if it is
/// still a stale agentfs mount. Always exits 1: nothing was mounted.
pub fn hold(daemon: Daemon, mountpoint: &Path, source: &str) -> ExitCode {
    eprintln!(
        "{} is an agentfs mount ({source}) whose daemon died: not remounting while workloads \
         may still hold it. Phase 1 recovers by restarting the pod (kubectl delete pod <pod>); \
         its next start recovers the journal. Holding until SIGTERM, then detaching the stale \
         mount.",
        mountpoint.display()
    );
    let wake = daemon.wait();
    eprintln!("{}: stopping the hold", crate::describe_wake(wake));
    match mount_state(mountpoint) {
        Ok(MountState::Stale { .. }) => match detach(mountpoint) {
            Ok(()) => eprintln!("detached stale mount {}", mountpoint.display()),
            Err(e) => eprintln!(
                "could not detach the stale mount {}: {e}",
                mountpoint.display()
            ),
        },
        Ok(other) => eprintln!(
            "{} is no longer a stale agentfs mount ({}); leaving it",
            mountpoint.display(),
            crate::describe_state(&other)
        ),
        Err(e) => eprintln!("checking {}: {e}", mountpoint.display()),
    }
    ExitCode::FAILURE
}
