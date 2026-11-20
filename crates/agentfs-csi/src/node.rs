//! The CSI Node service (docs/csi.md): validates the calls, resolves the
//! claim, and hands them to the [`Supervisor`]. There is no stage step:
//! NodePublish opens the pod's view at the kubelet's target on the volume's
//! host. NodeGetInfo reports this node's topology
//! ([`crate::identity::TOPOLOGY_KEY`]), which the kubelet puts on the node
//! as a label and the PVs' node affinity matches.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tonic::{Request, Response, Status};

use crate::csi::v1::node_server::Node;
use crate::csi::v1::node_service_capability::{self, rpc};
use crate::csi::v1::volume_capability::access_mode::Mode;
use crate::csi::v1::{
    NodeGetCapabilitiesRequest, NodeGetCapabilitiesResponse, NodeGetInfoRequest,
    NodeGetInfoResponse, NodePublishVolumeRequest, NodePublishVolumeResponse,
    NodeServiceCapability, NodeStageVolumeRequest, NodeStageVolumeResponse,
    NodeUnpublishVolumeRequest, NodeUnpublishVolumeResponse, NodeUnstageVolumeRequest,
    NodeUnstageVolumeResponse, VolumeCapability,
};
use agentfs_contracts::host::TreeRef;
use agentfs_contracts::ports::Found;

use crate::controller::{FORKS_NOT_BUILT, inspect, recording_format};
use crate::error::Error;
use crate::layout::validate_handle;
use crate::params::{VolumeParams, is_ephemeral, pod_context};
use crate::records::{self, ClaimKind, ClaimRecord, RECORD_VERSION};
use crate::supervisor::{Supervisor, ViewSpec};
use crate::volume::check_capability;

/// What an inline ephemeral volume is told.
pub const EPHEMERAL_REFUSED: &str = "inline ephemeral volumes are not supported \
    (volumeLifecycleModes: Persistent): use a PersistentVolumeClaim of an agentfs StorageClass";

#[derive(Debug, Clone)]
pub struct NodeService {
    supervisor: Arc<Supervisor>,
}

impl NodeService {
    pub fn new(supervisor: Arc<Supervisor>) -> Self {
        Self { supervisor }
    }
}

fn required(value: &str, what: &str) -> Result<(), Error> {
    if value.is_empty() {
        Err(Error::InvalidArgument(format!("{what} is required")))
    } else {
        Ok(())
    }
}

fn absolute(value: &str, what: &str) -> Result<PathBuf, Error> {
    required(value, what)?;
    let path = Path::new(value);
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Err(Error::InvalidArgument(format!(
            "{what} {value:?} is not an absolute path"
        )))
    }
}

fn capability(cap: Option<&VolumeCapability>) -> Result<Mode, Error> {
    check_capability(
        cap.ok_or_else(|| Error::InvalidArgument("the volume capability is required".into()))?,
    )
}

fn handle(volume_id: &str) -> Result<&str, Error> {
    required(volume_id, "the volume id")?;
    validate_handle(volume_id).map_err(|e| Error::NotFound(e.to_string()))?;
    Ok(volume_id)
}

fn node_capability(t: rpc::Type) -> NodeServiceCapability {
    NodeServiceCapability {
        r#type: Some(node_service_capability::Type::Rpc(
            node_service_capability::Rpc { r#type: t as i32 },
        )),
    }
}

