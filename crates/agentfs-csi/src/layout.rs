//! Where the driver keeps things (design §14.2).
//!
//! ```text
//! <base>/                         --base-path: one filesystem, the same path on
//!                                 the controller and every node
//!   <handle>/                     a volume (the PV name), 0700
//!     backing/                    the files the pods see
//!     state/                      the state area (design §2.2)
//!     csi.json                    the controller's record of the volume
//!   .nodes/<node-id>              a node plugin is registered (ControllerPublish checks)
//!   .deleting.<handle>/           a volume DeleteVolume is removing
//! <node-state>/                   --node-state: on the node, survives plugin restarts
//!   run/<handle>/history.sock     the volume's history service (directory 0700)
//!   staged/<handle>.json          a staged volume: its service runs
//!   published/<handle>/<key>.json a published target: its front runs
//! <mount-dir>/<handle>/<key>      --mount-dir: each front's own FUSE mount,
//!                                 bind-mounted onto the kubelet's target
//! <profiles>/<profile>.config|.exclude|.attributes
//! ```
//!
//! `<key>` is [`target_key`]: a hash of the kubelet's target path.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use agentfs_core::config::validate_name;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    #[error(
        "volume id {0:?} is not a valid agentfs-csi volume handle ([A-Za-z0-9][A-Za-z0-9._-]*, \
         no '..', no .lock suffix)"
    )]
    BadHandle(String),
    #[error("node id {0:?} is not a valid node name")]
    BadNode(String),
}

/// The driver's directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// `--base-path`: the volumes.
    pub base: PathBuf,
    /// `--node-state`: node records and service sockets.
    pub node_state: PathBuf,
    /// `--mount-dir`: the fronts' own mounts.
    pub mount_dir: PathBuf,
    /// `--profiles-dir`: the operator's profiles.
    pub profiles: PathBuf,
}

impl Layout {
    pub fn volume_dir(&self, handle: &str) -> PathBuf {
        self.base.join(handle)
    }

    /// The volume's state area.
    pub fn state(&self, handle: &str) -> PathBuf {
        self.volume_dir(handle).join("state")
    }

    /// The volume's backing tree.
    pub fn backing(&self, handle: &str) -> PathBuf {
        self.volume_dir(handle).join("backing")
    }

    /// The controller's record of the volume; the volume exists while it
    /// does.
    pub fn csi_json(&self, handle: &str) -> PathBuf {
        self.volume_dir(handle).join("csi.json")
    }

    /// Where a retained volume's record goes after DeleteVolume.
    pub fn retained_json(&self, handle: &str) -> PathBuf {
        self.volume_dir(handle).join("csi.retained.json")
    }

    /// Where DeleteVolume (`onDelete: delete`) moves a volume before
    /// removing its files. Handles start with an alphanumeric, so this
    /// never names a volume.
    pub fn deleting_dir(&self, handle: &str) -> PathBuf {
        self.base.join(format!(".deleting.{handle}"))
    }

    pub fn nodes_dir(&self) -> PathBuf {
        self.base.join(".nodes")
    }

    /// Present while a node plugin with this id has started.
    pub fn node_marker(&self, node: &str) -> PathBuf {
        self.nodes_dir().join(node)
    }

    /// The directory of the volume's history service socket (0700).
    pub fn run_dir(&self, handle: &str) -> PathBuf {
        self.node_state.join("run").join(handle)
    }

    /// The volume's history service socket, node-local.
    pub fn socket(&self, handle: &str) -> PathBuf {
        self.run_dir(handle).join("history.sock")
    }

    pub fn staged_dir(&self) -> PathBuf {
        self.node_state.join("staged")
    }

    pub fn staged_record(&self, handle: &str) -> PathBuf {
        self.staged_dir().join(format!("{handle}.json"))
    }

    pub fn published_root(&self) -> PathBuf {
        self.node_state.join("published")
    }

    pub fn published_dir(&self, handle: &str) -> PathBuf {
        self.published_root().join(handle)
    }

    pub fn published_record(&self, handle: &str, key: &str) -> PathBuf {
        self.published_dir(handle).join(format!("{key}.json"))
    }

    /// Where the front for `key` mounts the volume, before the bind mount
    /// onto the target.
    pub fn front_source(&self, handle: &str, key: &str) -> PathBuf {
        self.mount_dir.join(handle).join(key)
    }

    /// `<profiles>/<profile>.<kind>` (`config`, `exclude`, `attributes`).
    pub fn profile_file(&self, profile: &str, kind: &str) -> PathBuf {
        self.profiles.join(format!("{profile}.{kind}"))
    }
}

/// A volume handle is a directory name and an agentfs name component.
pub fn validate_handle(handle: &str) -> Result<(), LayoutError> {
    validate_name("volume id", handle).map_err(|_| LayoutError::BadHandle(handle.to_owned()))
}

/// A node id names a marker file.
pub fn validate_node(node: &str) -> Result<(), LayoutError> {
    validate_name("node id", node).map_err(|_| LayoutError::BadNode(node.to_owned()))
}

/// The first 16 hex digits of the SHA-256 of `target`: a short, stable
/// file name for a kubelet target path.
pub fn target_key(target: &Path) -> String {
    let digest = Sha256::digest(target.as_os_str().as_encoded_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_are_safe_directory_names() {
        for good in ["pvc-0b1c", "sanity-node-full-1-ABC", "a.b_c"] {
            validate_handle(good).unwrap();
        }
        for bad in [
            "", "a/b", "..", "a..b", ".nodes", "x.lock", "-x", "a b", "a.",
        ] {
            assert!(validate_handle(bad).is_err(), "{bad:?}");
        }
        validate_node("kind-control-plane").unwrap();
        assert!(validate_node("../x").is_err());
    }

    #[test]
    fn target_keys_are_stable_and_short() {
        let a = target_key(Path::new("/var/lib/kubelet/pods/1/volumes/x/mount"));
        let b = target_key(Path::new("/var/lib/kubelet/pods/2/volumes/x/mount"));
        assert_eq!(a.len(), 16);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        assert_eq!(
            a,
            target_key(Path::new("/var/lib/kubelet/pods/1/volumes/x/mount"))
        );
    }

    #[test]
    fn paths() {
        let l = Layout {
            base: "/b".into(),
            node_state: "/n".into(),
            mount_dir: "/m".into(),
            profiles: "/p".into(),
        };
        assert_eq!(l.state("v"), Path::new("/b/v/state"));
        assert_eq!(l.backing("v"), Path::new("/b/v/backing"));
        assert_eq!(l.csi_json("v"), Path::new("/b/v/csi.json"));
        assert_eq!(l.node_marker("n1"), Path::new("/b/.nodes/n1"));
        assert_eq!(l.socket("v"), Path::new("/n/run/v/history.sock"));
        assert_eq!(l.staged_record("v"), Path::new("/n/staged/v.json"));
        assert_eq!(
            l.published_record("v", "k"),
            Path::new("/n/published/v/k.json")
        );
        assert_eq!(l.front_source("v", "k"), Path::new("/m/v/k"));
        assert_eq!(l.profile_file("ci", "config"), Path::new("/p/ci.config"));
    }
}
