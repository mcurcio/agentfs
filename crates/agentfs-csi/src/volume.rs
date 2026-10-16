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
use crate::records::{self, MultiNodeFields, VolumeRecord};

/// What a refused multi-node access mode says: the decision, with where
/// it is recorded. `ci/e2e-csi.sh` waits for "multi-node" in it.
pub const MULTI_NODE: &str = "multi-node access modes are not supported: an agentfs volume is \
                              used from one node at a time; pods on other nodes read its \
                              history from the remote it pushes to (design §19.1, 2026-10-15)";

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

/// The volume's record, if it exists, for a call that would use it: a
/// record written by the multi-node build is `FAILED_PRECONDITION`.
pub fn load(layout: &Layout, handle: &str) -> Result<Option<VolumeRecord>, Error> {
    match load_any(layout, handle)? {
        Some((_, Some(_))) => Err(Error::FailedPrecondition(format!(
            "volume {handle} was created by the multi-node build: {MULTI_NODE}. Detach it from \
             every node, or delete it (docs/csi.md §7)"
        ))),
        found => Ok(found.map(|(record, _)| record)),
    }
}

/// The volume's record with the multi-node build's fields, if any: for
/// ControllerUnpublish and DeleteVolume, which still work on such a volume.
pub fn load_any(
    layout: &Layout,
    handle: &str,
) -> Result<Option<(VolumeRecord, Option<MultiNodeFields>)>, Error> {
    let path = layout.csi_json(handle);
    records::load_volume(&path).map_err(|e| Error::io(format!("reading {}", path.display()), e))
}

/// Writes the volume's record.
pub fn store(layout: &Layout, handle: &str, record: &VolumeRecord) -> Result<(), Error> {
    store_any(layout, handle, record, None)
}

/// Writes the volume's record, keeping the multi-node build's fields while
/// any node is still in them.
pub fn store_any(
    layout: &Layout,
    handle: &str,
    record: &VolumeRecord,
    legacy: Option<&MultiNodeFields>,
) -> Result<(), Error> {
    let path = layout.csi_json(handle);
    records::store_volume(&path, record, legacy)
        .map_err(|e| Error::io(format!("writing {}", path.display()), e))
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

/// Takes the state-area lock without waiting; a holder, or a filesystem
/// whose locks would not keep another node out, is `FAILED_PRECONDITION`
/// naming it.
pub fn lock(state: &StateDir, command: &str) -> Result<StateLock, Error> {
    StateLock::acquire(state, command, Duration::ZERO).map_err(lock_error)
}

/// A holder, or a filesystem whose locks would not keep another node out,
/// is `FAILED_PRECONDITION`; anything else is `INTERNAL`.
fn lock_error(e: LockError) -> Error {
    match e {
        LockError::Busy { .. } | LockError::UnsafeFilesystem { .. } => {
            Error::FailedPrecondition(chain(&e))
        }
        e => Error::Internal(chain(&e)),
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
             (design §19.1, 2026-10-15)"
        );
    }

    #[test]
    fn a_multi_node_record_is_refused_for_use_and_other_unknown_fields_fail() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout {
            base: dir.path().to_path_buf(),
            node_state: dir.path().join("node"),
            mount_dir: dir.path().join("mnt"),
            profiles: dir.path().join("profiles"),
        };
        let record = VolumeRecord {
            params: VolumeParams {
                profile: "p".into(),
                volume: "v".into(),
                branch: None,
                on_delete: crate::params::OnDelete::Delete,
                workload_mounts: Vec::new(),
            },
            capacity_bytes: 0,
            created: 1,
            published_node: None,
            static_volume: false,
        };
        let mut json = serde_json::to_value(&record).unwrap();
        json["multi_node"] = true.into();
        json["published_nodes"] = serde_json::json!(["n1", "n2"]);
        fs::create_dir_all(layout.volume_dir("v")).unwrap();
        fs::write(layout.csi_json("v"), json.to_string()).unwrap();
        // ControllerPublish and NodeStage read the volume through load().
        let Err(Error::FailedPrecondition(message)) = load(&layout, "v") else {
            panic!("a multi-node record must be FAILED_PRECONDITION");
        };
        assert!(message.contains(MULTI_NODE), "{message}");
        let (read, legacy) = load_any(&layout, "v").unwrap().unwrap();
        assert_eq!(read, record);
        assert_eq!(
            legacy
                .unwrap()
                .published_nodes
                .into_iter()
                .collect::<Vec<_>>(),
            ["n1", "n2"]
        );
        json["shards"] = 2.into();
        fs::write(layout.csi_json("v"), json.to_string()).unwrap();
        for err in [
            load(&layout, "v").unwrap_err(),
            load_any(&layout, "v").unwrap_err(),
        ] {
            let Error::Internal(message) = err else {
                panic!("{err:?}");
            };
            assert!(message.contains("unknown field `shards`"), "{message}");
        }
    }

    #[test]
    fn lock_errors_map_to_their_codes() {
        let state = PathBuf::from("/s");
        let unsafe_fs = LockError::UnsafeFilesystem {
            state: state.clone(),
            source: agentfs_core::shared_fs::CheckError::Io {
                path: state.clone(),
                source: io::Error::other("nfs3"),
            },
        };
        assert!(
            matches!(lock_error(unsafe_fs), Error::FailedPrecondition(m) if m.contains("cannot be locked safely"))
        );
        let busy = LockError::Busy {
            state: state.clone(),
            holder: "pid 1".into(),
        };
        assert!(matches!(lock_error(busy), Error::FailedPrecondition(m) if m.contains("pid 1")));
        let other = LockError::Io {
            path: state,
            source: io::Error::other("disk"),
        };
        assert!(matches!(lock_error(other), Error::Internal(m) if m.contains("disk")));
    }
}
