//! What `mount` does with each view's mount point before anything is staged
//! (docs/trees.md, "Restarting after a crash"): a live or foreign mount
//! there is refused; a stale agentfs mount (its daemon died) is refused,
//! held or stacked over by `--on-stale-mount`; a bare one is created when
//! missing, refused when it holds files, and sealed with
//! `--seal-mountpoint`.

use std::path::{Path, PathBuf};

use agentfs_fuse::privileges::Privileges;
use agentfs_fuse::stale::{MountState, mount_state};
use anyhow::{Context as _, Result, bail};

use super::OnStaleMount;
use crate::mountpoint;

/// What `mount` does next.
pub(super) enum Plan {
    /// Mount every view; `true` where it stacks over a stale mount.
    Mount(Vec<bool>),
    /// `--on-stale-mount hold`: mount nothing; these are stale.
    Hold(Vec<(PathBuf, String)>),
}

/// Judges every mount point in `views` (the root's first), then prepares
/// the bare ones. Nothing is created or sealed unless every one is
/// accepted.
///
/// # Errors
///
/// A refusal, naming the mount point and the fix.
pub(super) fn prepare(views: &[&Path], policy: OnStaleMount, seal: bool) -> Result<Plan> {
    let mut stale = Vec::new();
    for at in views {
        let found = mount_state(at).with_context(|| format!("checking {}", at.display()))?;
        match found {
            MountState::NotMounted => {}
            MountState::Live { source } => bail!(
                "{} is already mounted (a live agentfs mount, {source}): one mount per mount point",
                at.display()
            ),
            MountState::Foreign { fstype } => bail!(
                "{} has another filesystem mounted on it ({fstype})",
                at.display()
            ),
            MountState::Stale { source } => stale.push((at.to_path_buf(), source)),
        }
    }
    if !stale.is_empty() {
        match policy {
            OnStaleMount::Refuse => {
                let (at, source) = &stale[0];
                bail!(
                    "{p} is a stale agentfs mount ({source}) whose daemon died: start with \
                     --on-stale-mount recover (stack over it) or hold (wait for a pod restart), \
                     or detach it first: umount -l {p}",
                    p = at.display()
                );
            }
            OnStaleMount::Hold => return Ok(Plan::Hold(stale)),
            OnStaleMount::Recover => {
                if !Privileges::current().can_unmount() {
                    let (at, source) = &stale[0];
                    bail!(
                        "--on-stale-mount recover: stacking a mount needs CAP_SYS_ADMIN (run as \
                         root); {p} is a stale agentfs mount ({source}) and is left as it is \
                         (use --on-stale-mount hold and restart the pod, or umount -l {p})",
                        p = at.display()
                    );
                }
            }
        }
    }
    let mut stacked = Vec::with_capacity(views.len());
    for at in views {
        let over = stale.iter().any(|(path, _)| path == at);
        if !over {
            bare(at, seal)?;
        }
        stacked.push(over);
    }
    Ok(Plan::Mount(stacked))
}

/// A bare mount point: created when missing, refused when it holds files
/// (the mount would hide them, and nothing records them), sealed with
/// `seal`.
fn bare(at: &Path, seal: bool) -> Result<()> {
    mountpoint::create(at)?;
    let mut entries =
        std::fs::read_dir(at).with_context(|| format!("reading the mount point {}", at.display()))?;
    if entries.next().is_some() {
        bail!(
            "the mount point {} is not empty: files in it would be hidden by the mount and never \
             recorded (a workload wrote there while no daemon was mounted?); move them out first",
            at.display()
        );
    }
    if seal {
        mountpoint::seal(at)?;
    }
    Ok(())
}

/// `path` as the kernel names it: canonical, or through its parent where
/// the path cannot be reached (a stale mount answers `ENOTCONN`).
pub(super) fn resolved(path: &Path) -> PathBuf {
    if let Ok(path) = path.canonicalize() {
        return path;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => parent
            .canonicalize()
            .map_or_else(|_| path.to_path_buf(), |parent| parent.join(name)),
        _ => std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()),
    }
}
