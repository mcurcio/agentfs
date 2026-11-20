//! Stale agentfs mounts (docs/library/filesystem.md, "Stale mounts"): what
//! is mounted at a path ([`mount_state`]), clearing a mount whose daemon
//! died ([`detach`], [`detach_stale_layers`]), and stacking a new mount over
//! a stale one ([`prepare_stack_source`], [`stack`]). Shared by the
//! recording mount ([`crate::mount`]) and the tree view host
//! ([`crate::tree_view`]).
//!
//! An agentfs mount is a FUSE mount whose source starts `agentfs:`
//! ([`agentfs_core::mountinfo`]); [`source_volume`] reads the volume it names.
//! A daemon that dies leaves its mount answering `ENOTCONN`. Stacking mounts
//! FUSE at a private empty directory, bind-mounts it over the stale mount
//! (made shared, so propagation carries it into every copy of the stale
//! mount point: workload containers, subPath binds of the mount point itself;
//! a bind of a directory inside it is not reached) and detaches the private
//! directory; the stale layers below are detached once the new mount ends.

use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use agentfs_core::mountinfo::{self, MountEntry};

use crate::privileges::Privileges;

/// Most stale layers one detach pass removes (one per crash recovered by
/// stacking).
pub const STALE_LAYERS: usize = 16;

/// What every agentfs mount's source starts with.
pub(crate) const SOURCE_PREFIX: &str = "agentfs:";

/// The volume an agentfs mount's source names: `agentfs:<volume>` or
/// `agentfs:<volume>:<anything>`; `None` for any other source. A volume name
/// never holds `:`, so the first one ends it.
pub fn source_volume(source: &str) -> Option<&str> {
    let rest = source.strip_prefix(SOURCE_PREFIX)?;
    let volume = rest.split_once(':').map_or(rest, |(volume, _)| volume);
    (!volume.is_empty()).then_some(volume)
}

/// What is mounted at a path, as `agentfs probe` and the hold mode of a
/// restarted sidecar see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountState {
    /// An agentfs mount whose daemon answers.
    Live { source: String },
    /// An agentfs mount whose daemon is gone (`ENOTCONN`).
    Stale { source: String },
    /// Nothing is mounted at the path itself.
    NotMounted,
    /// Something other than agentfs is mounted there.
    Foreign { fstype: String },
}

/// What is mounted at `path`: the last (topmost) mountinfo entry for it,
/// and for an agentfs mount, whether its daemon answers a `statfs`. The
/// path is resolved without touching its last component when that fails,
/// since a stale mount answers every `stat` with `ENOTCONN`.
pub fn mount_state(path: &Path) -> io::Result<MountState> {
    mount_state_in(&mountinfo::read()?, path)
}

/// Whether a view whose source would be `expected` may stack over the stale
/// agentfs mount whose source is `found`: only over its own, the exact
/// source. A stale mount of another volume, or of another tree of this one
/// (a fork's at the root's mount point, or a crashed unkeyed fork's under a
/// new fork), is another mount point's, and stacking over it would show the
/// workload's open paths a different tree. The one judge of that rule: the
/// view host, the recording mount and `agentfs mount` all ask it.
#[must_use]
pub fn stackable(found: &str, expected: &str) -> bool {
    found == expected
}

/// `path` resolved as mountinfo names it: canonical, or through its parent
/// when the path itself cannot be reached (a stale mount answers
/// `ENOTCONN`). The one implementation of that rule.
///
/// # Errors
///
/// Neither the path nor its parent resolves.
pub fn resolved_mountpoint(path: &Path) -> io::Result<PathBuf> {
    Ok(match path.canonicalize() {
        Ok(p) => p,
        Err(_) => match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => {
                let parent = if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                };
                parent.canonicalize()?.join(name)
            }
            _ => std::path::absolute(path)?,
        },
    })
}

fn mount_state_in(entries: &[MountEntry], path: &Path) -> io::Result<MountState> {
    // A path whose parent does not exist yet has nothing mounted on it.
    let resolved = match resolved_mountpoint(path) {
        Ok(resolved) => resolved,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(MountState::NotMounted),
        Err(e) => return Err(e),
    };
    let Some(entry) = entries.iter().rev().find(|e| e.mountpoint == resolved) else {
        return Ok(MountState::NotMounted);
    };
    if !entry.is_agentfs() {
        return Ok(MountState::Foreign {
            fstype: entry.fstype.clone(),
        });
    }
    let source = entry.source.clone();
    match rustix::fs::statfs(&resolved) {
        Ok(_) => Ok(MountState::Live { source }),
        Err(rustix::io::Errno::NOTCONN) => Ok(MountState::Stale { source }),
        Err(e) => Err(e.into()),
    }
}

