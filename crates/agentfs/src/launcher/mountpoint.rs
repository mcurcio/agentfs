//! What `mount` does with each view's mount point before anything is staged
//! (docs/trees.md, "Restarting after a crash"): one inside the state area is
//! refused; a live or foreign mount there is refused; a stale agentfs mount
//! (its daemon died) is refused, held or stacked over by `--on-stale-mount`,
//! and stacked over only where it is the view's own ([`stale::stackable`]);
//! a bare one is created when missing, refused when it holds files, and
//! sealed with `--seal-mountpoint`.

use std::path::{Path, PathBuf};

use agentfs_fuse::privileges::Privileges;
use agentfs_fuse::stale::{self, MountState, mount_state};
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

/// Which stale mount a view may stack over.
pub(super) enum Expect {
    /// Only one whose source is exactly this (the root view's).
    Source(String),
    /// A keyed fork's: its tree is known only once the fork is staged, so a
    /// fork mount of this volume is accepted here and the view host judges
    /// the exact source when the view opens.
    KeyedFork { volume: String },
    /// An unkeyed fork mount makes a new fork, which never stacks over a
    /// crashed fork's mount.
    UnkeyedFork,
}

/// Refuses a mount point inside the state area, or one holding it, before
/// anything is created: the workload would see the history.
///
/// # Errors
///
/// The refusal, naming the mount point.
pub(super) fn outside_the_state_area(state: &Path, views: &[&Path]) -> Result<()> {
    let state = resolved(state);
    for at in views {
        let at_resolved = resolved(at);
        if at_resolved.starts_with(&state) || state.starts_with(&at_resolved) {
            bail!(
                "the mount point {} is inside the state area {} (or holds it): the workload \
                 would see the history; mount it outside the state area",
                at.display(),
                state.display()
            );
        }
    }
    Ok(())
}

/// Judges every mount point in `views` (the root's first; `expect` beside
/// each), then prepares the bare ones. Nothing is created or sealed unless
/// every one is accepted.
///
/// # Errors
///
/// A refusal, naming the mount point and the fix.
pub(super) fn prepare(
    views: &[&Path],
    expect: &[Expect],
    policy: OnStaleMount,
    seal: bool,
) -> Result<Plan> {
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
                for (at, source) in &stale {
                    let index = views.iter().position(|view| view == at);
                    own_stale_mount(at, source, index.and_then(|i| expect.get(i)))?;
                }
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

/// Refuses stacking over a stale mount at `at` that is not the view's own.
fn own_stale_mount(at: &Path, source: &str, expect: Option<&Expect>) -> Result<()> {
    let own = match expect {
        Some(Expect::Source(expected)) => stale::stackable(source, expected),
        Some(Expect::KeyedFork { volume }) => source.starts_with(&format!("agentfs:{volume}:")),
        Some(Expect::UnkeyedFork) => bail!(
            "{p} is a stale agentfs mount ({source}); a fork mount without --fork-key makes a \
             new fork, which never stacks over a crashed fork's mount: give the crashed fork's \
             --fork-key to resume it, or detach it first: umount -l {p}",
            p = at.display()
        ),
        None => false,
    };
    if !own {
        bail!(
            "{p} is a stale agentfs mount of another volume or tree ({source}): a view stacks \
             only over its own; detach it first: umount -l {p}",
            p = at.display()
        );
    }
    Ok(())
}

/// A bare mount point: created when missing, refused when it holds files
/// (the mount would hide them, and nothing records them), sealed with
/// `seal`.
fn bare(at: &Path, seal: bool) -> Result<()> {
    mountpoint::create(at)?;
    let mut entries = std::fs::read_dir(at)
        .with_context(|| format!("reading the mount point {}", at.display()))?;
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

/// `path` as the kernel names it ([`stale::resolved_mountpoint`]), or as
/// given, made absolute, where neither it nor its parent exists yet.
pub(super) fn resolved(path: &Path) -> PathBuf {
    stale::resolved_mountpoint(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf())
}
