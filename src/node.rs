use std::path::PathBuf;
use std::sync::Arc;
use tonic::{Request, Response, Status};
use tracing::{error, info, warn};

use crate::csi::{
    node_server::Node, NodeExpandVolumeRequest, NodeExpandVolumeResponse,
    NodeGetCapabilitiesRequest, NodeGetCapabilitiesResponse, NodeGetInfoRequest,
    NodeGetInfoResponse, NodeGetVolumeStatsRequest, NodeGetVolumeStatsResponse,
    NodePublishVolumeRequest, NodePublishVolumeResponse, NodeServiceCapability,
    NodeStageVolumeRequest, NodeStageVolumeResponse, NodeUnpublishVolumeRequest,
    NodeUnpublishVolumeResponse, NodeUnstageVolumeRequest, NodeUnstageVolumeResponse,
};

use crate::cleanup;
use crate::volume;

/// Optional cleanup registration context
pub struct CleanupContext {
    pub client: kube::Client,
    pub namespace: String,
}

pub struct NodeService {
    node_name: String,
    base_path: PathBuf,
    cleanup_ctx: Option<Arc<CleanupContext>>,
}

impl NodeService {
    pub fn new(node_name: String, base_path: PathBuf) -> Self {
        Self {
            node_name,
            base_path,
            cleanup_ctx: None,
        }
    }

    pub fn with_cleanup(mut self, client: kube::Client, namespace: String) -> Self {
        self.cleanup_ctx = Some(Arc::new(CleanupContext { client, namespace }));
        self
    }
}

