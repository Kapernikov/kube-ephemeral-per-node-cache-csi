#![allow(dead_code)] // Some methods used in future phases

//! Cleanup coordination for volume deletion.
//!
//! When a volume is deleted:
//! 1. Controller creates a cleanup ConfigMap
//! 2. Node plugins watch for cleanup ConfigMaps
//! 3. Each node deletes its local directory and reports completion
//! 4. Controller removes finalizer after all nodes report (or timeout)

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use k8s_openapi::api::core::v1::ConfigMap;
use kube::{
    api::{Api, ListParams, Patch, PatchParams, PostParams},
    Client,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

/// Label for cleanup ConfigMaps
pub const CLEANUP_LABEL: &str = "node-local-cache.csi.io/cleanup";
/// Finalizer name for PVs
pub const CLEANUP_FINALIZER: &str = "node-local-cache.csi.io/cleanup";
/// ConfigMap name prefix
pub const CLEANUP_PREFIX: &str = "nlc-cleanup-";

/// Cleanup status stored in ConfigMap data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupStatus {
    pub volume_id: String,
    pub created_at: String,
    pub nodes_completed: Vec<String>,
    pub nodes_failed: Vec<String>,
}

impl CleanupStatus {
    pub fn new(volume_id: &str) -> Self {
        Self {
            volume_id: volume_id.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            nodes_completed: Vec::new(),
            nodes_failed: Vec::new(),
        }
    }

    pub fn from_configmap(cm: &ConfigMap) -> Option<Self> {
        let data = cm.data.as_ref()?;
        let status_json = data.get("status")?;
        serde_json::from_str(status_json).ok()
    }

    pub fn to_configmap_data(&self) -> BTreeMap<String, String> {
        let mut data = BTreeMap::new();
        data.insert(
            "status".to_string(),
            serde_json::to_string(self).unwrap_or_default(),
        );
        data
    }

    pub fn mark_node_completed(&mut self, node_name: &str) {
        if !self.nodes_completed.contains(&node_name.to_string()) {
            self.nodes_completed.push(node_name.to_string());
        }
    }

    pub fn mark_node_failed(&mut self, node_name: &str) {
        if !self.nodes_failed.contains(&node_name.to_string()) {
            self.nodes_failed.push(node_name.to_string());
        }
    }
}

/// Controller-side cleanup operations
pub struct CleanupController {
    client: Client,
    namespace: String,
}

impl CleanupController {
    pub fn new(client: Client, namespace: String) -> Self {
        Self { client, namespace }
    }

    /// Get a clone of the kube client
    pub fn client(&self) -> Client {
        self.client.clone()
    }

    /// Get the namespace
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Create a cleanup request for a volume
    pub async fn create_cleanup_request(&self, volume_id: &str) -> Result<(), kube::Error> {
        let configmaps: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);

        let cm_name = format!("{}{}", CLEANUP_PREFIX, volume_id);
        let status = CleanupStatus::new(volume_id);

