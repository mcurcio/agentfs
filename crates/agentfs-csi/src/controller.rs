//! The CSI Controller service (docs/csi.md), served by every node plugin
//! pod, with csi-provisioner beside it (`--node-deployment`): each node
//! provisions and deletes the volumes on its own disk.
//!
//! - CreateVolume makes `<base>/<pv-name>/state` a tree volume on this node
//!   with `agentfs init` (Volume's ensure), which the request's
//!   accessibility requirements must include (`RESOURCE_EXHAUSTED`
//!   otherwise), installs the profile's rule files, writes `csi.json`, and
//!   answers with this node's topology. Clones go only into fork classes.
//! - CreateVolume of a fork class makes a fork claim: a keyed fork, under
//!   the claim's handle, of its `dataSource` claim's tree, made by Trees on
//!   the source volume's host on this node. It writes only `csi.json`
//!   (`ClaimKind::Fork`). A fork of a fork claim and `landing: approval`
//!   are refused by Trees (`INVALID_ARGUMENT`).
//! - DeleteVolume refuses a volume in use on this node (any claim's view
//!   on its host, a fork claim's included), drains the volume through its
//!   host (Volume releases it) and keeps the directory, renaming
//!   `csi.json` to `csi.retained.json`: deleting a volume is not built. A
//!   fork claim's DeleteVolume ends its fork by key and removes its
//!   directory. A recording-format volume is never served or deleted:
//!   `FAILED_PRECONDITION`, naming the directory and the moving runbook.
//! - ControllerPublish and ControllerUnpublish are `UNIMPLEMENTED`: there
//!   is no attach step (`attachRequired: false`); the PV's node affinity
//!   keeps a volume's pods on its node.
//!
//! Calls are serialised.

use std::fs;
use std::sync::Arc;

use agentfs_contracts::host::TreeRef;
use agentfs_contracts::name::ForkKey;
use agentfs_contracts::ports::{Found, StateArea as _};
use agentfs_engine::state_area::TreeStateArea;
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};

use crate::csi::v1::controller_server::Controller;
use crate::csi::v1::controller_service_capability::{self, rpc};
use crate::csi::v1::validate_volume_capabilities_response::Confirmed;
use crate::csi::v1::volume_content_source;
use crate::csi::v1::{
    ControllerGetCapabilitiesRequest, ControllerGetCapabilitiesResponse,
    ControllerPublishVolumeRequest, ControllerPublishVolumeResponse, ControllerServiceCapability,
    ControllerUnpublishVolumeRequest, ControllerUnpublishVolumeResponse, CreateVolumeRequest,
    CreateVolumeResponse, DeleteVolumeRequest, DeleteVolumeResponse, Topology,
    ValidateVolumeCapabilitiesRequest, ValidateVolumeCapabilitiesResponse, Volume,
};
use crate::error::Error;
use crate::identity::TOPOLOGY_KEY;
use crate::layout::{Layout, validate_handle};
use crate::params::{ForkClass, ParamError, VolumeParams, is_fork_class};
use crate::records::{self, ClaimKind, ClaimRecord, RECORD_VERSION};
use crate::supervisor::Supervisor;
use crate::volume;

#[derive(Clone)]
pub struct ControllerService {
    layout: Arc<Layout>,
    node_id: Arc<str>,
    supervisor: Arc<Supervisor>,
    serial: Arc<Mutex<()>>,
}

impl ControllerService {
    pub fn new(layout: Layout, node_id: String, supervisor: Arc<Supervisor>) -> Self {
        Self {
            layout: Arc::new(layout),
            node_id: node_id.into(),
            supervisor,
            serial: Arc::new(Mutex::new(())),
        }
    }
}

/// What a state area holds; a failure to read it is `INTERNAL`.
pub(crate) fn inspect(layout: &Layout, handle: &str) -> Result<Found, Error> {
    TreeStateArea::at(&layout.state(handle))
        .inspect()
        .map_err(|e| Error::Internal(e.to_string()))
}

/// What a recording-format volume is told: it is never served or deleted.
pub(crate) fn recording_format(layout: &Layout, handle: &str) -> Error {
    Error::FailedPrecondition(format!(
        "volume {handle} is a recording-format volume ({}): this driver never serves or deletes \
         it; archive it and remove the directory by hand (docs/runbooks/moving.md)",
        layout.volume_dir(handle).display()
    ))
}

