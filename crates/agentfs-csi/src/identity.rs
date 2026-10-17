//! The CSI Identity service (design §14.2): the driver's name and version,
//! and its capabilities: the Controller service, served by every node
//! plugin (F8 R8), and volume accessibility constraints, since a worktree
//! lives on one node's disk ([`TOPOLOGY_KEY`]).

use std::collections::HashMap;

use tonic::{Request, Response, Status};

use crate::DRIVER_NAME;
use crate::csi::v1::identity_server::Identity;
use crate::csi::v1::plugin_capability::{self, service};
use crate::csi::v1::{
    GetPluginCapabilitiesRequest, GetPluginCapabilitiesResponse, GetPluginInfoRequest,
    GetPluginInfoResponse, PluginCapability, ProbeRequest, ProbeResponse,
};

/// The topology key of every agentfs worktree: the node whose disk holds
/// it. NodeGetInfo reports it (the kubelet labels the node with it), and
/// CreateVolume answers it, so the PV's node affinity keeps a worktree's
/// pods on its node.
pub const TOPOLOGY_KEY: &str = "topology.agentfs.mcurcio.github.io/node";

#[derive(Debug, Clone, Default)]
pub struct IdentityService;

impl IdentityService {
    pub fn new() -> Self {
        Self
    }
}

#[tonic::async_trait]
impl Identity for IdentityService {
    async fn get_plugin_info(
        &self,
        _request: Request<GetPluginInfoRequest>,
    ) -> Result<Response<GetPluginInfoResponse>, Status> {
        Ok(Response::new(GetPluginInfoResponse {
            name: DRIVER_NAME.to_owned(),
            vendor_version: env!("CARGO_PKG_VERSION").to_owned(),
            manifest: HashMap::new(),
        }))
    }

    async fn get_plugin_capabilities(
        &self,
        _request: Request<GetPluginCapabilitiesRequest>,
    ) -> Result<Response<GetPluginCapabilitiesResponse>, Status> {
        let capabilities = [
            service::Type::ControllerService,
            service::Type::VolumeAccessibilityConstraints,
        ]
        .into_iter()
        .map(|t| PluginCapability {
            r#type: Some(plugin_capability::Type::Service(
                plugin_capability::Service { r#type: t as i32 },
            )),
        })
        .collect();
        Ok(Response::new(GetPluginCapabilitiesResponse {
            capabilities,
        }))
    }

    async fn probe(
        &self,
        _request: Request<ProbeRequest>,
    ) -> Result<Response<ProbeResponse>, Status> {
        Ok(Response::new(ProbeResponse { ready: Some(true) }))
    }
}
