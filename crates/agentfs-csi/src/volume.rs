//! Volumes on disk: creating one (CreateVolume, or a static PV adopted at
//! ControllerPublish or NodeStage), the profile's rules, and the access
//! modes the driver supports (design §14.2).
//!
//! A volume exists while `<base>/<handle>/csi.json` does. It is written
//! last, after the backing tree and the state area, so a half-created
//! volume is created again by the CO's retry.

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentfs_core::lock::{LockError, StateLock};
use agentfs_core::volume::{self, StateDir};

use crate::csi::v1::VolumeCapability;
use crate::csi::v1::volume_capability::AccessType;
use crate::csi::v1::volume_capability::access_mode::Mode;
use crate::error::Error;
use crate::layout::Layout;
use crate::params::VolumeParams;
use crate::records::{self, VolumeRecord};

/// What a refused multi-node access mode says without `--multi-node`.
pub const MULTI_NODE: &str = "multi-node access modes are not supported: this driver runs one \
                              node per volume unless started with --multi-node on a shared \
                              basePath (design §14.2)";

/// What MULTI_NODE_SINGLE_WRITER says: one writer across nodes is not a
/// guarantee the driver can give (each node's fronts are independent).
pub const SINGLE_WRITER_ACROSS_NODES: &str = "MULTI_NODE_SINGLE_WRITER is not supported: use \
                              ReadWriteMany (MULTI_NODE_MULTI_WRITER) or ReadOnlyMany";

/// The access modes the driver supports on one node.
pub const SUPPORTED_MODES: [Mode; 4] = [
    Mode::SingleNodeWriter,
    Mode::SingleNodeSingleWriter,
    Mode::SingleNodeMultiWriter,
    Mode::SingleNodeReaderOnly,
];

/// The multi-node access modes `--multi-node` adds.
pub const MULTI_NODE_MODES: [Mode; 2] = [Mode::MultiNodeMultiWriter, Mode::MultiNodeReaderOnly];

/// Whether `mode` lets several nodes use the volume at once.
pub fn is_multi_node(mode: Mode) -> bool {
    matches!(
        mode,
        Mode::MultiNodeMultiWriter | Mode::MultiNodeReaderOnly | Mode::MultiNodeSingleWriter
    )
}

/// Checks one volume capability: a mount (any fs_type, no mount flags)
/// with a single-node access mode, or with `multi_node` (the driver's
/// `--multi-node`) MULTI_NODE_MULTI_WRITER or MULTI_NODE_READER_ONLY.
/// Returns the mode.
pub fn check_capability(cap: &VolumeCapability, multi_node: bool) -> Result<Mode, Error> {
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
        Ok(m) if multi_node && MULTI_NODE_MODES.contains(&m) => Ok(m),
        Ok(Mode::MultiNodeSingleWriter) if multi_node => {
            Err(Error::InvalidArgument(SINGLE_WRITER_ACROSS_NODES.into()))
        }
        Ok(
            Mode::MultiNodeReaderOnly | Mode::MultiNodeSingleWriter | Mode::MultiNodeMultiWriter,
        ) => Err(Error::InvalidArgument(MULTI_NODE.into())),
        _ => Err(Error::InvalidArgument(format!(
            "access mode {mode} is not supported"
        ))),
    }
}

/// Checks a non-empty list of capabilities; whether any is multi-node.
pub fn check_capabilities(caps: &[VolumeCapability], multi_node: bool) -> Result<bool, Error> {
    if caps.is_empty() {
        return Err(Error::InvalidArgument(
            "volume capabilities are required".into(),
        ));
    }
    let mut any = false;
    for cap in caps {
        any |= is_multi_node(check_capability(cap, multi_node)?);
    }
    Ok(any)
}

/// The volume's record, if it exists.
pub fn load(layout: &Layout, handle: &str) -> Result<Option<VolumeRecord>, Error> {
    let path = layout.csi_json(handle);
    records::load(&path).map_err(|e| Error::io(format!("reading {}", path.display()), e))
}

