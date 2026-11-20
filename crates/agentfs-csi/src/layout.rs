//! Where the driver keeps things (docs/csi.md).
//!
//! ```text
//! <base>/                         --base-path: a directory on this node's own
//!                                 disk, never a network filesystem
//!   <handle>/                     a claim's volume directory (the PV name), 0700
//!     state/                      the tree state area (history in state/history.git)
//!     csi.json                    the claim record (CreateVolume, or a static
//!                                 PV over a kept volume)
//!     csi.retained.json           the record once DeleteVolume kept the volume
//!                                 (a fork claim's directory holds only its
//!                                 csi.json: its fork is in its source's state)
//! <node-state>/                   --node-state: on the node, survives plugin restarts
//!   published/<handle>.json       the claim's published target
//! <profiles>/<profile>.config|.exclude|.attributes
//! ```

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
    /// `--node-state`: the published records.
    pub node_state: PathBuf,
    /// `--profiles-dir`: the operator's profiles.
    pub profiles: PathBuf,
}

impl Layout {
    /// `<base>/<handle>`.
    pub fn volume_dir(&self, handle: &str) -> PathBuf {
        self.base.join(handle)
    }

    /// The volume's tree state area, `<base>/<handle>/state`.
    pub fn state(&self, handle: &str) -> PathBuf {
        self.volume_dir(handle).join("state")
    }

    /// The claim record; the claim exists while it does.
    pub fn csi_json(&self, handle: &str) -> PathBuf {
        self.volume_dir(handle).join("csi.json")
    }

    /// The claim record once DeleteVolume kept the volume.
    pub fn retained_json(&self, handle: &str) -> PathBuf {
        self.volume_dir(handle).join("csi.retained.json")
    }

    /// The directory of the published records.
    pub fn published_dir(&self) -> PathBuf {
        self.node_state.join("published")
    }

    /// The claim's published target, `<node_state>/published/<handle>.json`.
    pub fn published(&self, handle: &str) -> PathBuf {
        self.published_dir().join(format!("{handle}.json"))
    }

    /// `<profiles>/<profile>.<ext>` (`config`, `exclude`, `attributes`).
    pub fn profile_file(&self, profile: &str, ext: &str) -> PathBuf {
        self.profiles.join(format!("{profile}.{ext}"))
    }
}

/// A volume handle is a directory name and an agentfs name component.
pub fn validate_handle(handle: &str) -> Result<(), LayoutError> {
    validate_name("volume id", handle).map_err(|_| LayoutError::BadHandle(handle.to_owned()))
}

/// A node id names a topology segment.
pub fn validate_node(node: &str) -> Result<(), LayoutError> {
    validate_name("node id", node).map_err(|_| LayoutError::BadNode(node.to_owned()))
}

/// The first 16 hex digits of the SHA-256 of `target`: a short, stable
/// name for a kubelet target path.
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
            profiles: "/p".into(),
        };
        assert_eq!(l.volume_dir("v"), Path::new("/b/v"));
        assert_eq!(l.state("v"), Path::new("/b/v/state"));
        assert_eq!(l.csi_json("v"), Path::new("/b/v/csi.json"));
        assert_eq!(l.retained_json("v"), Path::new("/b/v/csi.retained.json"));
        assert_eq!(l.published("v"), Path::new("/n/published/v.json"));
        assert_eq!(l.profile_file("ci", "config"), Path::new("/p/ci.config"));
    }
}
