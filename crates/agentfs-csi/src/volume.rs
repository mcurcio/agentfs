//! The access modes the driver supports, and a profile's config file
//! (docs/csi.md).

use std::fs;
use std::io;
use std::path::PathBuf;

use crate::csi::v1::VolumeCapability;
use crate::csi::v1::volume_capability::AccessType;
use crate::csi::v1::volume_capability::access_mode::Mode;
use crate::error::Error;
use crate::layout::Layout;

/// What a refused multi-node access mode says: the decision, with where
/// it is recorded. `ci/e2e-csi.sh` waits for "multi-node" in it.
pub const MULTI_NODE: &str = "multi-node access modes are not supported: an agentfs volume is \
                              used from one node at a time; pods on other nodes read its \
                              history from the remote it pushes to (2026-10-15)";

/// The access modes the driver supports: one node at a time.
pub const SUPPORTED_MODES: [Mode; 4] = [
    Mode::SingleNodeWriter,
    Mode::SingleNodeSingleWriter,
    Mode::SingleNodeMultiWriter,
    Mode::SingleNodeReaderOnly,
];

/// Checks one volume capability: a mount (any fs_type, no mount flags)
/// with a single-node access mode. Returns the mode.
pub fn check_capability(cap: &VolumeCapability) -> Result<Mode, Error> {
    match &cap.access_type {
        None => {
            return Err(Error::InvalidArgument(
                "the volume capability has no access type (mount is required)".into(),
            ));
        }
        Some(AccessType::Block(_)) => {
            return Err(Error::InvalidArgument(
                "block volumes are not supported: an agentfs volume is a directory tree".into(),
            ));
        }
        Some(AccessType::Mount(mount)) if !mount.mount_flags.is_empty() => {
            return Err(Error::InvalidArgument(format!(
                "mount flags {:?} are not supported (agentfs mounts are FUSE mounts the driver \
                 configures)",
                mount.mount_flags
            )));
        }
        Some(AccessType::Mount(_)) => {}
    }
    let mode = cap
        .access_mode
        .as_ref()
        .ok_or_else(|| Error::InvalidArgument("the volume capability has no access mode".into()))?
        .mode;
    match Mode::try_from(mode) {
        Ok(m) if SUPPORTED_MODES.contains(&m) => Ok(m),
        Ok(
            Mode::MultiNodeReaderOnly | Mode::MultiNodeSingleWriter | Mode::MultiNodeMultiWriter,
        ) => Err(Error::InvalidArgument(MULTI_NODE.into())),
        _ => Err(Error::InvalidArgument(format!(
            "access mode {mode} is not supported"
        ))),
    }
}

/// Checks a non-empty list of capabilities.
pub fn check_capabilities(caps: &[VolumeCapability]) -> Result<(), Error> {
    if caps.is_empty() {
        return Err(Error::InvalidArgument(
            "volume capabilities are required".into(),
        ));
    }
    caps.iter()
        .try_for_each(|c| check_capability(c).map(|_| ()))
}

/// The profile's config fragment, which must exist.
pub fn profile_config(layout: &Layout, profile: &str) -> Result<PathBuf, Error> {
    let path = layout.profile_file(profile, "config");
    match fs::metadata(&path) {
        Ok(_) => Ok(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(Error::InvalidArgument(format!(
            "profile {profile:?} does not exist: no {} (add `{profile}.config` to the \
             agentfs-csi-profiles ConfigMap)",
            path.display()
        ))),
        Err(e) => Err(Error::io(format!("reading {}", path.display()), e)),
    }
}

/// An error with its sources, `a: b: c`.
pub fn chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        text.push_str(": ");
        text.push_str(&s.to_string());
        source = s.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csi::v1::volume_capability::{AccessMode, BlockVolume, MountVolume};

    fn cap(mode: Mode, mount: Option<MountVolume>) -> VolumeCapability {
        VolumeCapability {
            access_mode: Some(AccessMode { mode: mode as i32 }),
            access_type: Some(match mount {
                Some(m) => AccessType::Mount(m),
                None => AccessType::Block(BlockVolume {}),
            }),
        }
    }

    #[test]
    fn capabilities() {
        let mount = || Some(MountVolume::default());
        for mode in SUPPORTED_MODES {
            assert_eq!(check_capability(&cap(mode, mount())).unwrap(), mode);
        }
        for mode in [
            Mode::MultiNodeReaderOnly,
            Mode::MultiNodeSingleWriter,
            Mode::MultiNodeMultiWriter,
        ] {
            let err = check_capability(&cap(mode, mount())).unwrap_err();
            assert_eq!(err, Error::InvalidArgument(MULTI_NODE.into()), "{mode:?}");
            assert_eq!(err.to_string(), MULTI_NODE);
        }
        assert!(
            MULTI_NODE.contains("multi-node"),
            "ci/e2e-csi.sh waits for it"
        );
        assert!(check_capability(&cap(Mode::Unknown, mount())).is_err());
        let err = check_capability(&cap(Mode::SingleNodeWriter, None)).unwrap_err();
        assert!(err.to_string().contains("block"), "{err}");
        let flags = Some(MountVolume {
            mount_flags: vec!["ro".into()],
            ..MountVolume::default()
        });
        assert!(check_capability(&cap(Mode::SingleNodeWriter, flags)).is_err());
        let ext4 = Some(MountVolume {
            fs_type: "ext4".into(),
            ..MountVolume::default()
        });
        check_capability(&cap(Mode::SingleNodeWriter, ext4)).unwrap();
        assert!(check_capabilities(&[]).is_err());
        let none = VolumeCapability::default();
        assert!(check_capability(&none).is_err());
    }

    #[test]
    fn the_multi_node_decision_text_is_frozen() {
        assert_eq!(
            MULTI_NODE,
            "multi-node access modes are not supported: an agentfs volume is used from one node \
             at a time; pods on other nodes read its history from the remote it pushes to \
             (2026-10-15)"
        );
    }
}
