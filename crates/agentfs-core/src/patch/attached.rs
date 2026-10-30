//! Whether a workload may be attached when a daemon starts, so the patch
//! queue waits (design §12 "When it runs", §19.1 "No live landing yet"
//! and "Patches land after a clean stop").
//!
//! Patches land only at a start with nothing attached. A start lands them
//! only on evidence that nothing is: anything else defers the queue with
//! a [`Deferred`] reason, and the patches stay queued for the next start.
//!
//! - **A single-process mount** judges by its own stops. It writes the
//!   attached mark ([`mark`], `meta/attached.json`) before the mount can
//!   be seen and removes it ([`clear`]) only after an unmount that was not
//!   busy, with the session ended and no stale layer below. The mark is
//!   left by a crash, a kill, a lazy detach or a session that outlived the
//!   unmount (files still open, or the filesystem still mounted elsewhere,
//!   such as a kubelet `subPath` bind), so a start that finds it defers
//!   ([`Deferred::UncleanStop`]) unless the mark is another pod's
//!   ([`judge`], [`PodScope`]). A state area that was never mounted has
//!   no mark.
//! - **A history service** defers while fronts hold leases, or when its
//!   launcher says a workload is attached (the CSI node plugin, while a
//!   pod of the volume is published on its node).
//!
//! **Another pod's mark** (design §19.1 "Patches land after a clean
//! stop", 2026-10-30). A mount whose mount point lies in its own pod's
//! volume ([`PodScope`]) records that pod's uid in the mark. A start in a
//! different pod, whose mount point lies in that pod's own volume too,
//! lands the queue over such a mark: nothing attached to the old pod's
//! mount can reach a mount point in the new pod's volume, and the queue
//! lands before the new mount can be seen. A mark from the same pod (a
//! restart of the daemon alone), a mark without a uid, or a start that is
//! not pod-scoped defers as before.

use std::io;
use std::path::{Component, Path};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::fsutil::write_json_atomic;
use crate::mountinfo::{self, MountEntry};
use crate::volume::StateDir;

/// Why a start does not land the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deferred {
    /// The previous mount did not stop with nothing attached: it was
    /// killed or crashed, its unmount was busy (a lazy detach), its
    /// session outlived the unmount (files still open, or the filesystem
    /// still mounted elsewhere), or stale layers were left under it; and
    /// it was not another pod's ([`judge`]).
    UncleanStop,
    /// This mount stacks over a stale mount a crashed daemon left, which
    /// workloads may still hold.
    StaleMount,
    /// Fronts hold leases on the history service's volume.
    FrontsRunning,
    /// The launcher said a workload is attached (`serve-history
    /// --workload-attached`).
    WorkloadAttached,
}

impl Deferred {
    /// The reason's label, in the `PatchQueueDeferred` event.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UncleanStop => "unclean_stop",
            Self::StaleMount => "stale_mount",
            Self::FrontsRunning => "fronts_running",
            Self::WorkloadAttached => "workload_attached",
        }
    }

    /// Why, in words.
    pub fn describe(self) -> &'static str {
        match self {
            Self::UncleanStop => {
                "the previous mount of this volume did not stop with nothing attached (killed, \
                 crashed, busy, or still mounted elsewhere) and was not another pod's, so a \
                 workload may still hold it"
            }
            Self::StaleMount => "this mount stacks over a stale mount a workload may still hold",
            Self::FrontsRunning => "fronts are running",
            Self::WorkloadAttached => "the launcher says a workload is attached",
        }
    }
}

/// The attached mark's content: who wrote it, for the operator, and the
/// pod whose own volume held the mount point, for [`judge`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachedMark {
    pub pid: u32,
    /// When the mount started, in milliseconds since the epoch.
    pub since_ms: u64,
    /// The Kubernetes pod uid of a pod-scoped mount ([`PodScope`]); absent
    /// otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_uid: Option<String>,
}

/// A mount point that lies in its own pod's volume: the uid the launcher
/// gave (`AGENTFS_POD_UID`, the downward API's `metadata.uid`), trusted
/// only because the mount point's filesystem directory is under the
/// kubelet's directory for that pod (`.../pods/<uid>/volumes/...`, an
/// `emptyDir` for example). No other pod's containers mount that
/// directory, so nothing attached to another pod's mount can reach a
/// mount made there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodScope {
    uid: String,
}

impl PodScope {
    /// The scope of `mountpoint` (absolute, canonical) for the pod `uid`,
    /// from this process's mountinfo; `None` when `uid` is empty, the
    /// mountinfo cannot be read, or the mount point is not in that pod's
    /// own volume.
    pub fn of(uid: &str, mountpoint: &Path) -> Option<Self> {
        Self::from_entries(&mountinfo::read().ok()?, uid, mountpoint)
    }

