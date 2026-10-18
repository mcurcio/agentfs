//! Worktrees on this node's disk: creating one (CreateVolume: an empty
//! one, one started from another worktree's branch, or a fork), adopting
//! one through a static PV, the profile's rules, and the access modes the
//! driver supports (design §14.2).
//!
//! A worktree is a volume while `<base>/<handle>/csi.json` exists. It is
//! written last, after the backing tree and the state area, so a
//! half-created volume is created again by the CO's retry. Its identity
//! (the volume name, its branch `agentfs/<volume>` and its start point)
//! is in the state area's own config and never changes (F8 rule 1).

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentfs_core::config::Remotes;
use agentfs_core::fork::create::{self as fork_create, CreateError, CreateRequest};
use agentfs_core::fork::start::{self, StartError, StartRequest};
use agentfs_core::lock::{LockError, StateLock};
use agentfs_core::volume::{self, StateDir};

use crate::csi::v1::VolumeCapability;
use crate::csi::v1::volume_capability::AccessType;
use crate::csi::v1::volume_capability::access_mode::Mode;
use crate::error::Error;
use crate::layout::Layout;
use crate::params::{ForkClass, VolumeParams};
use crate::records::{self, VolumeRecord};

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

/// The volume's record, if it exists on this node.
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

/// The volume's directory (0700) and backing tree (0755).
fn make_dirs(layout: &Layout, handle: &str) -> Result<(), Error> {
    fs::create_dir_all(&layout.base)
        .map_err(|e| Error::io(format!("creating {}", layout.base.display()), e))?;
    for (path, mode) in [
        (&layout.volume_dir(handle), 0o700),
        (&layout.backing(handle), 0o755),
    ] {
        DirBuilder::new()
            .recursive(true)
            .mode(mode)
            .create(path)
            .map_err(|e| Error::io(format!("creating {}", path.display()), e))?;
    }
    Ok(())
}

/// The record of a volume made now.
fn new_record(params: VolumeParams, capacity_bytes: i64, static_volume: bool) -> VolumeRecord {
    VolumeRecord {
        params,
        capacity_bytes,
        created: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        static_volume,
        finish_requested: false,
    }
}

/// Writes `record` last, and drops a retained record of the same name: a
/// worktree made again over a retained one is no longer only retained.
fn finish(layout: &Layout, handle: &str, record: &VolumeRecord) -> Result<(), Error> {
    store(layout, handle, record)?;
    records::remove(&layout.retained_json(handle))
        .map_err(|e| Error::io("removing the retained record", e))
}

/// Creates the worktree CreateVolume asks for (or finishes a half-created
/// one): its directory, the backing tree, the state area with the
/// profile's config included (an empty one, or started from the class's
/// `startFrom`, [`start::start_from`]), the profile's rules, then
/// `csi.json`. An existing state area must be this volume's; it may not
/// be in use. A profile that declares no remote is refused
/// (`INVALID_ARGUMENT`, F8 S3), and a worktree this call made for it is
/// removed again.
pub fn create(
    layout: &Layout,
    handle: &str,
    params: &VolumeParams,
    capacity_bytes: i64,
) -> Result<VolumeRecord, Error> {
    let config = profile_config(layout, &params.profile)?;
    let start = params
        .start_point()
        .map_err(|e| Error::InvalidArgument(e.to_string()))?;
    let new = !layout.volume_dir(handle).exists();
    make_dirs(layout, handle)?;
    let state = StateDir::new(layout.state(handle));
    let _lock = if state.git().join("config").exists() {
        Some(lock(&state, "agentfs-csi create")?)
    } else {
        None
    };
    match &start {
        None => volume::init_with(&state, &params.volume, None, Some(&[config]))
            .map(|_| ())
            .map_err(|e| match e {
                volume::VolumeError::OtherVolume { .. }
                | volume::VolumeError::OtherBranch { .. } => {
                    Error::AlreadyExists(format!("{}: {e}", state.root().display()))
                }
                e => Error::Internal(format!(
                    "initialising {}: {}",
                    state.root().display(),
                    chain(&e)
                )),
            })?,
        Some(start) => {
            start::start_from(&StartRequest {
                state: &state,
                tree: &layout.backing(handle),
                volume: &params.volume,
                includes: &[config],
                start,
            })
            .map_err(start_error)?;
        }
    }
    if let Err(e) = refuse_undeclared(&state, &params.profile) {
        drop(_lock);
        if new {
            let dir = layout.volume_dir(handle);
            fs::remove_dir_all(&dir)
                .map_err(|e| Error::io(format!("removing {}", dir.display()), e))?;
        }
        return Err(e);
    }
    install_rules(layout, handle, &params.profile)?;
    let record = new_record(params.clone(), capacity_bytes, false);
    finish(layout, handle, &record)?;
    Ok(record)
}