impl NodeService {
    /// The view NodePublish opens: the claim's own volume, from its
    /// `csi.json`, or a static PV over a kept volume, adopted (its
    /// `csi.json` written) only over a tree volume of the asked name.
    async fn resolve(
        &self,
        handle: &str,
        volume_context: &HashMap<String, String>,
    ) -> Result<(String, TreeRef), Error> {
        let config = self.supervisor.config();
        let layout = &config.layout;
        let found = inspect(layout, handle)?;
        if found == Found::Other {
            return Err(recording_format(layout, handle));
        }
        if let Some(record) = records::load_claim(layout, handle)? {
            return match record.claim {
                ClaimKind::Volume { .. } => Ok((handle.to_owned(), TreeRef::Root)),
                ClaimKind::Fork { .. } => Err(Error::Internal(FORKS_NOT_BUILT.into())),
            };
        }
        let Found::Volume(identity) = found else {
            return Err(Error::NotFound(format!(
                "no volume {handle} on node {} under {}; the pod waits",
                config.node_id,
                layout.base.display()
            )));
        };
        let params = VolumeParams::from_static(handle, volume_context)
            .map_err(|e| Error::InvalidArgument(e.to_string()))?;
        if identity.name != params.name {
            return Err(Error::AlreadyExists(format!(
                "volume {handle} holds agentfs volume {}, not {}",
                identity.name, params.name
            )));
        }
        self.supervisor.ensure(handle, &params).await?;
        records::store_claim(
            layout,
            handle,
            &ClaimRecord {
                v: RECORD_VERSION,
                capacity_bytes: 0,
                claim: ClaimKind::Volume {
                    name: params.name.clone(),
                    profile: params.profile.clone(),
                },
            },
        )?;
        eprintln!(
            "adopted volume {handle} (agentfs volume {}, profile {}) through a static PV",
            params.name, params.profile
        );
        Ok((handle.to_owned(), TreeRef::Root))
    }
}

#[tonic::async_trait]
impl Node for NodeService {
    async fn node_stage_volume(
        &self,
        _request: Request<NodeStageVolumeRequest>,
    ) -> Result<Response<NodeStageVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "agentfs-csi has no stage step (no STAGE_UNSTAGE_VOLUME): NodePublish opens the view",
        ))
    }

    async fn node_unstage_volume(
        &self,
        _request: Request<NodeUnstageVolumeRequest>,
    ) -> Result<Response<NodeUnstageVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "agentfs-csi has no stage step (no STAGE_UNSTAGE_VOLUME)",
        ))
    }

    async fn node_publish_volume(
        &self,
        request: Request<NodePublishVolumeRequest>,
    ) -> Result<Response<NodePublishVolumeResponse>, Status> {
        let req = request.into_inner();
        required(&req.volume_id, "the volume id")?;
        let target = absolute(&req.target_path, "the target path")?;
        let mode = capability(req.volume_capability.as_ref())?;
        if is_ephemeral(&req.volume_context) {
            return Err(Error::InvalidArgument(EPHEMERAL_REFUSED.into()).into());
        }
        let handle = handle(&req.volume_id)?.to_owned();
        let pod = pod_context(&req.volume_context, &self.supervisor.config().node_id)
            .map_err(|e| Error::InvalidArgument(e.to_string()))?;
        let (volume, tree) = self.resolve(&handle, &req.volume_context).await?;
        self.supervisor
            .publish(ViewSpec {
                handle,
                volume,
                tree,
                target,
                read_only: req.readonly || mode == Mode::SingleNodeReaderOnly,
                pod,
            })
            .await?;
        Ok(Response::new(NodePublishVolumeResponse {}))
    }

    async fn node_unpublish_volume(
        &self,
        request: Request<NodeUnpublishVolumeRequest>,
    ) -> Result<Response<NodeUnpublishVolumeResponse>, Status> {
        let req = request.into_inner();
        required(&req.volume_id, "the volume id")?;
        let target = absolute(&req.target_path, "the target path")?;
        if validate_handle(&req.volume_id).is_ok() {
            self.supervisor.unpublish(&req.volume_id, &target).await?;
        }
        Ok(Response::new(NodeUnpublishVolumeResponse {}))
    }

    async fn node_get_capabilities(
        &self,
        _request: Request<NodeGetCapabilitiesRequest>,
    ) -> Result<Response<NodeGetCapabilitiesResponse>, Status> {
        Ok(Response::new(NodeGetCapabilitiesResponse {
            capabilities: [rpc::Type::SingleNodeMultiWriter]
                .into_iter()
                .map(node_capability)
                .collect(),
        }))
    }

    async fn node_get_info(
        &self,
        _request: Request<NodeGetInfoRequest>,
    ) -> Result<Response<NodeGetInfoResponse>, Status> {
        let node_id = self.supervisor.config().node_id.clone();
        Ok(Response::new(NodeGetInfoResponse {
            accessible_topology: Some(crate::controller::topology(&node_id)),
            node_id,
            max_volumes_per_node: 0,
        }))
    }
}