fn capability(t: rpc::Type) -> ControllerServiceCapability {
    ControllerServiceCapability {
        r#type: Some(controller_service_capability::Type::Rpc(
            controller_service_capability::Rpc { r#type: t as i32 },
        )),
    }
}

fn required(value: &str, what: &str) -> Result<(), Error> {
    if value.is_empty() {
        Err(Error::InvalidArgument(format!("{what} is required")))
    } else {
        Ok(())
    }
}

/// The capacity to record: required, else the limit, else 0.
fn capacity(req: &CreateVolumeRequest) -> Result<i64, Error> {
    let Some(range) = req.capacity_range else {
        return Ok(0);
    };
    if range.required_bytes < 0 || range.limit_bytes < 0 {
        return Err(Error::InvalidArgument("negative capacity".into()));
    }
    if range.limit_bytes > 0 && range.required_bytes > range.limit_bytes {
        return Err(Error::InvalidArgument(format!(
            "required capacity {} exceeds the limit {}",
            range.required_bytes, range.limit_bytes
        )));
    }
    Ok(if range.required_bytes > 0 {
        range.required_bytes
    } else {
        range.limit_bytes
    })
}

/// This node's topology segment: `{TOPOLOGY_KEY: node_id}`.
pub fn topology(node_id: &str) -> Topology {
    Topology {
        segments: [(TOPOLOGY_KEY.to_owned(), node_id.to_owned())].into(),
    }
}

/// `RESOURCE_EXHAUSTED` when the request's requisite topologies name
/// other nodes only: a volume is made on this node's disk or not at
/// all. A segment without this driver's key does not constrain.
fn check_accessible(req: &CreateVolumeRequest, node_id: &str) -> Result<(), Error> {
    let Some(wanted) = &req.accessibility_requirements else {
        return Ok(());
    };
    let here = |t: &Topology| {
        t.segments
            .get(TOPOLOGY_KEY)
            .is_none_or(|node| node == node_id)
    };
    if wanted.requisite.is_empty() || wanted.requisite.iter().any(here) {
        return Ok(());
    }
    let nodes: Vec<&str> = wanted
        .requisite
        .iter()
        .filter_map(|t| t.segments.get(TOPOLOGY_KEY).map(String::as_str))
        .collect();
    Err(Error::ResourceExhausted(format!(
        "the volume must be on node {}, and this is node {node_id}: each node provisions its own \
         volumes",
        nodes.join(" or ")
    )))
}

/// What a fork class without a `dataSource` is told.
pub const FORK_NEEDS_SOURCE: &str = "a fork class needs a dataSource claim";

/// The fork claim's key: its handle.
fn fork_key(handle: &str) -> Result<ForkKey, Error> {
    ForkKey::parse(handle).map_err(|e| Error::InvalidArgument(e.to_string()))
}

/// What a claim with a recording-format volume of its name is told.
fn recording_format_exists(layout: &Layout, handle: &str) -> Error {
    Error::AlreadyExists(format!(
        "volume {handle}: a recording-format volume is here ({}); move it with \
         docs/runbooks/moving.md",
        layout.volume_dir(handle).display()
    ))
}

