use tonic::{Request, Response, Status};
use tracing::info;

use crate::csi::{
    identity_server::Identity, plugin_capability, GetPluginCapabilitiesRequest,
    GetPluginCapabilitiesResponse, GetPluginInfoRequest, GetPluginInfoResponse, PluginCapability,
    ProbeRequest, ProbeResponse,
};

pub const DRIVER_NAME: &str = "node-local-cache.csi.io";
pub const DRIVER_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct IdentityService {
    /// Whether this instance is running in controller mode (vs node mode)
    is_controller: bool,
}

impl IdentityService {
    /// Create a new IdentityService
    /// - `is_controller`: true if running in controller mode, false for node mode
    pub fn new(is_controller: bool) -> Self {
        Self { is_controller }
    }
}

#[tonic::async_trait]
impl Identity for IdentityService {
    async fn get_plugin_info(
        &self,
        _request: Request<GetPluginInfoRequest>,
    ) -> Result<Response<GetPluginInfoResponse>, Status> {
        info!("GetPluginInfo called");

        Ok(Response::new(GetPluginInfoResponse {
            name: DRIVER_NAME.to_string(),
            vendor_version: DRIVER_VERSION.to_string(),
            manifest: Default::default(),
        }))
    }

    async fn get_plugin_capabilities(
        &self,
        _request: Request<GetPluginCapabilitiesRequest>,
    ) -> Result<Response<GetPluginCapabilitiesResponse>, Status> {
        info!(is_controller = %self.is_controller, "GetPluginCapabilities called");

        // Only advertise ControllerService when running in controller mode
        let capabilities = if self.is_controller {
            vec![PluginCapability {
                r#type: Some(plugin_capability::Type::Service(
                    plugin_capability::Service {
                        r#type: plugin_capability::service::Type::ControllerService as i32,
                    },
                )),
            }]
        } else {
            // Node mode - no controller capabilities
            vec![]
        };

        Ok(Response::new(GetPluginCapabilitiesResponse {
            capabilities,
        }))
    }

    async fn probe(
        &self,
        _request: Request<ProbeRequest>,
    ) -> Result<Response<ProbeResponse>, Status> {
        // Always ready
        Ok(Response::new(ProbeResponse { ready: Some(true) }))
    }
}