#[tonic::async_trait]
impl Node for NodeService {
    async fn node_publish_volume(
        &self,
        request: Request<NodePublishVolumeRequest>,
    ) -> Result<Response<NodePublishVolumeResponse>, Status> {
        let req = request.into_inner();
        let volume_id = &req.volume_id;
        let target_path = PathBuf::from(&req.target_path);
        let readonly = req.readonly;

        info!(
            volume_id = %volume_id,
            target_path = %target_path.display(),
            readonly = readonly,
            "NodePublishVolume called"
        );

        // Validate volume ID
        if !volume::validate_volume_id(volume_id) {
            return Err(Status::invalid_argument(format!(
                "Invalid volume ID: {}",
                volume_id
            )));
        }

        // Construct source path
        let source_path = volume::volume_path(&self.base_path, volume_id);

        // Create source directory if it doesn't exist
        if let Err(e) = std::fs::create_dir_all(&source_path) {
            error!(path = %source_path.display(), error = %e, "Failed to create source directory");
            return Err(Status::internal(format!(
                "Failed to create volume directory: {}",
                e
            )));
        }

        // Create target directory parent if needed
        if let Some(parent) = target_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                error!(path = %parent.display(), error = %e, "Failed to create target parent directory");
                return Err(Status::internal(format!(
                    "Failed to create target parent directory: {}",
                    e
                )));
            }
        }

        // Create target mount point (directory for volume mount)
        if !target_path.exists() {
            if let Err(e) = std::fs::create_dir_all(&target_path) {
                error!(path = %target_path.display(), error = %e, "Failed to create target directory");
                return Err(Status::internal(format!(
                    "Failed to create target directory: {}",
                    e
                )));
            }
        }

        // Check if already mounted
        if volume::is_mounted(&target_path)? {
            info!(target_path = %target_path.display(), "Already mounted, skipping");
            return Ok(Response::new(NodePublishVolumeResponse {}));
        }

        // Perform bind mount
        let mount_flags = if readonly {
            nix::mount::MsFlags::MS_BIND | nix::mount::MsFlags::MS_RDONLY
        } else {
            nix::mount::MsFlags::MS_BIND
        };

        if let Err(e) = nix::mount::mount(
            Some(&source_path),
            &target_path,
            None::<&str>,
            mount_flags,
            None::<&str>,
        ) {
            error!(
                source = %source_path.display(),
                target = %target_path.display(),
                error = %e,
                "Failed to bind mount"
            );
            return Err(Status::internal(format!("Failed to bind mount: {}", e)));
        }

        // For readonly, we need to remount with readonly flag
        if readonly {
            let remount_flags = nix::mount::MsFlags::MS_BIND
                | nix::mount::MsFlags::MS_REMOUNT
                | nix::mount::MsFlags::MS_RDONLY;

            if let Err(e) = nix::mount::mount(
                None::<&str>,
                &target_path,
                None::<&str>,
                remount_flags,
                None::<&str>,
            ) {
                warn!(error = %e, "Failed to remount readonly, continuing anyway");
            }
        }

        info!(
            source = %source_path.display(),
            target = %target_path.display(),
            "Volume mounted successfully"
        );

        // Register this node as having the volume for cleanup tracking
        if let Some(ctx) = &self.cleanup_ctx {
            if let Err(e) = cleanup::register_node_publish(
                &ctx.client,
                &ctx.namespace,
                volume_id,
                &self.node_name,
            )
            .await
            {
                // Log but don't fail - cleanup tracking is best-effort
                warn!(
                    volume_id = %volume_id,
                    error = %e,
                    "Failed to register node for cleanup tracking"
                );
            }

            // Emit event for visibility
            cleanup::emit_event(
                &ctx.client,
                &ctx.namespace,
                volume_id,
                "VolumePublished",
                &format!(
                    "Volume mounted on node {} at {}",
                    self.node_name,
                    target_path.display()
                ),
                "Normal",
            )
            .await;
        }

        Ok(Response::new(NodePublishVolumeResponse {}))
    }

    async fn node_unpublish_volume(
        &self,
        request: Request<NodeUnpublishVolumeRequest>,
    ) -> Result<Response<NodeUnpublishVolumeResponse>, Status> {
        let req = request.into_inner();
        let volume_id = &req.volume_id;
        let target_path = PathBuf::from(&req.target_path);

        info!(
            volume_id = %volume_id,
            target_path = %target_path.display(),
            "NodeUnpublishVolume called"
        );

        // Check if mounted
        if !volume::is_mounted(&target_path)? {
            info!(target_path = %target_path.display(), "Not mounted, nothing to do");
            return Ok(Response::new(NodeUnpublishVolumeResponse {}));
        }

        // Unmount
        if let Err(e) = nix::mount::umount(&target_path) {
            // Try lazy unmount if regular unmount fails
            warn!(error = %e, "Regular unmount failed, trying lazy unmount");
            if let Err(e) = nix::mount::umount2(&target_path, nix::mount::MntFlags::MNT_DETACH) {
                error!(error = %e, "Lazy unmount also failed");
                return Err(Status::internal(format!("Failed to unmount: {}", e)));
            }
        }

        info!(target_path = %target_path.display(), "Volume unmounted successfully");

        Ok(Response::new(NodeUnpublishVolumeResponse {}))
    }

    async fn node_get_capabilities(
        &self,
        _request: Request<NodeGetCapabilitiesRequest>,
    ) -> Result<Response<NodeGetCapabilitiesResponse>, Status> {
        info!("NodeGetCapabilities called");

        // We don't need staging - return empty capabilities
        let capabilities: Vec<NodeServiceCapability> = vec![];

        Ok(Response::new(NodeGetCapabilitiesResponse { capabilities }))
    }

    async fn node_get_info(
        &self,
        _request: Request<NodeGetInfoRequest>,
    ) -> Result<Response<NodeGetInfoResponse>, Status> {
        info!(node_name = %self.node_name, "NodeGetInfo called");

        Ok(Response::new(NodeGetInfoResponse {
            node_id: self.node_name.clone(),
            max_volumes_per_node: 0, // No limit
            // No topology - volumes accessible from any node
            accessible_topology: None,
        }))
    }

    // Staging not implemented - not needed for bind mounts

    async fn node_stage_volume(
        &self,
        _request: Request<NodeStageVolumeRequest>,
    ) -> Result<Response<NodeStageVolumeResponse>, Status> {
        Err(Status::unimplemented("NodeStageVolume not supported"))
    }

    async fn node_unstage_volume(
        &self,
        _request: Request<NodeUnstageVolumeRequest>,
    ) -> Result<Response<NodeUnstageVolumeResponse>, Status> {
        Err(Status::unimplemented("NodeUnstageVolume not supported"))
    }

    async fn node_get_volume_stats(
        &self,
        _request: Request<NodeGetVolumeStatsRequest>,
    ) -> Result<Response<NodeGetVolumeStatsResponse>, Status> {
        Err(Status::unimplemented("NodeGetVolumeStats not supported"))
    }

    async fn node_expand_volume(
        &self,
        _request: Request<NodeExpandVolumeRequest>,
    ) -> Result<Response<NodeExpandVolumeResponse>, Status> {
        Err(Status::unimplemented("NodeExpandVolume not supported"))
    }
}
