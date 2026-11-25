//! The driver's persisted records (docs/csi.md): the claim record
//! `csi.json` in each volume directory, and the node's published records,
//! from which a restarted node plugin reconciles. Each is JSON written
//! whole (a temporary file renamed over it, mode 0600) with
//! [`agentfs_os::fsutil::write_json_atomic`]. A field this driver does not
//! know fails the load, so a record of another build (the recording
//! driver's `csi.json` included) is refused, never half-read.

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use agentfs_contracts::host::TreeRef;
use agentfs_contracts::name::{ClientLabel, VolumeName};
use agentfs_contracts::pod::PodContext;

use crate::error::Error;
use crate::layout::Layout;

/// The records' version, in each record's `v`.
pub const RECORD_VERSION: u32 = 1;

/// What a claim is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClaimKind {
    /// A tree volume of its own in `<base>/<handle>/state`.
    Volume { name: VolumeName, profile: String },
    /// A fork in `source`'s volume under the key `<handle>` (the fork-claim
    /// slice builds it).
    Fork { source: String },
}

/// `<base>/<handle>/csi.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimRecord {
    pub v: u32,
    /// As requested; recorded, not enforced.
    pub capacity_bytes: i64,
    pub claim: ClaimKind,
}

/// `<node_state>/published/<handle>.json`: the claim's view is open at
/// `target`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedRecord {
    pub v: u32,
    pub handle: String,
    /// The volume directory holding the state: the host serving the view.
    pub volume: String,
    pub tree: TreeRef,
    pub target: PathBuf,
    pub read_only: bool,
    pub pod: PodContext,
    pub label: ClientLabel,
}

/// Reads a record; `None` if there is none.
fn load<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, Error> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Error::Internal(format!("reading {}: {e}", path.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io(format!("reading {}", path.display()), e)),
    }
}

