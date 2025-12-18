use std::sync::Arc;
use tokio::sync::RwLock;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use crate::cleanup::CleanupController;
use crate::csi::{
    controller_server::Controller, controller_service_capability, ControllerExpandVolumeRequest,
    ControllerExpandVolumeResponse, ControllerGetCapabilitiesRequest,
    ControllerGetCapabilitiesResponse, ControllerGetVolumeRequest, ControllerGetVolumeResponse,
    ControllerModifyVolumeRequest, ControllerModifyVolumeResponse, ControllerPublishVolumeRequest,
    ControllerPublishVolumeResponse, ControllerServiceCapability, ControllerUnpublishVolumeRequest,
    ControllerUnpublishVolumeResponse, CreateSnapshotRequest, CreateSnapshotResponse,
    CreateVolumeRequest, CreateVolumeResponse, DeleteSnapshotRequest, DeleteSnapshotResponse,
    DeleteVolumeRequest, DeleteVolumeResponse, GetCapacityRequest, GetCapacityResponse,
    ListSnapshotsRequest, ListSnapshotsResponse, ListVolumesRequest, ListVolumesResponse,
    ValidateVolumeCapabilitiesRequest, ValidateVolumeCapabilitiesResponse, Volume,
};

use crate::volume;

pub struct ControllerService {
    cleanup: Option<Arc<RwLock<CleanupController>>>,
}

impl ControllerService {
    pub fn new() -> Self {
        Self { cleanup: None }
    }

    pub fn with_cleanup(cleanup: CleanupController) -> Self {
        Self {
            cleanup: Some(Arc::new(RwLock::new(cleanup))),
        }
    }
}

#[tonic::async_trait]
impl Controller for ControllerService {
    async fn create_volume(
        &self,
        request: Request<CreateVolumeRequest>,
    ) -> Result<Response<CreateVolumeResponse>, Status> {
        let req = request.into_inner();
        info!(name = %req.name, "CreateVolume called");

        // Generate deterministic volume ID from request name (which is pvc-<uid> from external-provisioner)
        // This ensures idempotency - retries produce the same volume ID
        let volume_id = volume::generate_volume_id(&req.name);
        let capacity_bytes = req
            .capacity_range
            .as_ref()
            .map(|c| c.required_bytes)
            .unwrap_or(0);

        info!(volume_id = %volume_id, capacity = capacity_bytes, "Volume created");

        Ok(Response::new(CreateVolumeResponse {
            volume: Some(Volume {
                volume_id,
                capacity_bytes,
                // No topology constraints - accessible from any node
                accessible_topology: vec![],
                volume_context: Default::default(),
                content_source: None,
            }),
        }))
    }

    async fn delete_volume(
        &self,
        request: Request<DeleteVolumeRequest>,
    ) -> Result<Response<DeleteVolumeResponse>, Status> {
        let req = request.into_inner();
        info!(volume_id = %req.volume_id, "DeleteVolume called");

        // Create cleanup request if cleanup controller is available
        if let Some(cleanup) = &self.cleanup {
            let cleanup = cleanup.read().await;
            if let Err(e) = cleanup.create_cleanup_request(&req.volume_id).await {
                warn!(
                    volume_id = %req.volume_id,
                    error = %e,
                    "Failed to create cleanup request, continuing anyway"
                );
            }
        }

        Ok(Response::new(DeleteVolumeResponse {}))
    }