impl ControllerService {
    async fn create_volume(
        &self,
        req: &CreateVolumeRequest,
    ) -> Result<CreateVolumeResponse, Error> {
        let layout = self.layout.as_ref();
        let node_id = &*self.node_id;
        required(&req.name, "the volume name")?;
        let handle = req.name.as_str();
        validate_handle(handle).map_err(|e| Error::InvalidArgument(e.to_string()))?;
        volume::check_capabilities(&req.volume_capabilities)?;
        let fork_class = is_fork_class(&req.parameters);
        let source = match req.volume_content_source.as_ref().map(|s| &s.r#type) {
            None => None,
            Some(Some(volume_content_source::Type::Snapshot(_))) => {
                return Err(Error::InvalidArgument("snapshots are not supported".into()));
            }
            Some(Some(volume_content_source::Type::Volume(_))) if !fork_class => {
                return Err(Error::InvalidArgument(
                    "cloning needs a fork class (fork: \"true\")".into(),
                ));
            }
            Some(Some(volume_content_source::Type::Volume(v))) => Some(v.volume_id.as_str()),
            Some(None) => {
                return Err(Error::InvalidArgument(
                    "the volume content source names no volume".into(),
                ));
            }
        };
        if fork_class {
            let source = source.ok_or_else(|| Error::InvalidArgument(FORK_NEEDS_SOURCE.into()))?;
            return self.create_fork(req, source).await;
        }
        let invalid = |e: ParamError| Error::InvalidArgument(e.to_string());
        let params = VolumeParams::from_create(handle, &req.parameters).map_err(invalid)?;
        let capacity_bytes = capacity(req)?;
        volume::profile_config(layout, &params.profile)?;
        check_accessible(req, node_id)?;
        let claim = ClaimKind::Volume {
            name: params.name.clone(),
            profile: params.profile.clone(),
        };
        let response = |capacity_bytes| CreateVolumeResponse {
            volume: Some(Volume {
                capacity_bytes,
                volume_id: handle.to_owned(),
                volume_context: params.to_context(),
                content_source: None,
                accessible_topology: vec![topology(node_id)],
            }),
        };
        // The state first: a recording driver's csi.json is no claim record.
        if inspect(layout, handle)? == Found::Other {
            return Err(recording_format_exists(layout, handle));
        }
        if existing(layout, handle, capacity_bytes, &claim)? {
            return Ok(response(capacity_bytes));
        }
        self.supervisor.ensure(handle, &params).await?;
        records::store_claim(
            layout,
            handle,
            &ClaimRecord {
                v: RECORD_VERSION,
                capacity_bytes,
                claim,
            },
        )?;
        eprintln!(
            "created volume {handle} on node {node_id} (agentfs volume {}, profile {})",
            params.name, params.profile
        );
        Ok(response(capacity_bytes))
    }

    /// CreateVolume of a fork class with the source claim `source`: a keyed
    /// fork of the source's tree under the claim's handle, made on the
    /// source volume's host.
    async fn create_fork(
        &self,
        req: &CreateVolumeRequest,
        source: &str,
    ) -> Result<CreateVolumeResponse, Error> {
        let layout = self.layout.as_ref();
        let node_id = &*self.node_id;
        let handle = req.name.as_str();
        let invalid = |e: ParamError| Error::InvalidArgument(e.to_string());
        let class = ForkClass::from_create(&req.parameters).map_err(invalid)?;
        let capacity_bytes = capacity(req)?;
        check_accessible(req, node_id)?;
        validate_handle(source).map_err(|e| Error::InvalidArgument(e.to_string()))?;
        if inspect(layout, source)? == Found::Other {
            return Err(recording_format(layout, source));
        }
        let Some(source_record) = records::load_claim(layout, source)? else {
            return Err(Error::FailedPrecondition(format!(
                "source volume {source} is not on node {node_id}: a fork claim is made on its \
                 source's node (give its pod affinity to the source's pods)"
            )));
        };
        // The source's tree: a volume's root, or a fork claim's fork (which
        // Trees refuses: forks of forks are not built).
        let (volume, upstream) = match source_record.claim {
            ClaimKind::Volume { .. } => (source.to_owned(), TreeRef::Root),
            ClaimKind::Fork { source: dir } => (dir, TreeRef::Fork(fork_key(source)?)),
        };
        let key = fork_key(handle)?;
        let claim = ClaimKind::Fork {
            source: volume.clone(),
        };
        let response = || CreateVolumeResponse {
            volume: Some(Volume {
                capacity_bytes,
                volume_id: handle.to_owned(),
                volume_context: [("source".to_owned(), volume.clone())].into(),
                content_source: req.volume_content_source.clone(),
                accessible_topology: vec![topology(node_id)],
            }),
        };
        if inspect(layout, handle)? == Found::Other {
            return Err(recording_format_exists(layout, handle));
        }
        if existing(layout, handle, capacity_bytes, &claim)? {
            return Ok(response());
        }
        let tree = self
            .supervisor
            .create_fork(&volume, upstream, key, class.settings)
            .await?;
        records::store_claim(
            layout,
            handle,
            &ClaimRecord {
                v: RECORD_VERSION,
                capacity_bytes,
                claim,
            },
        )?;
        eprintln!("created fork {tree} of volume {volume} for claim {handle}");
        Ok(response())
    }

