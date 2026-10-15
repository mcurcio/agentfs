//! Where workloads see the mount (design §5).
//!
//! `git worktree add` writes an absolute `gitdir:` into the new worktree's
//! `.git` file: a path in the workload's view of the mount. The engine maps
//! such targets exactly through [`View`]s ([`agentfs_core::nested`]); this
//! module finds the ones the daemon can know: the mount point itself, every
//! other mount of the same filesystem in the daemon's mount namespace (a
//! bind mount of the mount or of a directory in it, as `/proc/self/mountinfo`
//! lists them), and the paths an operator names with `--workload-mount`
//! (a container's `mountPath`, which only the pod spec knows).

use std::io;
use std::path::Path;

use agentfs_core::mountinfo::{self, MountEntry};
use agentfs_core::nested::View;

/// One `--workload-mount` value: `PATH`, where a workload sees the whole
/// tree, or `PATH=DIR`, where it sees the tree's directory `DIR` (a
/// `subPath`).
pub fn parse_workload_mount(value: &str) -> Result<View, String> {
    let (at, root) = value.split_once('=').unwrap_or((value, ""));
    View::new(at, root).ok_or_else(|| {
        format!(
            "{value}: expected an absolute path, optionally =DIR with DIR a directory in the tree"
        )
    })
}

/// The views of the mount at `mountpoint` that `/proc/self/mountinfo`
/// shows: every mount of the same device, at the tree directory it shows.
pub fn mounted_views(mountpoint: &Path) -> io::Result<Vec<View>> {
    let entries = mountinfo::read()?;
    let mountpoint = mountpoint.canonicalize()?;
    Ok(views_in(&entries, &mountpoint))
}

/// [`mounted_views`] over parsed mountinfo.
fn views_in(entries: &[MountEntry], mountpoint: &Path) -> Vec<View> {
    // The topmost mount at the mount point is this one.
    let Some(this) = entries.iter().rev().find(|e| e.mountpoint == mountpoint) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|e| e.device == this.device)
        .filter_map(|e| View::new(e.mountpoint.to_str()?, e.root.to_str()?))
        .fold(Vec::new(), |mut views, view| {
            if !views.contains(&view) {
                views.push(view);
            }
            views
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
40 22 0:44 / /agentfs/workspace rw,nosuid shared:9 - fuse.agentfs agentfs:v rw
41 22 0:44 /proj /src rw shared:9 - fuse.agentfs agentfs:v rw
42 22 0:44 / /with\\040space rw - fuse.agentfs agentfs:v rw
43 22 0:50 / /other rw - fuse.agentfs agentfs:w rw
garbage line
";

    #[test]
    fn every_mount_of_the_device_is_a_view() {
        let entries = mountinfo::parse(MOUNTINFO);
        let views = views_in(&entries, Path::new("/agentfs/workspace"));
        assert_eq!(
            views,
            [
                View::new("/agentfs/workspace", "").unwrap(),
                View::new("/src", "proj").unwrap(),
                View::new("/with space", "").unwrap(),
            ]
        );
        assert!(views_in(&entries, Path::new("/nowhere")).is_empty());
    }

    #[test]
    fn workload_mounts_parse() {
        assert_eq!(
            parse_workload_mount("/workspace").unwrap(),
            View::new("/workspace", "").unwrap()
        );
        assert_eq!(
            parse_workload_mount("/src/=proj/sub/").unwrap(),
            View::new("/src", "proj/sub").unwrap()
        );
        assert!(parse_workload_mount("workspace").is_err());
        assert!(parse_workload_mount("/w=../up").is_err());
    }
}