/// Writes a record whole, creating its directory (0700).
fn store<T: Serialize>(path: &Path, value: &T) -> Result<(), Error> {
    let write = || -> io::Result<()> {
        if let Some(dir) = path.parent() {
            DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
        agentfs_os::fsutil::write_json_atomic(path, value)
    };
    write().map_err(|e| Error::io(format!("writing {}", path.display()), e))
}

/// The claim record of `handle`, if the claim exists on this node.
pub fn load_claim(layout: &Layout, handle: &str) -> Result<Option<ClaimRecord>, Error> {
    load(&layout.csi_json(handle))
}

/// The name and profile of the tree volume in the directory `volume`: from
/// its `csi.json`, else from its `csi.retained.json` (a kept volume).
/// `None` when neither names a volume of its own.
pub fn load_volume_claim(
    layout: &Layout,
    volume: &str,
) -> Result<Option<(VolumeName, String)>, Error> {
    let record = match load_claim(layout, volume)? {
        Some(record) => Some(record),
        None => load::<ClaimRecord>(&layout.retained_json(volume))?,
    };
    Ok(match record.map(|r| r.claim) {
        Some(ClaimKind::Volume { name, profile }) => Some((name, profile)),
        Some(ClaimKind::Fork { .. }) | None => None,
    })
}

/// Writes the claim record of `handle`.
pub fn store_claim(layout: &Layout, handle: &str, record: &ClaimRecord) -> Result<(), Error> {
    store(&layout.csi_json(handle), record)
}

/// Every published record, in name order, and the names under the
/// published directory that are no readable record (an older driver's
/// node state among them), which are left where they are.
pub fn load_published(layout: &Layout) -> (Vec<PublishedRecord>, Vec<String>) {
    let dir = layout.published_dir();
    let mut paths: Vec<PathBuf> = match fs::read_dir(&dir) {
        Ok(entries) => entries.filter_map(Result::ok).map(|e| e.path()).collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return (Vec::new(), vec![format!("{}: {e}", dir.display())]),
    };
    paths.sort();
    let mut good = Vec::new();
    let mut bad = Vec::new();
    for path in paths {
        let name = path.display().to_string();
        if path.extension().is_some_and(|x| x == "tmp") {
            continue;
        }
        if !path.is_file() || path.extension().is_none_or(|x| x != "json") {
            bad.push(name);
            continue;
        }
        match load::<PublishedRecord>(&path) {
            Ok(Some(record)) => good.push(record),
            Ok(None) => {}
            Err(e) => bad.push(format!("{name}: {e}")),
        }
    }
    (good, bad)
}

/// Writes a published record.
pub fn store_published(layout: &Layout, record: &PublishedRecord) -> Result<(), Error> {
    store(&layout.published(&record.handle), record)
}

/// Removes the published record of `handle`; a missing one is fine.
pub fn remove_published(layout: &Layout, handle: &str) -> Result<(), Error> {
    let path = layout.published(handle);
    match fs::remove_file(&path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(Error::io(format!("removing {}", path.display()), e))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use agentfs_contracts::name::{ForkKey, Label};

    use super::*;

    fn layout(dir: &Path) -> Layout {
        Layout {
            base: dir.join("base"),
            node_state: dir.join("node"),
            profiles: dir.join("profiles"),
        }
    }

    fn volume_claim() -> ClaimRecord {
        ClaimRecord {
            v: RECORD_VERSION,
            capacity_bytes: 1 << 30,
            claim: ClaimKind::Volume {
                name: VolumeName::parse("apps.data.pvc-1").unwrap(),
                profile: "p".into(),
            },
        }
    }

    #[test]
    fn claim_records_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        assert_eq!(load_claim(&l, "pvc-1").unwrap(), None);
        assert_eq!(load_volume_claim(&l, "pvc-1").unwrap(), None);
        store_claim(&l, "pvc-1", &volume_claim()).unwrap();
        assert_eq!(load_claim(&l, "pvc-1").unwrap(), Some(volume_claim()));
        let text = fs::read_to_string(l.csi_json("pvc-1")).unwrap();
        assert!(text.contains("\"kind\": \"volume\""), "{text}");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&l.csi_json("pvc-1")), 0o600);
        assert_eq!(mode(&l.volume_dir("pvc-1")), 0o700);
        let found = load_volume_claim(&l, "pvc-1").unwrap().unwrap();
        assert_eq!(found.0.as_str(), "apps.data.pvc-1");
        assert_eq!(found.1, "p");
        // A kept volume is found by its retained record.
        fs::rename(l.csi_json("pvc-1"), l.retained_json("pvc-1")).unwrap();
        assert_eq!(load_claim(&l, "pvc-1").unwrap(), None);
        assert_eq!(load_volume_claim(&l, "pvc-1").unwrap(), Some(found));

        let fork = ClaimRecord {
            v: RECORD_VERSION,
            capacity_bytes: 0,
            claim: ClaimKind::Fork {
                source: "pvc-1".into(),
            },
        };
        store_claim(&l, "pvc-2", &fork).unwrap();
        assert_eq!(load_claim(&l, "pvc-2").unwrap(), Some(fork));
        assert_eq!(load_volume_claim(&l, "pvc-2").unwrap(), None);
    }

    /// The recording driver's `csi.json` is no claim record.
    #[test]
    fn a_recording_csi_json_does_not_parse() {
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        fs::create_dir_all(l.volume_dir("old")).unwrap();
        fs::write(
            l.csi_json("old"),
            r#"{"params":{"profile":"p","volume":"v","on_delete":"retain"},"capacity_bytes":1,"created":1,"static_volume":false}"#,
        )
        .unwrap();
        let err = load_claim(&l, "old").unwrap_err();
        assert!(matches!(err, Error::Internal(_)), "{err:?}");
        assert!(err.to_string().contains("csi.json"), "{err}");
        // A field this driver does not know is refused too.
        let mut json = serde_json::to_value(volume_claim()).unwrap();
        json["created"] = serde_json::json!(1);
        fs::write(l.csi_json("old"), json.to_string()).unwrap();
        assert!(load_claim(&l, "old").is_err());
    }

    #[test]
    fn published_records_round_trip_and_bad_ones_are_listed() {
        let dir = tempfile::tempdir().unwrap();
        let l = layout(dir.path());
        assert_eq!(load_published(&l), (Vec::new(), Vec::new()));
        let record = |handle: &str, tree: TreeRef| PublishedRecord {
            v: RECORD_VERSION,
            handle: handle.into(),
            volume: "pvc-1".into(),
            tree,
            target: format!("/var/lib/kubelet/pods/{handle}/mount").into(),
            read_only: false,
            pod: PodContext {
                namespace: Some(Label::parse("apps").unwrap()),
                pod: Some(Label::parse("w-0").unwrap()),
                pod_uid: None,
                node: Some(Label::parse("n1").unwrap()),
            },
            label: ClientLabel::parse("csi-0123456789abcdef").unwrap(),
        };
        let b = record("b", TreeRef::Fork(ForkKey::parse("b").unwrap()));
        let a = record("a", TreeRef::Root);
        store_published(&l, &b).unwrap();
        store_published(&l, &a).unwrap();
        // An older driver's node state: a directory per volume, and junk.
        fs::create_dir_all(l.published_dir().join("old-volume")).unwrap();
        fs::write(l.published("c"), "{").unwrap();
        let (good, bad) = load_published(&l);
        assert_eq!(good, vec![a, b]);
        assert_eq!(bad.len(), 2, "{bad:?}");
        assert!(bad.iter().any(|b| b.ends_with("old-volume")), "{bad:?}");
        assert!(bad.iter().any(|b| b.contains("c.json")), "{bad:?}");
        remove_published(&l, "a").unwrap();
        remove_published(&l, "a").unwrap();
        assert!(!l.published("a").exists());
    }
}