    /// [`PodScope::of`] over given mountinfo entries. The mount holding
    /// `mountpoint` is the deepest, topmost one that is not an agentfs
    /// mount, so a stale agentfs mount on the mount point (a restart that
    /// stacks over it) is looked through to the volume below. Its root
    /// (the directory of its filesystem it shows) must hold the
    /// components `pods/<uid>/volumes`, wherever the kubelet's root
    /// directory is (an empty uid, or one with a `/`, never matches).
    pub fn from_entries(entries: &[MountEntry], uid: &str, mountpoint: &Path) -> Option<Self> {
        let underlying: Vec<MountEntry> = entries
            .iter()
            .filter(|e| !e.is_agentfs())
            .cloned()
            .collect();
        let entry = mountinfo::containing(&underlying, mountpoint)?;
        let names: Vec<&std::ffi::OsStr> = entry
            .root
            .components()
            .filter_map(|c| match c {
                Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect();
        names
            .windows(3)
            .any(|w| w[0] == "pods" && w[1] == uid && w[2] == "volumes")
            .then(|| Self {
                uid: uid.to_owned(),
            })
    }

    /// The pod uid.
    pub fn uid(&self) -> &str {
        &self.uid
    }
}

/// What a start may do with the patch queue, from the attached mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Judgement {
    /// No mark: the last mount stopped with nothing attached, or there
    /// was none.
    Clean,
    /// The mark is another pod's (`uid`), and this start is pod-scoped:
    /// nothing attached to that pod's mount can reach this one.
    AnotherPod { uid: String },
    /// A workload may be attached: the queue waits.
    Deferred(Deferred),
}

impl Judgement {
    /// Why the queue waits, if it does.
    pub fn deferred(&self) -> Option<Deferred> {
        match self {
            Self::Deferred(reason) => Some(*reason),
            Self::Clean | Self::AnotherPod { .. } => None,
        }
    }
}

/// Judges a single-process mount's start from the attached mark
/// (design §12 "Only with nothing attached"): no mark lands; a mark from
/// another pod lands when `scope` is this start's pod scope; anything
/// else, including a mark that cannot be read, defers
/// ([`Deferred::UncleanStop`]).
pub fn judge(state: &StateDir, scope: Option<&PodScope>) -> Judgement {
    let bytes = match std::fs::read(state.attached_mark()) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Judgement::Clean,
        Err(_) => return Judgement::Deferred(Deferred::UncleanStop),
        Ok(bytes) => bytes,
    };
    let previous = serde_json::from_slice::<AttachedMark>(&bytes)
        .ok()
        .and_then(|mark| mark.pod_uid);
    match (previous, scope) {
        (Some(previous), Some(scope)) if !previous.is_empty() && previous != scope.uid => {
            Judgement::AnotherPod { uid: previous }
        }
        _ => Judgement::Deferred(Deferred::UncleanStop),
    }
}

/// Writes the attached mark, durably, before the mount can be seen, with
/// the mount's pod scope if it has one.
pub fn mark(state: &StateDir, scope: Option<&PodScope>) -> io::Result<()> {
    std::fs::create_dir_all(state.meta())?;
    let since_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    write_json_atomic(
        &state.attached_mark(),
        &AttachedMark {
            pid: std::process::id(),
            since_ms,
            pod_uid: scope.map(|s| s.uid.clone()),
        },
    )
}