/// `INVALID_ARGUMENT` when the worktree at `state`, with `profile`'s
/// config, declares no remote ([`Remotes::Undeclared`], F8 S3). It does
/// not refuse `onDelete: delete` on a local-only profile: the release
/// decider keeps a local-only worktree whatever `onDelete` says.
fn refuse_undeclared(state: &StateDir, profile: &str) -> Result<(), Error> {
    let history = agentfs_core::history::History::open(&state.git()).map_err(|e| {
        Error::Internal(format!("opening {}: {}", state.root().display(), chain(&e)))
    })?;
    match &history.settings().remotes {
        Remotes::Undeclared { beside: None } => Err(Error::InvalidArgument(format!(
            "profile {profile:?} declares no remote: add a [remote] section to its config, or \
             agentfs.remotes = none for local-only volumes"
        ))),
        Remotes::Undeclared {
            beside: Some(remote),
        } => Err(Error::InvalidArgument(format!(
            "profile {profile:?} sets agentfs.remotes = none beside remote section {remote:?}: \
             remove one of them"
        ))),
        Remotes::Pushed(_) | Remotes::NoneValid | Remotes::LocalOnly => Ok(()),
    }
}

/// Adopts a static PV (NodeStage of a handle with no `csi.json`): only
/// over an existing worktree whose `agentfs.volume` is `params.volume`
/// (rule 1: NodeStage never creates). Writes `csi.json` with
/// `static_volume`; the state area is checked and its includes set by
/// the caller ([`volume::reopen_with`]).
pub fn adopt(
    layout: &Layout,
    node_id: &str,
    handle: &str,
    params: &VolumeParams,
) -> Result<VolumeRecord, Error> {
    let config = profile_config(layout, &params.profile)?;
    let state = StateDir::new(layout.state(handle));
    volume::reopen_with(&state, &params.volume, &[config])
        .map_err(|e| reopen_error(layout, node_id, e))?;
    let record = new_record(params.clone(), 0, true);
    finish(layout, handle, &record)?;
    Ok(record)
}

/// [`volume::reopen_with`]'s error as the CSI code the CO acts on: a
/// missing worktree is `NOT_FOUND` (the pod waits, naming it), another
/// volume's state area `ALREADY_EXISTS`.
pub fn reopen_error(layout: &Layout, node_id: &str, e: volume::VolumeError) -> Error {
    match e {
        volume::VolumeError::NoWorktree { path } => Error::NotFound(format!(
            "no worktree on node {node_id} under {}; the pod waits (nothing at {}: a worktree \
             lives on the node that created it, docs/csi.md)",
            layout.base.display(),
            path.display()
        )),
        e @ volume::VolumeError::OtherVolume { .. } => Error::AlreadyExists(chain(&e)),
        e => Error::FailedPrecondition(chain(&e)),
    }
}

/// Creates a fork of `source_handle` (CreateVolume with a fork class,
/// design §11.5): directories as [`create`], then
/// [`agentfs_core::fork::create::create`] with the source profile's config
/// as the fork's include, the source's profile rules, and `csi.json`
/// last. The source is read without its lock, so it may be in use; it
/// must be on this node (`FAILED_PRECONDITION` otherwise: a fork's pod
/// needs affinity to its source's pods).
pub fn create_fork(
    layout: &Layout,
    node_id: &str,
    handle: &str,
    class: &ForkClass,
    source_handle: &str,
    capacity_bytes: i64,
) -> Result<VolumeRecord, Error> {
    let Some(source) = load(layout, source_handle)? else {
        return Err(Error::FailedPrecondition(format!(
            "the source volume {source_handle} is not on node {node_id} (under {}): a fork is \
             made on its source's node; give the fork's pod affinity to its source's pods",
            layout.base.display()
        )));
    };
    let params = class.params(&source.params);
    let config = profile_config(layout, &params.profile)?;
    make_dirs(layout, handle)?;
    let state = StateDir::new(layout.state(handle));
    let lock = lock(&state, "agentfs-csi create")?;
    let source_state = StateDir::new(layout.state(source_handle));
    let push_target = fork_create::push_target(&source_state).map_err(create_error)?;
    let identity = fork_create::create(&CreateRequest {
        source: &source_state,
        fork: &state,
        tree: &layout.backing(handle),
        name: &params.volume,
        includes: &[config],
        push_target,
        link: class.link,
    })
    .map_err(create_error)?;
    drop(lock);
    install_rules(layout, handle, &params.profile)?;
    let record = new_record(params, capacity_bytes, false);
    finish(layout, handle, &record)?;
    eprintln!(
        "created fork {handle} of {source_handle} (agentfs volume {} of {} at {})",
        identity.fork, identity.source, identity.base
    );
    Ok(record)
}