/// Writes the volume's record.
pub fn store(layout: &Layout, handle: &str, record: &VolumeRecord) -> Result<(), Error> {
    let path = layout.csi_json(handle);
    records::store(&path, record).map_err(|e| Error::io(format!("writing {}", path.display()), e))
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

/// Creates the volume (or finishes a half-created one): its directory
/// (0700), the backing tree, the state area with the profile's config
/// included, the profile's rules, then `csi.json`. An existing state area
/// must name the same volume and branch; it may not be in use.
pub fn create(
    layout: &Layout,
    handle: &str,
    params: &VolumeParams,
    capacity_bytes: i64,
    static_volume: bool,
) -> Result<VolumeRecord, Error> {
    create_with(layout, handle, params, capacity_bytes, static_volume, false)
}

/// [`create`], recording whether the volume was created for several nodes
/// (`multi_node`).
pub fn create_with(
    layout: &Layout,
    handle: &str,
    params: &VolumeParams,
    capacity_bytes: i64,
    static_volume: bool,
    multi_node: bool,
) -> Result<VolumeRecord, Error> {
    let config = profile_config(layout, &params.profile)?;
    let dir = layout.volume_dir(handle);
    fs::create_dir_all(&layout.base)
        .map_err(|e| Error::io(format!("creating {}", layout.base.display()), e))?;
    for (path, mode) in [(&dir, 0o700), (&layout.backing(handle), 0o755)] {
        DirBuilder::new()
            .recursive(true)
            .mode(mode)
            .create(path)
            .map_err(|e| Error::io(format!("creating {}", path.display()), e))?;
    }
    let state = StateDir::new(layout.state(handle));
    let _lock = if state.git().join("config").exists() {
        Some(lock(&state, "agentfs-csi create")?)
    } else {
        None
    };
    volume::init_with(
        &state,
        &params.volume,
        params.branch.as_deref(),
        Some(&[config]),
    )
    .map_err(|e| match e {
        volume::VolumeError::OtherVolume { .. } | volume::VolumeError::OtherBranch { .. } => {
            Error::AlreadyExists(format!("{}: {e}", state.root().display()))
        }
        e => Error::Internal(format!(
            "initialising {}: {}",
            state.root().display(),
            chain(&e)
        )),
    })?;
    install_rules(layout, handle, &params.profile)?;
    let record = VolumeRecord {
        params: params.clone(),
        capacity_bytes,
        created: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        published_node: None,
        static_volume,
        multi_node,
        published_nodes: std::collections::BTreeSet::new(),
    };
    store(layout, handle, &record)?;
    // Re-created over a retained volume: it is no longer only retained.
    records::remove(&layout.retained_json(handle))
        .map_err(|e| Error::io("removing the retained record", e))?;
    Ok(record)
}

/// Copies the profile's `.exclude` and `.attributes` (empty when absent)
/// into the state area's `info/`, where the history service reads them
/// at start.
pub fn install_rules(layout: &Layout, handle: &str, profile: &str) -> Result<(), Error> {
    let info = StateDir::new(layout.state(handle)).git().join("info");
    for kind in ["exclude", "attributes"] {
        let from = layout.profile_file(profile, kind);
        let text = match fs::read(&from) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(Error::io(format!("reading {}", from.display()), e)),
        };
        let to = info.join(kind);
        if fs::read(&to).ok().as_deref() == Some(text.as_slice()) {
            continue;
        }
        let tmp = info.join(format!(".{kind}.agentfs-csi"));
        fs::write(&tmp, &text)
            .and_then(|()| fs::rename(&tmp, &to))
            .map_err(|e| Error::io(format!("writing {}", to.display()), e))?;
    }
    Ok(())
}

/// Takes the state-area lock without waiting; a holder is
/// `FAILED_PRECONDITION` naming it.
pub fn lock(state: &StateDir, command: &str) -> Result<StateLock, Error> {
    StateLock::acquire(state, command, Duration::ZERO).map_err(|e| match e {
        LockError::Busy { .. } => Error::FailedPrecondition(e.to_string()),
        e => Error::Internal(chain(&e)),
    })
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
        for multi in [false, true] {
            for mode in SUPPORTED_MODES {
                assert_eq!(check_capability(&cap(mode, mount()), multi).unwrap(), mode);
            }
        }
        // Without --multi-node: the error says how to get it.
        for mode in [
            Mode::MultiNodeMultiWriter,
            Mode::MultiNodeReaderOnly,
            Mode::MultiNodeSingleWriter,
        ] {
            let err = check_capability(&cap(mode, mount()), false).unwrap_err();
            assert!(err.to_string().contains("multi-node"), "{err}");
            assert!(err.to_string().contains("--multi-node"), "{err}");
        }
        // With it: RWX and ROX, never a single writer across nodes.
        for mode in MULTI_NODE_MODES {
            assert_eq!(check_capability(&cap(mode, mount()), true).unwrap(), mode);
            assert!(is_multi_node(mode));
        }
        let err = check_capability(&cap(Mode::MultiNodeSingleWriter, mount()), true).unwrap_err();
        assert!(
            err.to_string().contains("MULTI_NODE_SINGLE_WRITER"),
            "{err}"
        );
        assert!(check_capability(&cap(Mode::Unknown, mount()), true).is_err());
        let err = check_capability(&cap(Mode::SingleNodeWriter, None), false).unwrap_err();
        assert!(err.to_string().contains("block"), "{err}");
        let flags = Some(MountVolume {
            mount_flags: vec!["ro".into()],
            ..MountVolume::default()
        });
        assert!(check_capability(&cap(Mode::SingleNodeWriter, flags), false).is_err());
        let ext4 = Some(MountVolume {
            fs_type: "ext4".into(),
            ..MountVolume::default()
        });
        check_capability(&cap(Mode::SingleNodeWriter, ext4), false).unwrap();
        assert!(check_capabilities(&[], true).is_err());
        assert!(!check_capabilities(&[cap(Mode::SingleNodeWriter, mount())], true).unwrap());
        assert!(
            check_capabilities(
                &[
                    cap(Mode::SingleNodeWriter, mount()),
                    cap(Mode::MultiNodeMultiWriter, mount())
                ],
                true
            )
            .unwrap()
        );
        let none = VolumeCapability::default();
        assert!(check_capability(&none, false).is_err());
    }
}
