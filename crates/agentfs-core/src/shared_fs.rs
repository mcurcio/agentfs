//! Which filesystems a state area may live on (design §2.3, §14.2).
//!
//! The state lock ([`crate::lock::StateLock`]) is the only thing that keeps
//! a second history service off a volume. On NFS it holds across nodes
//! only when the server enforces it: NFS 4.1 or later, without `nolock`
//! and with `local_lock=none`. [`require_safe_locks`] is checked whenever
//! the lock is taken (CLI and sidecar state areas, F7 decision).
//!
//! [`network_filesystem`] tells a node's own disk from a network
//! filesystem: agentfs-csi keeps every worktree on its node's disk and
//! refuses a `--base-path` on a network filesystem at start (F8 R1,
//! design §14.2).

use std::path::{Path, PathBuf};

use crate::mountinfo::{self, MountEntry};

/// A filesystem whose locks would not keep another node out.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnsafeLocks {
    #[error(
        "{} is on NFS version {version} ({export}): agentfs needs NFS 4.1 or later, whose server enforces every client's locks (design §14.2)",
        path.display()
    )]
    NfsVersion {
        path: PathBuf,
        export: String,
        version: String,
    },
    #[error(
        "{} is on NFS mounted with {option} ({export}): its locks exclude only processes on this node, so a second history service could run on another (design §14.2)",
        path.display()
    )]
    LocalLocks {
        path: PathBuf,
        export: String,
        option: String,
    },
}

