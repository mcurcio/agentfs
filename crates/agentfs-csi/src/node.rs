//! The CSI Node service (design §14.2): validates the calls and hands them
//! to the [`Supervisor`].

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
use crate::error::Error;
use crate::layout::validate_handle;
use crate::params::{PodInfo, is_ephemeral};
use crate::supervisor::{PublishRequest, StageRequest, Supervisor};
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

#[tonic::async_trait]
impl Node for NodeService {
    async fn node_stage_volume(
        &self,
        request: Request<NodeStageVolumeRequest>,
    ) -> Result<Response<NodeStageVolumeResponse>, Status> {
        let req = request.into_inner();
        required(&req.volume_id, "the volume id")?;
        absolute(&req.staging_target_path, "the staging target path")?;
        capability(req.volume_capability.as_ref())?;
        if is_ephemeral(&req.volume_context) {
            return Err(Error::InvalidArgument(EPHEMERAL_REFUSED.into()).into());
        }
        let handle = handle(&req.volume_id)?.to_owned();
        self.supervisor
            .stage(StageRequest {
                handle,
                volume_context: req.volume_context,
            })
            .await?;
        Ok(Response::new(NodeStageVolumeResponse {}))
    }

    async fn node_unstage_volume(
        &self,
        request: Request<NodeUnstageVolumeRequest>,
    ) -> Result<Response<NodeUnstageVolumeResponse>, Status> {
        let req = request.into_inner();
        required(&req.volume_id, "the volume id")?;
        absolute(&req.staging_target_path, "the staging target path")?;
        if validate_handle(&req.volume_id).is_ok() {
            self.supervisor.unstage(&req.volume_id).await?;
        }
        Ok(Response::new(NodeUnstageVolumeResponse {}))
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
        absolute(&req.staging_target_path, "the staging target path")?;
        let handle = handle(&req.volume_id)?.to_owned();
        self.supervisor
            .publish(PublishRequest {
                handle,
                target,
                readonly: req.readonly || mode == Mode::SingleNodeReaderOnly,
                single_writer: mode == Mode::SingleNodeSingleWriter,
                pod: PodInfo::from_context(&req.volume_context),
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
            capabilities: [
                rpc::Type::StageUnstageVolume,
                rpc::Type::SingleNodeMultiWriter,
            ]
            .into_iter()
            .map(node_capability)
            .collect(),
        }))
    }

    async fn node_get_info(
        &self,
        _request: Request<NodeGetInfoRequest>,
    ) -> Result<Response<NodeGetInfoResponse>, Status> {
        Ok(Response::new(NodeGetInfoResponse {
            node_id: self.supervisor.config().node_id.clone(),
            max_volumes_per_node: 0,
            accessible_topology: None,
        }))
    }
}