    async fn delete_volume(&self, req: &DeleteVolumeRequest) -> Result<(), Error> {
        let layout = self.layout.as_ref();
        required(&req.volume_id, "the volume id")?;
        let handle = req.volume_id.as_str();
        if validate_handle(handle).is_err() {
            // Not one of ours, so already gone.
            return Ok(());
        }
        // The state first: a recording driver's csi.json is no claim record.
        let found = inspect(layout, handle)?;
        if found == Found::Other {
            return Err(recording_format(layout, handle));
        }
        let Some(record) = records::load_claim(layout, handle)? else {
            return Ok(());
        };
        match record.claim {
            ClaimKind::Fork { source } => {
                if let Some(published) = self.supervisor.claim_in_use(&source, handle).await {
                    return Err(in_use(handle, &published));
                }
                // A source whose state is gone has taken its forks with it.
                match inspect(layout, &source)? {
                    Found::Volume(_) => {
                        self.supervisor.end_fork(&source, fork_key(handle)?).await?
                    }
                    Found::Nothing => {}
                    Found::Other => return Err(recording_format(layout, &source)),
                }
                let dir = layout.volume_dir(handle);
                match fs::remove_dir_all(&dir) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                        return Err(Error::io(format!("removing {}", dir.display()), e));
                    }
                    _ => {}
                }
                eprintln!("ended fork of claim {handle} in volume {source}");
                Ok(())
            }
            ClaimKind::Volume { .. } => {
                if let Some(published) = self.supervisor.in_use(handle).await {
                    return Err(in_use(handle, &published));
                }
                let dir = layout.volume_dir(handle);
                if found == Found::Nothing {
                    keep(layout, handle)?;
                    eprintln!("volume {handle} had no state area");
                    return Ok(());
                }
                self.supervisor.release(handle).await?;
                keep(layout, handle)?;
                eprintln!(
                    "kept volume {handle} at {}: a deleted claim's tree volume is kept (deleting \
                     it is not built); its history is {}/state/history.git",
                    dir.display(),
                    dir.display()
                );
                Ok(())
            }
        }
    }
}

/// What DeleteVolume of a claim in use is told.
fn in_use(handle: &str, published: &records::PublishedRecord) -> Error {
    Error::FailedPrecondition(format!(
        "volume {handle} is in use on this node (published at {}); nothing deleted",
        published.target.display()
    ))
}

/// Whether the claim exists already as asked (CreateVolume is
/// idempotent); `ALREADY_EXISTS` when it exists otherwise.
fn existing(
    layout: &Layout,
    handle: &str,
    capacity_bytes: i64,
    claim: &ClaimKind,
) -> Result<bool, Error> {
    let Some(existing) = records::load_claim(layout, handle)? else {
        return Ok(false);
    };
    if existing.capacity_bytes != capacity_bytes {
        return Err(Error::AlreadyExists(format!(
            "volume {handle} exists with capacity {} bytes, not {capacity_bytes}",
            existing.capacity_bytes
        )));
    }
    if existing.claim != *claim {
        return Err(Error::AlreadyExists(format!(
            "volume {handle} exists with other parameters ({:?})",
            existing.claim
        )));
    }
    Ok(true)
}

/// Renames `csi.json` to `csi.retained.json`: the claim is gone, the
/// volume stays.
fn keep(layout: &Layout, handle: &str) -> Result<(), Error> {
    let to = layout.retained_json(handle);
    fs::rename(layout.csi_json(handle), &to)
        .map_err(|e| Error::io(format!("writing {}", to.display()), e))
}

pub(crate) fn validate(
    layout: &Layout,
    req: &ValidateVolumeCapabilitiesRequest,
) -> Result<ValidateVolumeCapabilitiesResponse, Error> {
    required(&req.volume_id, "the volume id")?;
    if req.volume_capabilities.is_empty() {
        return Err(Error::InvalidArgument(
            "volume capabilities are required".into(),
        ));
    }
    let handle = req.volume_id.as_str();
    let not_found = || Error::NotFound(format!("volume {handle} does not exist"));
    validate_handle(handle).map_err(|_| not_found())?;
    if records::load_claim(layout, handle)?.is_none() {
        return Err(not_found());
    }
    Ok(match volume::check_capabilities(&req.volume_capabilities) {
        Ok(()) => ValidateVolumeCapabilitiesResponse {
            confirmed: Some(Confirmed {
                volume_context: req.volume_context.clone(),
                volume_capabilities: req.volume_capabilities.clone(),
                parameters: req.parameters.clone(),
                mutable_parameters: req.mutable_parameters.clone(),
            }),
            message: String::new(),
        },
        Err(e) => ValidateVolumeCapabilitiesResponse {
            confirmed: None,
            message: e.to_string(),
        },
    })
}