// No field may be named `source`: thiserror would take it as the error's
// source.
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    #[error(transparent)]
    Unsafe(#[from] UnsafeLocks),
    #[error("finding the filesystem of {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Whether the locks of the filesystem `entry` mounts, which holds `path`,
/// exclude other nodes. NFS below 4.1 is refused (a missing or unparsable
/// `vers` on `nfs4` too), then `local_lock` other than `none` and `nolock`.
/// Any other filesystem passes.
pub fn assess(entry: &MountEntry, path: &Path) -> Result<(), UnsafeLocks> {
    let nfs_version = |version: &str| UnsafeLocks::NfsVersion {
        path: path.to_path_buf(),
        export: entry.source.clone(),
        version: version.to_owned(),
    };
    match entry.fstype.as_str() {
        "nfs" => {
            return Err(nfs_version(entry.super_option("vers").unwrap_or("3")));
        }
        "nfs4" => {
            let vers = entry.super_option("vers");
            if !vers.and_then(major_minor).is_some_and(|v| v >= (4, 1)) {
                return Err(nfs_version(vers.unwrap_or("unknown")));
            }
        }
        _ => return Ok(()),
    }
    let local_locks = |option: String| UnsafeLocks::LocalLocks {
        path: path.to_path_buf(),
        export: entry.source.clone(),
        option,
    };
    if let Some(value) = entry.super_option("local_lock")
        && value != "none"
    {
        return Err(local_locks(format!("local_lock={value}")));
    }
    if entry.has_super_flag("nolock") {
        return Err(local_locks("nolock".into()));
    }
    Ok(())
}

/// `major.minor` as numbers.
fn major_minor(version: &str) -> Option<(u32, u32)> {
    let (major, minor) = version.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Refuses `path` (which must exist) when the filesystem holding it would
/// let another node take a lock this one holds: canonicalises it, finds
/// its mount in `/proc/self/mountinfo` and [`assess`]es that.
pub fn require_safe_locks(path: &Path) -> Result<(), CheckError> {
    let entry = mount_of(path)?;
    Ok(assess(&entry, path)?)
}

/// The filesystem types that are network filesystems: their files live on
/// another machine.
pub const NETWORK_FILESYSTEMS: [&str; 11] = [
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smbfs",
    "ceph",
    "fuse.glusterfs",
    "glusterfs",
    "lustre",
    "gpfs",
    "afs",
];

/// The mount `path` is on, when it is a network filesystem (nfs, nfs4, cifs, smb3, smbfs, ceph,
/// fuse.glusterfs, glusterfs, lustre, gpfs, afs). agentfs-csi refuses such a --base-path (R1). The
/// state lock's NFS 4.1 check for CLI and sidecar state areas is unchanged (F7 decision).
pub fn network_filesystem(path: &Path) -> Result<Option<MountEntry>, CheckError> {
    let entry = mount_of(path)?;
    Ok(is_network(&entry).then_some(entry))
}

/// Whether `entry` mounts a network filesystem ([`NETWORK_FILESYSTEMS`]).
pub fn is_network(entry: &MountEntry) -> bool {
    NETWORK_FILESYSTEMS.contains(&entry.fstype.as_str())
}

/// The mount holding `path` (which must exist), from
/// `/proc/self/mountinfo`.
fn mount_of(path: &Path) -> Result<MountEntry, CheckError> {
    let io_err = |source| CheckError::Io {
        path: path.to_path_buf(),
        source,
    };
    let resolved = path.canonicalize().map_err(io_err)?;
    let entries = mountinfo::read().map_err(io_err)?;
    match mountinfo::containing(&entries, &resolved) {
        Some(entry) => Ok(entry.clone()),
        None => Err(io_err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no mount holds it in /proc/self/mountinfo",
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(fstype: &str, options: &str) -> MountEntry {
        MountEntry {
            device: "0:60".into(),
            root: "/".into(),
            mountpoint: "/srv".into(),
            fstype: fstype.into(),
            source: "server:/export".into(),
            super_options: options.into(),
        }
    }

    fn check(fstype: &str, options: &str) -> Result<(), UnsafeLocks> {
        assess(&mount(fstype, options), Path::new("/srv/vol/state/meta"))
    }

    fn version_of(result: Result<(), UnsafeLocks>) -> String {
        match result {
            Err(UnsafeLocks::NfsVersion { version, .. }) => version,
            other => panic!("{other:?}"),
        }
    }

    fn option_of(result: Result<(), UnsafeLocks>) -> String {
        match result {
            Err(UnsafeLocks::LocalLocks { option, .. }) => option,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn nfs_older_than_4_1_is_refused() {
        assert_eq!(version_of(check("nfs", "rw,vers=3,hard,proto=tcp")), "3");
        assert_eq!(version_of(check("nfs", "rw,hard")), "3", "nfs is v2 or v3");
        let err = check("nfs4", "rw,vers=4.0,local_lock=none").unwrap_err();
        assert_eq!(version_of(Err(err.clone())), "4.0");
        let text = err.to_string();
        assert!(text.contains("NFS version 4.0"), "{text}");
        assert!(text.contains("server:/export"), "{text}");
        assert!(text.contains("/srv/vol/state/meta"), "{text}");
        assert_eq!(version_of(check("nfs4", "rw,local_lock=none")), "unknown");
        assert_eq!(version_of(check("nfs4", "rw,vers=4,local_lock=none")), "4");
        assert_eq!(version_of(check("nfs4", "rw,vers=x.y")), "x.y");
    }

    #[test]
    fn local_locks_are_refused() {
        for mode in ["flock", "posix", "all"] {
            let result = check("nfs4", &format!("rw,vers=4.2,local_lock={mode}"));
            assert_eq!(option_of(result.clone()), format!("local_lock={mode}"));
            let text = result.unwrap_err().to_string();
            assert!(text.contains(&format!("local_lock={mode}")), "{text}");
        }
        assert_eq!(
            option_of(check("nfs4", "rw,vers=4.1,nolock,local_lock=none")),
            "nolock"
        );
        assert_eq!(option_of(check("nfs4", "rw,vers=4.1,nolock")), "nolock");
    }

    #[test]
    fn safe_nfs_and_local_filesystems_pass() {
        check("nfs4", "rw,vers=4.1,hard,proto=tcp,local_lock=none").unwrap();
        check(
            "nfs4",
            "rw,vers=4.2,hard,proto=tcp,local_lock=none,addr=10.0.0.1",
        )
        .unwrap();
        for fstype in ["ext4", "xfs", "overlay", "tmpfs", "fuse.agentfs"] {
            check(fstype, "rw,relatime").unwrap();
        }
        // The deepest mount decides: a local bind of an NFS directory is
        // still NFS, and an NFS export under a local mount is NFS too.
        let entries = mountinfo::parse(
            "\
22 1 8:1 / / rw - ext4 /dev/sda1 rw
50 22 0:60 / /srv rw - nfs4 server:/export rw,vers=4.0,local_lock=none
51 22 0:60 /vol /bind rw - nfs4 server:/export rw,vers=4.0,local_lock=none
52 22 0:61 / /ok\\040here rw - nfs4 server:/other rw,vers=4.2,local_lock=none
",
        );
        let at = |p: &str| {
            let path = Path::new(p);
            assess(mountinfo::containing(&entries, path).unwrap(), path)
        };
        at("/home/x").unwrap();
        assert_eq!(version_of(at("/srv/vol/state")), "4.0");
        assert_eq!(version_of(at("/bind/state/meta")), "4.0");
        // An escaped mount point matches its decoded path.
        at("/ok here/state").unwrap();
        // This process's own filesystems: whatever they are, the check
        // runs and names the path on an I/O error.
        let missing = Path::new("/no/such/agentfs/dir");
        let err = require_safe_locks(missing).unwrap_err();
        assert!(matches!(err, CheckError::Io { .. }), "{err:?}");
        assert!(err.to_string().contains("/no/such/agentfs/dir"), "{err}");
    }

    /// F8 R1: agentfs-csi's base path must be a node's own disk.
    #[test]
    fn network_filesystems_are_told_apart() {
        for fstype in NETWORK_FILESYSTEMS {
            assert!(is_network(&mount(fstype, "rw")), "{fstype}");
        }
        // Whatever its version or lock options: NFS 4.2 is still remote.
        assert!(is_network(&mount("nfs4", "rw,vers=4.2,local_lock=none")));
        for fstype in [
            "ext4",
            "xfs",
            "btrfs",
            "zfs",
            "overlay",
            "tmpfs",
            "fuse.agentfs",
        ] {
            assert!(!is_network(&mount(fstype, "rw")), "{fstype}");
        }
        // A missing path is an error naming it (ci/e2e-nfs.sh refuses a
        // real NFS base path).
        let err = network_filesystem(Path::new("/no/such/agentfs/dir")).unwrap_err();
        assert!(err.to_string().contains("/no/such/agentfs/dir"), "{err}");
    }
}