/// Removes the attached mark, durably: the mount stopped with nothing
/// attached.
pub fn clear(state: &StateDir) -> io::Result<()> {
    match std::fs::remove_file(state.attached_mark()) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }
    std::fs::File::open(state.meta())?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    const UID: &str = "0b6f2c1e-7d1a-4c55-9f3e-2a1d6c0e8b11";
    const OTHER: &str = "5e1a9d04-3b2c-4f7e-8a6d-c9b0e1f2a3d4";

    /// A sidecar's mountinfo on a kind node, whose `/var` is a docker
    /// volume: the emptyDir at /agentfs, the PVC at /data, an agentfs
    /// mount on the mount point (live or stale), a tmpfs emptyDir and a
    /// hostPath.
    fn sidecar(uid: &str) -> Vec<MountEntry> {
        mountinfo::parse(&format!(
            "\
900 800 0:90 / / rw - overlay overlay rw
901 900 8:1 /var/lib/docker/volumes/4e1f/_data/lib/kubelet/pods/{uid}/volumes/kubernetes.io~empty-dir/agentfs-mount /agentfs rw shared:5 - ext4 /dev/root rw
902 900 8:1 /var/lib/docker/volumes/4e1f/_data/local-path-provisioner/pvc-1 /data rw - ext4 /dev/root rw
903 901 0:95 / /agentfs/workspace rw shared:6 - fuse.agentfs agentfs:hermes rw
904 900 0:96 / /scratch rw - tmpfs tmpfs rw
905 900 8:1 /srv/shared /shared rw - ext4 /dev/root rw
906 900 8:1 /var/lib/kubelet/pods/{uid}/volumes/kubernetes.io~empty-dir/x /plain rw - ext4 /dev/root rw
"
        ))
    }

    fn scope(entries: &[MountEntry], uid: &str, at: &str) -> Option<PodScope> {
        PodScope::from_entries(entries, uid, Path::new(at))
    }

    #[test]
    fn a_mount_point_in_its_own_pods_volume_is_pod_scoped() {
        let entries = sidecar(UID);
        // Through the agentfs mount on it (a stale one a restart stacks
        // over) to the emptyDir.
        assert_eq!(
            scope(&entries, UID, "/agentfs/workspace").map(|s| s.uid().to_owned()),
            Some(UID.to_owned())
        );
        // The kubelet's default root directory too.
        assert!(scope(&entries, UID, "/plain/workspace").is_some());
        // Another pod's uid, or none.
        assert_eq!(scope(&entries, OTHER, "/agentfs/workspace"), None);
        assert_eq!(scope(&entries, "", "/agentfs/workspace"), None);
        assert_eq!(scope(&entries, "..", "/agentfs/workspace"), None);
        assert_eq!(scope(&entries, "a/b", "/agentfs/workspace"), None);
        // Not the kubelet's pod directory: a PVC, a tmpfs (an emptyDir of
        // medium Memory shows its own root), a hostPath, the image.
        assert_eq!(scope(&entries, UID, "/data/backing"), None);
        assert_eq!(scope(&entries, UID, "/scratch/workspace"), None);
        assert_eq!(scope(&entries, UID, "/shared/workspace"), None);
        assert_eq!(scope(&entries, UID, "/workspace"), None);
        // A name that only contains the uid is not that component.
        let near = mountinfo::parse(&format!(
            "1 0 8:1 /pods/{UID}x/volumes/e /m rw - ext4 /dev/root rw\n\
             2 0 8:1 /pods/volumes/{UID} /n rw - ext4 /dev/root rw\n"
        ));
        assert_eq!(scope(&near, UID, "/m/w"), None);
        assert_eq!(scope(&near, UID, "/n/w"), None);
        assert_eq!(scope(&[], UID, "/agentfs/workspace"), None);
    }

    fn pod(uid: &str) -> PodScope {
        PodScope {
            uid: uid.to_owned(),
        }
    }

    #[test]
    fn a_new_state_area_is_clean_and_a_mark_lasts_until_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path());
        assert_eq!(judge(&state, None), Judgement::Clean, "never mounted");
        mark(&state, None).unwrap();
        let back: AttachedMark =
            serde_json::from_slice(&std::fs::read(state.attached_mark()).unwrap()).unwrap();
        assert_eq!(back.pid, std::process::id());
        assert_eq!(back.pod_uid, None);
        assert_eq!(judge(&state, None).deferred(), Some(Deferred::UncleanStop));
        clear(&state).unwrap();
        assert_eq!(judge(&state, Some(&pod(UID))), Judgement::Clean);
        clear(&state).unwrap();
    }

    #[test]
    fn another_pods_mark_lands_and_the_same_pods_defers() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path());
        mark(&state, Some(&pod(OTHER))).unwrap();
        let back: AttachedMark =
            serde_json::from_slice(&std::fs::read(state.attached_mark()).unwrap()).unwrap();
        assert_eq!(back.pod_uid.as_deref(), Some(OTHER));
        // A new pod: the old pod's processes cannot reach its mount point.
        let new = judge(&state, Some(&pod(UID)));
        assert_eq!(
            new,
            Judgement::AnotherPod {
                uid: OTHER.to_owned()
            }
        );
        assert_eq!(new.deferred(), None);
        // The same pod: a restart of the daemon alone.
        assert_eq!(
            judge(&state, Some(&pod(OTHER))).deferred(),
            Some(Deferred::UncleanStop)
        );
        // A start that is not pod-scoped (Docker, the CLI, a hostPath).
        assert_eq!(judge(&state, None).deferred(), Some(Deferred::UncleanStop));
    }

    #[test]
    fn a_mark_without_a_pod_defers_any_start() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path());
        mark(&state, None).unwrap();
        assert_eq!(
            judge(&state, Some(&pod(UID))).deferred(),
            Some(Deferred::UncleanStop)
        );
        // An empty uid written by hand, a mark that does not parse, and a
        // mark that is not a file.
        for content in [r#"{"pid":1,"since_ms":2,"pod_uid":""}"#, "not json", ""] {
            std::fs::write(state.attached_mark(), content).unwrap();
            assert_eq!(
                judge(&state, Some(&pod(UID))).deferred(),
                Some(Deferred::UncleanStop),
                "{content:?}"
            );
        }
        std::fs::remove_file(state.attached_mark()).unwrap();
        std::fs::create_dir(state.attached_mark()).unwrap();
        assert_eq!(
            judge(&state, Some(&pod(UID))).deferred(),
            Some(Deferred::UncleanStop)
        );
    }

    #[test]
    fn a_mark_that_cannot_be_checked_defers() {
        use std::os::unix::fs::PermissionsExt;
        if rustix::process::geteuid().is_root() {
            return; // root reads through the mode
        }
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path());
        mark(&state, Some(&pod(OTHER))).unwrap();
        std::fs::set_permissions(state.meta(), std::fs::Permissions::from_mode(0o000)).unwrap();
        let seen = judge(&state, Some(&pod(UID)));
        std::fs::set_permissions(state.meta(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(seen.deferred(), Some(Deferred::UncleanStop));
    }
}