        let cm = ConfigMap {
            metadata: kube::api::ObjectMeta {
                name: Some(cm_name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some(BTreeMap::from([(
                    CLEANUP_LABEL.to_string(),
                    "pending".to_string(),
                )])),
                ..Default::default()
            },
            data: Some(status.to_configmap_data()),
            ..Default::default()
        };

        match configmaps.create(&PostParams::default(), &cm).await {
            Ok(_) => {
                info!(volume_id = %volume_id, configmap = %cm_name, "Created cleanup request");
                Ok(())
            }
            Err(kube::Error::Api(err)) if err.code == 409 => {
                // Already exists, that's fine
                debug!(volume_id = %volume_id, "Cleanup request already exists");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Delete a cleanup request after completion
    pub async fn delete_cleanup_request(&self, volume_id: &str) -> Result<(), kube::Error> {
        let configmaps: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let cm_name = format!("{}{}", CLEANUP_PREFIX, volume_id);

        match configmaps.delete(&cm_name, &Default::default()).await {
            Ok(_) => {
                info!(volume_id = %volume_id, "Deleted cleanup request");
                Ok(())
            }
            Err(kube::Error::Api(err)) if err.code == 404 => {
                // Already deleted, that's fine
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// List all pending cleanup requests
    pub async fn list_pending_cleanups(&self) -> Result<Vec<CleanupStatus>, kube::Error> {
        let configmaps: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let lp = ListParams::default().labels(&format!("{}=pending", CLEANUP_LABEL));

        let cms = configmaps.list(&lp).await?;
        let statuses: Vec<CleanupStatus> = cms
            .items
            .iter()
            .filter_map(CleanupStatus::from_configmap)
            .collect();

        Ok(statuses)
    }

    /// Prune old cleanup ConfigMaps that have exceeded the TTL
    /// This is called periodically to garbage collect completed or timed-out cleanups
    pub async fn prune_old_cleanups(&self, ttl: Duration) -> Result<usize, kube::Error> {
        let configmaps: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let lp = ListParams::default().labels(CLEANUP_LABEL);

        let cms = configmaps.list(&lp).await?;
        let now = chrono::Utc::now();
        let mut pruned = 0;

        for cm in cms.items {
            if let Some(status) = CleanupStatus::from_configmap(&cm) {
                // Parse created_at timestamp
                if let Ok(created_at) = chrono::DateTime::parse_from_rfc3339(&status.created_at) {
                    let age = now.signed_duration_since(created_at);
                    if age > chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::MAX)
                    {
                        // ConfigMap is older than TTL, delete it
                        if let Some(name) = cm.metadata.name.as_ref() {
                            match configmaps.delete(name, &Default::default()).await {
                                Ok(_) => {
                                    info!(
                                        configmap = %name,
                                        volume_id = %status.volume_id,
                                        age_secs = age.num_seconds(),
                                        nodes_completed = status.nodes_completed.len(),
                                        "Pruned old cleanup ConfigMap"
                                    );
                                    pruned += 1;
                                }
                                Err(e) => {
                                    warn!(
                                        configmap = %name,
                                        error = %e,
                                        "Failed to prune cleanup ConfigMap"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(pruned)
    }
}

/// Run the controller cleanup pruning loop
pub async fn run_controller_prune_loop(client: Client, namespace: String, interval: Duration, ttl: Duration) {
    info!(
        interval_secs = interval.as_secs(),
        ttl_secs = ttl.as_secs(),
        "Starting controller cleanup pruner"
    );

    let controller = CleanupController::new(client, namespace);

    loop {
        tokio::time::sleep(interval).await;

        match controller.prune_old_cleanups(ttl).await {
            Ok(count) if count > 0 => {
                info!(count = count, "Pruned old cleanup ConfigMaps");
            }
            Ok(_) => {
                debug!("No cleanup ConfigMaps to prune");
            }
            Err(e) => {
                error!(error = %e, "Error pruning cleanup ConfigMaps");
            }
        }
    }
}

/// Node-side cleanup operations
pub struct CleanupNode {
    client: Client,
    namespace: String,
    node_name: String,
    base_path: std::path::PathBuf,
}

impl CleanupNode {
    pub fn new(
        client: Client,
        namespace: String,
        node_name: String,
        base_path: std::path::PathBuf,
    ) -> Self {
        Self {
            client,
            namespace,
            node_name,
            base_path,
        }
    }

    /// Process all pending cleanup requests for this node
    pub async fn process_pending_cleanups(&self) -> Result<usize, kube::Error> {
        let configmaps: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let lp = ListParams::default().labels(&format!("{}=pending", CLEANUP_LABEL));

        let cms = configmaps.list(&lp).await?;
        let mut processed = 0;

        for cm in cms.items {
            if let Some(mut status) = CleanupStatus::from_configmap(&cm) {
                // Skip if we already processed this
                if status.nodes_completed.contains(&self.node_name)
                    || status.nodes_failed.contains(&self.node_name)
                {
                    continue;
                }

                // Process cleanup
                let volume_path = self.base_path.join(&status.volume_id);
                let result = self.cleanup_volume_directory(&volume_path).await;

                match result {
                    Ok(cleaned) => {
                        if cleaned {
                            info!(
                                volume_id = %status.volume_id,
                                node = %self.node_name,
                                "Cleaned up volume directory"
                            );
                        } else {
                            debug!(
                                volume_id = %status.volume_id,
                                node = %self.node_name,
                                "No directory to clean"
                            );
                        }
                        status.mark_node_completed(&self.node_name);
                    }
                    Err(e) => {
                        error!(
                            volume_id = %status.volume_id,
                            node = %self.node_name,
                            error = %e,
                            "Failed to clean up volume directory"
                        );
                        status.mark_node_failed(&self.node_name);
                    }
                }

                // Update ConfigMap
                if let Some(name) = cm.metadata.name.as_ref() {
                    let patch = serde_json::json!({
                        "data": status.to_configmap_data()
                    });
                    if let Err(e) = configmaps
                        .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
                        .await
                    {
                        warn!(
                            configmap = %name,
                            error = %e,
                            "Failed to update cleanup status"
                        );
                    }
                }

                processed += 1;
            }
        }

        Ok(processed)
    }

    /// Delete a volume directory if it exists
    async fn cleanup_volume_directory(&self, path: &Path) -> Result<bool, std::io::Error> {
        if !path.exists() {
            return Ok(false);
        }

        // Safety check: ensure path is under base_path
        if !path.starts_with(&self.base_path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Path is not under base path",
            ));
        }

        std::fs::remove_dir_all(path)?;
        Ok(true)
    }

    /// Run the cleanup watcher loop
    pub async fn run_cleanup_loop(self, interval: Duration) {
        info!(
            node = %self.node_name,
            interval_secs = interval.as_secs(),
            "Starting cleanup watcher"
        );

        loop {
            match self.process_pending_cleanups().await {
                Ok(count) if count > 0 => {
                    info!(count = count, "Processed cleanup requests");
                }
                Ok(_) => {
                    debug!("No pending cleanups");
                }
                Err(e) => {
                    error!(error = %e, "Error processing cleanups");
                }
            }

            tokio::time::sleep(interval).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cleanup_status_serialization() {
        let mut status = CleanupStatus::new("nlc-test-123");
        status.mark_node_completed("node1");
        status.mark_node_completed("node2");

        let data = status.to_configmap_data();
        let json = data.get("status").unwrap();

        let parsed: CleanupStatus = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.volume_id, "nlc-test-123");
        assert_eq!(parsed.nodes_completed.len(), 2);
        assert!(parsed.nodes_completed.contains(&"node1".to_string()));
    }

    #[test]
    fn test_cleanup_status_idempotent() {
        let mut status = CleanupStatus::new("nlc-test-123");
        status.mark_node_completed("node1");
        status.mark_node_completed("node1"); // Duplicate
        status.mark_node_completed("node1"); // Duplicate

        assert_eq!(status.nodes_completed.len(), 1);
    }
}