/// Detaches whatever is mounted at `path` lazily: `umount2(MNT_DETACH)`
/// with the privilege to unmount, `fusermount3 -u -z` otherwise. Used to
/// clear a stale agentfs mount.
pub fn detach(path: &Path) -> io::Result<()> {
    if Privileges::current().can_unmount() {
        return rustix::mount::unmount(path, rustix::mount::UnmountFlags::DETACH)
            .map_err(io::Error::from);
    }
    let out = Command::new(fusermount())
        .args(["-u", "-q", "-z", "--"])
        .arg(path)
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(
            String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        ))
    }
}

/// Detaches the stale agentfs mounts at `path`, topmost first, until the
/// topmost is not one, at most `max`; returns how many it detached. Each
/// crash of a recovered daemon leaves one stale layer under the mount
/// stacked over it.
pub fn detach_stale_layers(path: &Path, max: usize) -> io::Result<usize> {
    let mut n = 0;
    while n < max && matches!(mount_state(path)?, MountState::Stale { .. }) {
        detach(path)?;
        n += 1;
    }
    Ok(n)
}

/// Counts the agentfs mounts at `path` (stacked layers included).
pub fn agentfs_layers(path: &Path) -> io::Result<usize> {
    let resolved = resolved_mountpoint(path)?;
    Ok(mountinfo::read()?
        .iter()
        .filter(|e| e.mountpoint == resolved && e.is_agentfs())
        .count())
}

/// Prepares `source` for a stacked mount: clears stale mounts a crash
/// between mounting and detaching it left, and creates it (mode 0700) as
/// an empty directory.
pub fn prepare_stack_source(source: &Path) -> io::Result<()> {
    // The parent first: on a fresh state area it does not exist yet, and
    // looking for stale layers resolves the source through it.
    if let Some(parent) = source.parent() {
        std::fs::create_dir_all(parent)?;
    }
    detach_stale_layers(source, STALE_LAYERS)?;
    match std::fs::DirBuilder::new().mode(0o700).create(source) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    if std::fs::read_dir(source)?.next().is_some() {
        return Err(io::Error::other(format!(
            "{} is not empty",
            source.display()
        )));
    }
    Ok(())
}

/// Detaches the stale layers under every entry of `stack` (the state area's
/// directory of private stacking sources), whichever view ids this run
/// opens: a crash between mounting a stacked view's source and binding it
/// leaves a dead mount at its entry, which no later run might reuse. Empty
/// entries are removed. Answers the layers detached; a missing `stack` holds
/// none.
///
/// # Errors
///
/// The directory could not be read, or a layer could not be detached.
pub fn clear_stack_sources(stack: &Path) -> io::Result<usize> {
    let entries = match std::fs::read_dir(stack) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut detached = 0;
    for entry in entries {
        let source = entry?.path();
        detached += detach_stale_layers(&source, STALE_LAYERS)?;
        // Best effort: a live source (another layer) or a file stays.
        let _ = std::fs::remove_dir(&source);
    }
    Ok(detached)
}

/// Bind-mounts the mount at `source` over `target`, makes the new layer
/// shared (best effort: under a Bidirectional emptyDir it already is) so it
/// propagates, then detaches `source`.
pub fn stack(source: &Path, target: &Path) -> io::Result<()> {
    use rustix::mount::{MountPropagationFlags, UnmountFlags};
    rustix::mount::mount_bind(source, target)?;
    if let Err(e) = rustix::mount::mount_change(target, MountPropagationFlags::SHARED) {
        eprintln!(
            "agentfs: making {} shared failed ({e}); copies of the stale mount may not see the \
             new one",
            target.display()
        );
    }
    if let Err(e) = rustix::mount::unmount(source, UnmountFlags::DETACH) {
        // Take the new layer off again, so the caller can unmount the
        // source and leave everything as it was.
        let _ = rustix::mount::unmount(target, UnmountFlags::DETACH);
        return Err(e.into());
    }
    Ok(())
}

