//! The driver's persisted records (design §14.2): the controller's
//! `csi.json` per volume, and each node's staged and published records,
//! from which a restarted node plugin reconciles. Written atomically
//! (mode 0600) with [`agentfs_core::fsutil::write_json_atomic`]. A field
//! this driver does not know fails the load, so a record written for a
//! behaviour it lacks is refused, never half-read. One exception: the two
//! fields of a `csi.json` written by the multi-node build (2026-10, since
//! dropped), which [`load_volume`] returns apart as [`MultiNodeFields`] so
//! that such a volume can still be detached and deleted.

use std::collections::BTreeSet;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::params::{PodInfo, VolumeParams};

/// `<base>/<handle>/csi.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeRecord {
    pub params: VolumeParams,
    /// As requested; recorded, not enforced.
    pub capacity_bytes: i64,
    /// Unix seconds.
    pub created: u64,
    /// The node ControllerPublish attached it to, if any.
    #[serde(default)]
    pub published_node: Option<String>,
    /// Adopted from a static PV rather than created by CreateVolume.
    #[serde(default)]
    pub static_volume: bool,
}

/// What the multi-node build (2026-10, since dropped) added to a
/// `csi.json`. The only home of these field names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultiNodeFields {
    #[serde(default)]
    pub multi_node: bool,
    /// The nodes ControllerPublish attached the volume to.
    #[serde(default)]
    pub published_nodes: BTreeSet<String>,
}

impl MultiNodeFields {
    const KEYS: [&str; 2] = ["multi_node", "published_nodes"];
}

/// Reads a `csi.json`, with the multi-node build's fields apart when it
/// has any; `None` if there is none. Any other unknown field fails.
pub fn load_volume(path: &Path) -> io::Result<Option<(VolumeRecord, Option<MultiNodeFields>)>> {
    let Some(mut map) = load::<serde_json::Map<String, serde_json::Value>>(path)? else {
        return Ok(None);
    };
    let mut legacy = serde_json::Map::new();
    for key in MultiNodeFields::KEYS {
        if let Some(value) = map.remove(key) {
            legacy.insert(key.to_owned(), value);
        }
    }
    let invalid = |e: serde_json::Error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {e}", path.display()),
        )
    };
    let record = serde_json::from_value(map.into()).map_err(invalid)?;
    let legacy = if legacy.is_empty() {
        None
    } else {
        Some(serde_json::from_value(legacy.into()).map_err(invalid)?)
    };
    Ok(Some((record, legacy)))
}

/// Writes a `csi.json`, keeping the multi-node build's fields while any
/// node is still in them.
pub fn store_volume(
    path: &Path,
    record: &VolumeRecord,
    legacy: Option<&MultiNodeFields>,
) -> io::Result<()> {
    match legacy {
        Some(fields) if !fields.published_nodes.is_empty() => {
            let mut value = serde_json::to_value(record).map_err(io::Error::other)?;
            if let (Some(map), serde_json::Value::Object(extra)) = (
                value.as_object_mut(),
                serde_json::to_value(fields).map_err(io::Error::other)?,
            ) {
                map.extend(extra);
            }
            store(path, &value)
        }
        _ => store(path, record),
    }
}

/// `<node-state>/staged/<handle>.json`: the volume's history service runs
/// on this node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedRecord {
    pub handle: String,
    pub params: VolumeParams,
}

/// `<node-state>/published/<handle>/<key>.json`: a front serves this
/// target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedRecord {
    pub handle: String,
    pub target: PathBuf,
    pub readonly: bool,
    /// SINGLE_NODE_SINGLE_WRITER: no other target may publish the volume.
    #[serde(default)]
    pub single_writer: bool,
    pub pod: Option<PodInfo>,
    /// The front's pending slot, preferred again after a crash.
    pub slot: Option<u16>,
    pub front_pid: Option<u32>,
    /// `--workload-mount` values for the front (`workloadMounts`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workload_mounts: Vec<String>,
}

/// Reads a record; `None` if there is none.
pub fn load<T: DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {e}", path.display()),
            )
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Writes a record atomically, creating its directory (0700).
pub fn store<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    agentfs_core::fsutil::write_json_atomic(path, value)
}

/// Removes a record; a missing one is fine.
pub fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Records that could not be read, with why.
pub type Unreadable = Vec<(PathBuf, io::Error)>;

