//! The CSI Identity service (design §14.2): the driver's name and version,
//! and whether this process serves the Controller service.

use std::collections::HashMap;

use tonic::{Request, Response, Status};

use crate::DRIVER_NAME;
use crate::csi::v1::identity_server::Identity;
use crate::csi::v1::plugin_capability::{self, service};
use crate::csi::v1::{
    GetPluginCapabilitiesRequest, GetPluginCapabilitiesResponse, GetPluginInfoRequest,
    GetPluginInfoResponse, PluginCapability, ProbeRequest, ProbeResponse,
};

#[derive(Debug, Clone)]
pub struct IdentityService {
    controller: bool,
}

impl IdentityService {
    /// `controller`: this process serves the Controller service.
    pub fn new(controller: bool) -> Self {
        Self { controller }
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
        let capabilities = self
            .controller
            .then(|| PluginCapability {
                r#type: Some(plugin_capability::Type::Service(
                    plugin_capability::Service {
                        r#type: service::Type::ControllerService as i32,
                    },
                )),
            })
            .into_iter()
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