    async fn controller_get_capabilities(
        &self,
        _request: Request<ControllerGetCapabilitiesRequest>,
    ) -> Result<Response<ControllerGetCapabilitiesResponse>, Status> {
        info!("ControllerGetCapabilities called");

        let capabilities = vec![ControllerServiceCapability {
            r#type: Some(controller_service_capability::Type::Rpc(
                controller_service_capability::Rpc {
                    r#type: controller_service_capability::rpc::Type::CreateDeleteVolume as i32,
                },
            )),
        }];

        Ok(Response::new(ControllerGetCapabilitiesResponse {
            capabilities,
        }))
    }

    async fn validate_volume_capabilities(
        &self,
        request: Request<ValidateVolumeCapabilitiesRequest>,
    ) -> Result<Response<ValidateVolumeCapabilitiesResponse>, Status> {
        let req = request.into_inner();
        info!(volume_id = %req.volume_id, "ValidateVolumeCapabilities called");

        // Validate each capability - we only support filesystem mounts, not block volumes
        for cap in &req.volume_capabilities {
            if let Some(access_type) = &cap.access_type {
                match access_type {
                    crate::csi::volume_capability::AccessType::Mount(_) => {
                        // Filesystem mounts are supported with any access mode
                        // Note: for this driver, "multi-node" access modes work but each node
                        // sees its own independent cache (that's the feature, not a bug)
                    }
                    crate::csi::volume_capability::AccessType::Block(_) => {
                        // Block volumes not supported
                        info!(volume_id = %req.volume_id, "Rejecting block volume capability");
                        return Ok(Response::new(ValidateVolumeCapabilitiesResponse {
                            confirmed: None,
                            message: "Block volumes are not supported, only filesystem mounts"
                                .to_string(),
                        }));
                    }
                }
            }
        }

        // All capabilities validated - confirm them
        Ok(Response::new(ValidateVolumeCapabilitiesResponse {
            confirmed: Some(
                crate::csi::validate_volume_capabilities_response::Confirmed {
                    volume_context: req.volume_context,
                    volume_capabilities: req.volume_capabilities,
                    parameters: req.parameters,
                    mutable_parameters: Default::default(),
                },
            ),
            message: String::new(),
        }))
    }

    // Unimplemented RPCs - not needed for our use case

    async fn controller_publish_volume(
        &self,
        _request: Request<ControllerPublishVolumeRequest>,
    ) -> Result<Response<ControllerPublishVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "ControllerPublishVolume not supported",
        ))
    }

    async fn controller_unpublish_volume(
        &self,
        _request: Request<ControllerUnpublishVolumeRequest>,
    ) -> Result<Response<ControllerUnpublishVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "ControllerUnpublishVolume not supported",
        ))
    }

    async fn list_volumes(
        &self,
        _request: Request<ListVolumesRequest>,
    ) -> Result<Response<ListVolumesResponse>, Status> {
        Err(Status::unimplemented("ListVolumes not supported"))
    }

    async fn get_capacity(
        &self,
        _request: Request<GetCapacityRequest>,
    ) -> Result<Response<GetCapacityResponse>, Status> {
        Err(Status::unimplemented("GetCapacity not supported"))
    }

    async fn create_snapshot(
        &self,
        _request: Request<CreateSnapshotRequest>,
    ) -> Result<Response<CreateSnapshotResponse>, Status> {
        Err(Status::unimplemented("CreateSnapshot not supported"))
    }

    async fn delete_snapshot(
        &self,
        _request: Request<DeleteSnapshotRequest>,
    ) -> Result<Response<DeleteSnapshotResponse>, Status> {
        Err(Status::unimplemented("DeleteSnapshot not supported"))
    }

    async fn list_snapshots(
        &self,
        _request: Request<ListSnapshotsRequest>,
    ) -> Result<Response<ListSnapshotsResponse>, Status> {
        Err(Status::unimplemented("ListSnapshots not supported"))
    }

    async fn controller_expand_volume(
        &self,
        _request: Request<ControllerExpandVolumeRequest>,
    ) -> Result<Response<ControllerExpandVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "ControllerExpandVolume not supported",
        ))
    }

    async fn controller_get_volume(
        &self,
        _request: Request<ControllerGetVolumeRequest>,
    ) -> Result<Response<ControllerGetVolumeResponse>, Status> {
        Err(Status::unimplemented("ControllerGetVolume not supported"))
    }

    async fn controller_modify_volume(
        &self,
        _request: Request<ControllerModifyVolumeRequest>,
    ) -> Result<Response<ControllerModifyVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "ControllerModifyVolume not supported",
        ))
    }
}
