//! `mount --on-stale-mount hold`: what a restarted sidecar does when its
//! previous daemon died and left the mount point, the sandbox overlay's
//! mount point, or both, stale (design §17.1).
//!
//! `--on-stale-mount recover` (the shipped sidecar's mode) mounts over the
//! stale mount instead (design §17.1); hold is for operators who want to
//! recover by hand. With hold, phase 1 recovers a crashed daemon by
//! restarting the pod. Detaching the dead mount
//! and mounting afresh is not safe: workload containers still hold the
//! dead mount (`ENOTCONN`), and detaching it while they run would let their
//! writes land in the bare directory beneath, where nothing records them
//! (recover stacks over it instead). So the restarted sidecar mounts
//! nothing and takes no lock. It reports the situation and waits:
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

use agentfs_fuse::{MountState, detach_stale_layers, mount_state};

use crate::STALE_LAYERS;
use crate::daemon::Daemon;

/// Holds until SIGINT or SIGTERM, then detaches each mount point in
/// `stale` (the mount point, the sandbox overlay's, or both, each with the
/// source of its stale agentfs mount) that is still a stale agentfs mount.
/// Always exits 1: nothing was mounted.
pub fn hold(daemon: Daemon, stale: &[(&Path, &str)]) -> ExitCode {
    for (mountpoint, source) in stale {
        eprintln!(
            "{} is an agentfs mount ({source}) whose daemon died: not remounting while \
             workloads may still hold it.",
            mountpoint.display()
        );
    }
    eprintln!(
        "Phase 1 recovers by restarting the pod (kubectl delete pod <pod>); its next start \
         recovers the journal. Holding until SIGTERM, then detaching the stale mount(s)."
    );
    let wake = daemon.wait();
    eprintln!("{}: stopping the hold", crate::describe_wake(wake));
    for (mountpoint, _) in stale {
        detach(mountpoint);
    }
    ExitCode::FAILURE
}

/// Detaches every stale agentfs layer at `mountpoint`, if it is still one.
fn detach(mountpoint: &Path) {
    match mount_state(mountpoint) {
        // Every layer: crashes recovered by stacking leave one each.
        Ok(MountState::Stale { .. }) => match detach_stale_layers(mountpoint, STALE_LAYERS) {
            Ok(n) => eprintln!(
                "detached stale mount {} ({n} layer(s))",
                mountpoint.display()
            ),
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
}