/// Every `*.json` record directly in `dir`, in name order, with the files
/// that could not be read.
pub fn load_all<T: DeserializeOwned>(dir: &Path) -> io::Result<(Vec<T>, Unreadable)> {
    let mut paths = match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect::<Vec<_>>(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
        Err(e) => return Err(e),
    };
    paths.sort();
    let mut good = Vec::new();
    let mut bad = Vec::new();
    for path in paths {
        match load(&path) {
            Ok(Some(record)) => good.push(record),
            Ok(None) => {}
            Err(e) => bad.push((path, e)),
        }
    }
    Ok((good, bad))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::params::OnDelete;

    fn params() -> VolumeParams {
        VolumeParams {
            profile: "p".into(),
            volume: "v".into(),
            branch: None,
            on_delete: OnDelete::Delete,
            workload_mounts: Vec::new(),
        }
    }

    #[test]
    fn records_round_trip_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/csi.json");
        assert_eq!(load::<VolumeRecord>(&path).unwrap(), None);
        let rec = VolumeRecord {
            params: params(),
            capacity_bytes: 1 << 30,
            created: 1,
            published_node: Some("n".into()),
            static_volume: false,
        };
        store(&path, &rec).unwrap();
        assert_eq!(load::<VolumeRecord>(&path).unwrap(), Some(rec));
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"on_delete\": \"delete\""), "{text}");
        remove(&path).unwrap();
        remove(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn a_record_with_fields_this_driver_does_not_know_is_refused() {
        // A csi.json written by the multi-node build (2026-10, since
        // dropped): its two fields come apart, and round-trip while a node
        // is in them. Any other unknown field fails, naming it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("csi.json");
        let rec = VolumeRecord {
            params: params(),
            capacity_bytes: 1,
            created: 1,
            published_node: None,
            static_volume: false,
        };
        let fields = MultiNodeFields {
            multi_node: true,
            published_nodes: ["node-1".to_owned(), "node-2".to_owned()].into(),
        };
        store_volume(&path, &rec, Some(&fields)).unwrap();
        let err = load::<VolumeRecord>(&path).unwrap_err();
        assert!(
            err.to_string().contains("unknown field `multi_node`"),
            "{err}"
        );
        assert_eq!(
            load_volume(&path).unwrap(),
            Some((rec.clone(), Some(fields.clone())))
        );
        let none_left = MultiNodeFields::default();
        store_volume(&path, &rec, Some(&none_left)).unwrap();
        assert_eq!(load_volume(&path).unwrap(), Some((rec.clone(), None)));
        let mut json = serde_json::to_value(&rec).unwrap();
        json["published_nodes"] = serde_json::json!(["node-1"]);
        json["replicas"] = 2.into();
        fs::write(&path, json.to_string()).unwrap();
        let err = load_volume(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        assert!(
            err.to_string().contains("unknown field `replicas`"),
            "{err}"
        );
        assert!(err.to_string().contains("csi.json"), "{err}");
    }

    #[test]
    fn load_all_reads_every_record_and_reports_bad_ones() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load_all::<StagedRecord>(&dir.path().join("none"))
                .unwrap()
                .0
                .is_empty()
        );
        for h in ["b", "a"] {
            store(
                &dir.path().join(format!("{h}.json")),
                &StagedRecord {
                    handle: h.into(),
                    params: params(),
                },
            )
            .unwrap();
        }
        fs::write(dir.path().join("c.json"), "{").unwrap();
        fs::write(dir.path().join("ignored.tmp"), "x").unwrap();
        let (good, bad): (Vec<StagedRecord>, _) = load_all(dir.path()).unwrap();
        assert_eq!(
            good.iter().map(|r| r.handle.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(bad.len(), 1);
        assert!(bad[0].0.ends_with("c.json"));
    }

    #[test]
    fn published_records_keep_the_pod_and_slot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.json");
        let rec = PublishedRecord {
            handle: "v".into(),
            target: "/t".into(),
            readonly: false,
            single_writer: true,
            pod: Some(PodInfo {
                namespace: "ns".into(),
                name: "p".into(),
                uid: "u".into(),
                service_account: "sa".into(),
            }),
            slot: Some(3),
            front_pid: Some(42),
            workload_mounts: vec!["/workspace".into()],
        };
        store(&path, &rec).unwrap();
        assert_eq!(load::<PublishedRecord>(&path).unwrap(), Some(rec));
    }
}