/// `fusermount3`, or `fusermount` where only the old one is installed.
pub(crate) fn fusermount() -> &'static str {
    ["fusermount3", "fusermount"]
        .into_iter()
        .find(|bin| Command::new(bin).arg("-V").output().is_ok())
        .unwrap_or("fusermount3")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn a_source_names_its_volume() {
        assert_eq!(source_volume("agentfs:t"), Some("t"));
        assert_eq!(source_volume("agentfs:t:fork/3"), Some("t"));
        assert_eq!(source_volume("agentfs:vol:sandbox-overlay"), Some("vol"));
        assert_eq!(source_volume("agentfs-trees"), None);
        assert_eq!(source_volume("agentfs:"), None);
        assert_eq!(source_volume("agentfs::x"), None);
        assert_eq!(source_volume("tmpfs"), None);
    }

    /// Only a view's own stale mount is stackable: the exact source, never
    /// another tree of the same volume.
    #[test]
    fn only_the_exact_source_is_stackable() {
        assert!(stackable("agentfs:t", "agentfs:t"));
        assert!(stackable("agentfs:t:fork/3", "agentfs:t:fork/3"));
        assert!(!stackable("agentfs:t:fork/3", "agentfs:t"));
        assert!(!stackable("agentfs:t", "agentfs:t:fork/3"));
        assert!(!stackable("agentfs:t:fork/2", "agentfs:t:fork/3"));
        assert!(!stackable("agentfs:u", "agentfs:t"));
    }

    #[test]
    fn mount_state_reads_the_topmost_entry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mnt = root.join("mnt");
        fs::create_dir(&mnt).unwrap();
        let entry = |fstype: &str, source: &str| MountEntry {
            device: "0:1".into(),
            root: "/".into(),
            mountpoint: mnt.clone(),
            fstype: fstype.into(),
            source: source.into(),
            super_options: "rw".into(),
        };
        assert_eq!(mount_state_in(&[], &mnt).unwrap(), MountState::NotMounted);
        // Only the path itself counts, not a mount above it.
        let above = MountEntry {
            mountpoint: root.clone(),
            ..entry("fuse.agentfs", "agentfs:v")
        };
        assert_eq!(
            mount_state_in(std::slice::from_ref(&above), &mnt).unwrap(),
            MountState::NotMounted
        );
        // A foreign mount on top of an agentfs one wins, and the reverse.
        let stack = [entry("fuse.agentfs", "agentfs:v"), entry("tmpfs", "tmpfs")];
        assert_eq!(
            mount_state_in(&stack, &mnt).unwrap(),
            MountState::Foreign {
                fstype: "tmpfs".into()
            }
        );
        // The directory answers statfs here, so an agentfs entry is live;
        // the stale case (ENOTCONN) needs a real dead mount (tree_view.rs's
        // root-only stale_view tests).
        let stack = [entry("tmpfs", "tmpfs"), entry("fuse.agentfs", "agentfs:v")];
        assert_eq!(
            mount_state_in(&stack, &mnt).unwrap(),
            MountState::Live {
                source: "agentfs:v".into()
            }
        );
        // A path through a symlink resolves to the mount point.
        std::os::unix::fs::symlink(&mnt, root.join("alias")).unwrap();
        assert!(matches!(
            mount_state_in(&stack, &root.join("alias")).unwrap(),
            MountState::Live { .. }
        ));
        // A missing last component is resolved through its parent.
        assert_eq!(
            mount_state_in(&stack, &root.join("missing")).unwrap(),
            MountState::NotMounted
        );
        // So is a path whose parent is missing too (a fresh state area's
        // stack source).
        assert_eq!(
            mount_state_in(&stack, &root.join("no/such/dir")).unwrap(),
            MountState::NotMounted
        );
        // This process's real mountinfo parses and has the root.
        assert!(matches!(
            mount_state(Path::new("/")).unwrap(),
            MountState::Foreign { .. } | MountState::NotMounted
        ));
    }

    #[test]
    fn stack_source_is_prepared_on_a_fresh_state_area() {
        // The parent (`<state>/meta`) does not exist yet: looking for stale
        // layers must not fail on it (it once did, so every first recover
        // failed with ENOENT).
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("state/meta/stack");
        prepare_stack_source(&source).unwrap();
        assert!(source.is_dir());
        // Again, over the empty directory it left.
        prepare_stack_source(&source).unwrap();
        fs::write(source.join("f"), "x").unwrap();
        assert!(prepare_stack_source(&source).is_err());
    }
}