#[tonic::async_trait]
impl Controller for ControllerService {
    async fn create_volume(
        &self,
        request: Request<CreateVolumeRequest>,
    ) -> Result<Response<CreateVolumeResponse>, Status> {
        let req = request.into_inner();
        let _serial = self.serial.lock().await;
        Ok(Response::new(self.create_volume(&req).await?))
    }

    async fn delete_volume(
        &self,
        request: Request<DeleteVolumeRequest>,
    ) -> Result<Response<DeleteVolumeResponse>, Status> {
        let req = request.into_inner();
        let _serial = self.serial.lock().await;
        self.delete_volume(&req).await?;
        Ok(Response::new(DeleteVolumeResponse {}))
    }

    async fn controller_publish_volume(
        &self,
        _request: Request<ControllerPublishVolumeRequest>,
    ) -> Result<Response<ControllerPublishVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "agentfs-csi has no attach step (attachRequired: false): a volume's pods run on its \
             node",
        ))
    }

    async fn controller_unpublish_volume(
        &self,
        _request: Request<ControllerUnpublishVolumeRequest>,
    ) -> Result<Response<ControllerUnpublishVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "agentfs-csi has no attach step (attachRequired: false)",
        ))
    }

    async fn validate_volume_capabilities(
        &self,
        request: Request<ValidateVolumeCapabilitiesRequest>,
    ) -> Result<Response<ValidateVolumeCapabilitiesResponse>, Status> {
        let req = request.into_inner();
        let _serial = self.serial.lock().await;
        Ok(Response::new(validate(&self.layout, &req)?))
    }

    async fn controller_get_capabilities(
        &self,
        _request: Request<ControllerGetCapabilitiesRequest>,
    ) -> Result<Response<ControllerGetCapabilitiesResponse>, Status> {
        Ok(Response::new(ControllerGetCapabilitiesResponse {
            capabilities: [
                rpc::Type::CreateDeleteVolume,
                rpc::Type::SingleNodeMultiWriter,
                rpc::Type::CloneVolume,
            ]
            .into_iter()
            .map(capability)
            .collect(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csi::v1::TopologyRequirement;
    use crate::csi::v1::VolumeCapability;
    use crate::csi::v1::volume_capability::access_mode::Mode;
    use crate::csi::v1::volume_capability::{AccessMode, AccessType, MountVolume};

    fn request(requisite: &[&str]) -> CreateVolumeRequest {
        CreateVolumeRequest {
            name: "v".into(),
            volume_capabilities: vec![VolumeCapability {
                access_mode: Some(AccessMode {
                    mode: Mode::SingleNodeWriter as i32,
                }),
                access_type: Some(AccessType::Mount(MountVolume::default())),
            }],
            accessibility_requirements: Some(TopologyRequirement {
                requisite: requisite.iter().map(|n| topology(n)).collect(),
                preferred: Vec::new(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn only_this_nodes_volumes_are_provisioned_here() {
        check_accessible(&request(&[]), "node-a").unwrap();
        check_accessible(&request(&["node-a"]), "node-a").unwrap();
        check_accessible(&request(&["node-b", "node-a"]), "node-a").unwrap();
        let err = check_accessible(&request(&["node-b"]), "node-a").unwrap_err();
        let Error::ResourceExhausted(message) = err else {
            panic!("{err:?}");
        };
        assert!(message.contains("node node-b"), "{message}");
        // A segment without this driver's key does not constrain.
        let mut req = request(&[]);
        req.accessibility_requirements
            .as_mut()
            .unwrap()
            .requisite
            .push(Topology {
                segments: [("zone".to_owned(), "z1".to_owned())].into(),
            });
        check_accessible(&req, "node-a").unwrap();
    }
}