/// A fork's [`CreateError`] as the CSI code the CO acts on.
pub fn create_error(e: CreateError) -> Error {
    let message = chain(&e);
    match e {
        CreateError::SourceNotFound { .. } => Error::NotFound(message),
        CreateError::SourceHasNoHistory { .. }
        | CreateError::SourceIsFork { .. }
        | CreateError::NoForkRemote { .. }
        | CreateError::SourceSettingsNotIncluded { .. }
        | CreateError::LfsObjectMissing { .. } => Error::FailedPrecondition(message),
        CreateError::BadName { .. } => Error::InvalidArgument(message),
        CreateError::Exists { .. } => Error::AlreadyExists(message),
        CreateError::SourceUnreadable(_) => Error::Unavailable(message),
        _ => Error::Internal(message),
    }
}

/// A start class's [`StartError`] as the CSI code the CO acts on (F8 S1).
pub fn start_error(e: StartError) -> Error {
    let message = chain(&e);
    match e {
        StartError::NoSuchRemote { .. } | StartError::OwnBranch { .. } | StartError::LocalOnly => {
            Error::InvalidArgument(message)
        }
        StartError::NotReachable { .. } | StartError::NoSuchBranch { .. } => {
            Error::FailedPrecondition(message)
        }
        StartError::Unreachable { .. } | StartError::LfsObjectMissing { .. } => {
            Error::Unavailable(message)
        }
        StartError::Exists { .. } => Error::AlreadyExists(message),
        _ => Error::Internal(message),
    }
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
    fn start_errors_map_to_their_codes() {
        let oid = git2::Oid::from_str(&"1".repeat(40)).unwrap();
        let cases = [
            (
                StartError::NoSuchRemote { remote: "x".into() },
                "InvalidArgument",
            ),
            (
                StartError::OwnBranch { branch: "b".into() },
                "InvalidArgument",
            ),
            (StartError::LocalOnly, "InvalidArgument"),
            (
                StartError::NotReachable {
                    commit: oid,
                    branch: "b".into(),
                },
                "FailedPrecondition",
            ),
            (
                StartError::NoSuchBranch {
                    remote: "r".into(),
                    branch: "b".into(),
                },
                "FailedPrecondition",
            ),
            (
                StartError::Unreachable {
                    remote: "r".into(),
                    message: "m".into(),
                },
                "Unavailable",
            ),
            (
                StartError::LfsObjectMissing {
                    path: "p".into(),
                    message: "m".into(),
                },
                "Unavailable",
            ),
            (StartError::Exists { found: "f".into() }, "AlreadyExists"),
        ];
        for (error, code) in cases {
            let mapped = format!("{:?}", start_error(error));
            assert!(mapped.starts_with(code), "{mapped}");
        }
    }

    #[test]
    fn a_static_pv_is_adopted_only_over_its_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout {
            base: dir.path().join("base"),
            node_state: dir.path().join("node"),
            mount_dir: dir.path().join("mnt"),
            profiles: dir.path().join("profiles"),
        };
        fs::create_dir_all(&layout.profiles).unwrap();
        fs::write(
            layout.profile_file("p", "config"),
            "[agentfs]\n\tremotes = none\n",
        )
        .unwrap();
        let params = VolumeParams::from_context(
            "h",
            &[("profile", "p"), ("volumeName", "v")]
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
        .unwrap();
        let Err(Error::NotFound(message)) = adopt(&layout, "node-1", "h", &params) else {
            panic!("no worktree: NOT_FOUND");
        };
        assert!(message.contains("no worktree on node node-1"), "{message}");
        assert!(!layout.csi_json("h").exists(), "nothing is created");
        volume::init(&StateDir::new(layout.state("h")), "other", None).unwrap();
        assert!(matches!(
            adopt(&layout, "node-1", "h", &params),
            Err(Error::AlreadyExists(_))
        ));
        fs::remove_dir_all(layout.volume_dir("h")).unwrap();
        volume::init(&StateDir::new(layout.state("h")), "v", None).unwrap();
        let record = adopt(&layout, "node-1", "h", &params).unwrap();
        assert!(record.static_volume);
        assert_eq!(load(&layout, "h").unwrap(), Some(record));
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
