//! Preparing a bare mount point (docs/trees.md, "Restarting after a crash"):
//! creating a missing one and sealing it, shared by the tree launcher's
//! views and the recording engine's front.

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context as _, Result, bail};

/// Creates a missing mount point (one level: its parent must exist), mode
/// 0755, so a sidecar can mount into a subdirectory of a fresh emptyDir.
/// Anything already there is the caller's to judge.
pub fn create(mountpoint: &Path) -> Result<()> {
    match std::fs::symlink_metadata(mountpoint) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .mode(0o755)
                .create(mountpoint)
                .with_context(|| format!("creating the mount point {}", mountpoint.display()))?;
            eprintln!("created the mount point {}", mountpoint.display());
            Ok(())
        }
        _ => Ok(()),
    }
}

/// `--seal-mountpoint`: makes the bare mount point root's with mode 0000.
/// The mount on top is unaffected; what it hides is a directory no non-root
/// process (nor root without `CAP_DAC_OVERRIDE`) can write, so a workload
/// that binds it while no daemon is mounted (a `subPath` bound while the
/// sidecar restarts) gets EACCES instead of writing where nothing records
/// it.
pub fn seal(mountpoint: &Path) -> Result<()> {
    if !rustix::process::geteuid().is_root() {
        bail!(
            "--seal-mountpoint (AGENTFS_SEAL_MOUNTPOINT) needs root: the sealed mount point is \
             owned by root"
        );
    }
    std::os::unix::fs::chown(mountpoint, Some(0), Some(0))
        .and_then(|()| std::fs::set_permissions(mountpoint, std::fs::Permissions::from_mode(0o000)))
        .with_context(|| format!("sealing the mount point {}", mountpoint.display()))?;
    Ok(())
}
