//! Cleanup coordination for volume deletion.
//!
//! Volume lifecycle:
//! 1. NodePublishVolume → ConfigMap created/updated with node in `nodes_with_volume`
//! 2. DeleteVolume → ConfigMap marked as cleanup pending
//! 3. Node plugins watch for cleanup ConfigMaps
//! 4. Each node deletes its local directory and reports in `nodes_completed`
//! 5. Controller prunes ConfigMap when all nodes complete (or after timeout)

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use rand::Rng;

use k8s_openapi::api::core::v1::{ConfigMap, Event, Node, ObjectReference};
use kube::{
    api::{Api, ListParams, PostParams},
    Client,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

/// Label key for volume ConfigMaps
pub const VOLUME_LABEL: &str = "node-local-cache.csi.io/volume";
/// ConfigMap name prefix
pub const VOLUME_CM_PREFIX: &str = "nlc-vol-";

/// Maximum retries for optimistic concurrency conflicts
/// High value to handle gang scheduling scenarios where many pods start simultaneously
const MAX_RETRIES: u32 = 15;

/// Base backoff delay in milliseconds for optimistic concurrency retries
const BASE_BACKOFF_MS: u64 = 10;
/// Maximum backoff delay in milliseconds
const MAX_BACKOFF_MS: u64 = 1000;

/// Sleep with exponential backoff and jitter to avoid thundering herd
async fn backoff_sleep(attempt: u32) {
    let base = BASE_BACKOFF_MS * 2u64.pow(attempt.min(6)); // cap exponent to avoid overflow
    let max = base.min(MAX_BACKOFF_MS);
    let jitter = rand::rng().random_range(0..=max);
    tokio::time::sleep(Duration::from_millis(jitter)).await;
}

/// Volume status stored in ConfigMap data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeStatus {
    pub volume_id: String,
    pub created_at: String,
    #[serde(default)]
    pub cleanup_requested_at: Option<String>,
    #[serde(default)]
    pub nodes_with_volume: Vec<String>,
    #[serde(default)]
    pub nodes_completed: Vec<String>,
    #[serde(default)]
    pub nodes_failed: Vec<String>,
    /// Nodes that no longer exist in the cluster (scaled down, decommissioned)
    #[serde(default)]
    pub nodes_decommissioned: Vec<String>,
}

impl VolumeStatus {
    pub fn new(volume_id: &str) -> Self {
        Self {
            volume_id: volume_id.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            cleanup_requested_at: None,
            nodes_with_volume: Vec::new(),
            nodes_completed: Vec::new(),
            nodes_failed: Vec::new(),
            nodes_decommissioned: Vec::new(),
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

    pub fn add_node(&mut self, node_name: &str) {
        if !self.nodes_with_volume.contains(&node_name.to_string()) {
            self.nodes_with_volume.push(node_name.to_string());
        }
    }

    pub fn mark_cleanup_requested(&mut self) {
        if self.cleanup_requested_at.is_none() {
            self.cleanup_requested_at = Some(chrono::Utc::now().to_rfc3339());
        }
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

    pub fn mark_node_decommissioned(&mut self, node_name: &str) {
        if !self.nodes_decommissioned.contains(&node_name.to_string()) {
            self.nodes_decommissioned.push(node_name.to_string());
        }
    }

    /// Check if cleanup is complete (all nodes with volume have reported or are gone)
    pub fn is_cleanup_complete(&self) -> bool {
        if self.cleanup_requested_at.is_none() {
            return false;
        }
        let nodes_with: HashSet<_> = self.nodes_with_volume.iter().collect();
        let nodes_done: HashSet<_> = self
            .nodes_completed
            .iter()
            .chain(self.nodes_failed.iter())
            .chain(self.nodes_decommissioned.iter())
            .collect();
        nodes_with.is_subset(&nodes_done)
    }

    /// Get nodes that haven't reported yet (not completed, failed, or decommissioned)
    pub fn pending_nodes(&self) -> Vec<&String> {
        self.nodes_with_volume
            .iter()
            .filter(|n| {
                !self.nodes_completed.contains(n)
                    && !self.nodes_failed.contains(n)
                    && !self.nodes_decommissioned.contains(n)
            })
            .collect()
    }
}

fn configmap_name(volume_id: &str) -> String {
    format!("{}{}", VOLUME_CM_PREFIX, volume_id)
}

/// Emit a Kubernetes event for visibility
/// Events show up in `kubectl get events` and `kubectl describe`
pub async fn emit_event(
    client: &Client,
    namespace: &str,
    volume_id: &str,
    reason: &str,
    message: &str,
    event_type: &str, // "Normal" or "Warning"
) {
    let events: Api<Event> = Api::namespaced(client.clone(), namespace);
    let cm_name = configmap_name(volume_id);

    let event = Event {
        metadata: kube::api::ObjectMeta {
            generate_name: Some("nlc-".to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        involved_object: ObjectReference {
            api_version: Some("v1".to_string()),
            kind: Some("ConfigMap".to_string()),
            name: Some(cm_name),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        reason: Some(reason.to_string()),
        message: Some(message.to_string()),
        type_: Some(event_type.to_string()),
        first_timestamp: Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
            chrono::Utc::now(),
        )),
        ..Default::default()
    };

    if let Err(e) = events.create(&PostParams::default(), &event).await {
        warn!(reason = %reason, error = %e, "Failed to emit event");
    }
}

/// Register that a node has published a volume (call from NodePublishVolume)
/// Uses optimistic concurrency - retries on conflict
pub async fn register_node_publish(
    client: &Client,
    namespace: &str,
    volume_id: &str,
    node_name: &str,
) -> Result<(), kube::Error> {
    let configmaps: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
    let cm_name = configmap_name(volume_id);

    for attempt in 0..MAX_RETRIES {
        // Try to get existing ConfigMap
        let result = configmaps.get(&cm_name).await;

        match result {
            Ok(existing) => {
                // Update existing ConfigMap
                let resource_version = existing.metadata.resource_version.clone();
                let mut status = VolumeStatus::from_configmap(&existing)
                    .unwrap_or_else(|| VolumeStatus::new(volume_id));
                status.add_node(node_name);

                let patch = ConfigMap {
                    metadata: kube::api::ObjectMeta {
                        name: Some(cm_name.clone()),
                        namespace: Some(namespace.to_string()),
                        resource_version,
                        labels: Some(BTreeMap::from([(
                            VOLUME_LABEL.to_string(),
                            "active".to_string(),
                        )])),
                        ..Default::default()
                    },
                    data: Some(status.to_configmap_data()),
                    ..Default::default()
                };

                match configmaps
                    .replace(&cm_name, &PostParams::default(), &patch)
                    .await
                {
                    Ok(_) => {
                        debug!(
                            volume_id = %volume_id,
                            node = %node_name,
                            "Registered node for volume"
                        );
                        return Ok(());
                    }
                    Err(kube::Error::Api(ref err)) if err.code == 409 => {
                        debug!(attempt = attempt, "Conflict updating ConfigMap, retrying");
                        backoff_sleep(attempt).await;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(kube::Error::Api(ref err)) if err.code == 404 => {
                // Create new ConfigMap
                let mut status = VolumeStatus::new(volume_id);
                status.add_node(node_name);

                let cm = ConfigMap {
                    metadata: kube::api::ObjectMeta {
                        name: Some(cm_name.clone()),
                        namespace: Some(namespace.to_string()),
                        labels: Some(BTreeMap::from([(
                            VOLUME_LABEL.to_string(),
                            "active".to_string(),
                        )])),
                        ..Default::default()
                    },
                    data: Some(status.to_configmap_data()),
                    ..Default::default()
                };

                match configmaps.create(&PostParams::default(), &cm).await {
                    Ok(_) => {
                        info!(
                            volume_id = %volume_id,
                            node = %node_name,
                            "Created volume tracking ConfigMap"
                        );
                        return Ok(());
                    }
                    Err(kube::Error::Api(ref err)) if err.code == 409 => {
                        // Someone else created it, retry to update
                        debug!("ConfigMap created by another, retrying");
                        backoff_sleep(attempt).await;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }

    Err(kube::Error::Api(kube::core::ErrorResponse {
        status: "Failure".to_string(),
        message: "Max retries exceeded for optimistic concurrency".to_string(),
        reason: "Conflict".to_string(),
        code: 409,
    }))
}

/// Mark a volume for cleanup (call from DeleteVolume)
/// Uses optimistic concurrency - retries on conflict
pub async fn mark_volume_for_cleanup(
    client: &Client,
    namespace: &str,
    volume_id: &str,
) -> Result<(), kube::Error> {
    let configmaps: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
    let cm_name = configmap_name(volume_id);

    for attempt in 0..MAX_RETRIES {
        let result = configmaps.get(&cm_name).await;

        match result {
            Ok(existing) => {
                let resource_version = existing.metadata.resource_version.clone();
                let mut status = VolumeStatus::from_configmap(&existing)
                    .unwrap_or_else(|| VolumeStatus::new(volume_id));
                status.mark_cleanup_requested();

                let patch = ConfigMap {
                    metadata: kube::api::ObjectMeta {
                        name: Some(cm_name.clone()),
                        namespace: Some(namespace.to_string()),
                        resource_version,
                        labels: Some(BTreeMap::from([(
                            VOLUME_LABEL.to_string(),
                            "cleanup".to_string(),
                        )])),
                        ..Default::default()
                    },
                    data: Some(status.to_configmap_data()),
                    ..Default::default()
                };

                match configmaps
                    .replace(&cm_name, &PostParams::default(), &patch)
                    .await
                {
                    Ok(_) => {
                        info!(
                            volume_id = %volume_id,
                            nodes_to_cleanup = status.nodes_with_volume.len(),
                            "Marked volume for cleanup"
                        );
                        emit_event(
                            client,
                            namespace,
                            volume_id,
                            "CleanupRequested",
                            &format!(
                                "Volume cleanup requested, {} node(s) to clean: {:?}",
                                status.nodes_with_volume.len(),
                                status.nodes_with_volume
                            ),
                            "Normal",
                        )
                        .await;
                        return Ok(());
                    }
                    Err(kube::Error::Api(ref err)) if err.code == 409 => {
                        debug!(attempt = attempt, "Conflict, retrying");
                        backoff_sleep(attempt).await;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(kube::Error::Api(ref err)) if err.code == 404 => {
                // Volume was never published, nothing to clean up
                debug!(volume_id = %volume_id, "No tracking ConfigMap, nothing to clean");
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    }

    Err(kube::Error::Api(kube::core::ErrorResponse {
        status: "Failure".to_string(),
        message: "Max retries exceeded".to_string(),
        reason: "Conflict".to_string(),
        code: 409,
    }))
}

/// Mark node cleanup complete with optimistic concurrency
async fn mark_node_cleanup_complete(
    client: &Client,
    namespace: &str,
    configmaps: &Api<ConfigMap>,
    cm_name: &str,
    node_name: &str,
    success: bool,
) -> Result<(), kube::Error> {
    for attempt in 0..MAX_RETRIES {
        let existing = configmaps.get(cm_name).await?;
        let resource_version = existing.metadata.resource_version.clone();
        let labels = existing.metadata.labels.clone();

        let mut status = match VolumeStatus::from_configmap(&existing) {
            Some(s) => s,
            None => {
                warn!(configmap = %cm_name, "Invalid status in ConfigMap");
                return Ok(());
            }
        };

        let volume_id = status.volume_id.clone();

        if success {
            status.mark_node_completed(node_name);
        } else {
            status.mark_node_failed(node_name);
        }

        let patch = ConfigMap {
            metadata: kube::api::ObjectMeta {
                name: Some(cm_name.to_string()),
                resource_version,
                labels,
                ..Default::default()
            },
            data: Some(status.to_configmap_data()),
            ..Default::default()
        };

        match configmaps
            .replace(cm_name, &PostParams::default(), &patch)
            .await
        {
            Ok(_) => {
                let (reason, msg, event_type) = if success {
                    (
                        "NodeCleanupComplete",
                        format!("Node {} completed cleanup", node_name),
                        "Normal",
                    )
                } else {
                    (
                        "NodeCleanupFailed",
                        format!("Node {} failed cleanup", node_name),
                        "Warning",
                    )
                };
                emit_event(client, namespace, &volume_id, reason, &msg, event_type).await;
                return Ok(());
            }
            Err(kube::Error::Api(ref err)) if err.code == 409 => {
                debug!(
                    attempt = attempt,
                    "Conflict updating cleanup status, retrying"
                );
                backoff_sleep(attempt).await;
                continue;
            }
            Err(e) => return Err(e),
        }
    }

    warn!(configmap = %cm_name, "Max retries exceeded updating cleanup status");
    Ok(()) // Don't fail the cleanup for status update issues
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

    /// Create a cleanup request for a volume (legacy method, calls mark_volume_for_cleanup)
    pub async fn create_cleanup_request(&self, volume_id: &str) -> Result<(), kube::Error> {
        mark_volume_for_cleanup(&self.client, &self.namespace, volume_id).await
    }

    /// Get set of node names that exist in the cluster
    async fn get_existing_nodes(&self) -> Result<HashSet<String>, kube::Error> {
        let nodes: Api<Node> = Api::all(self.client.clone());
        let node_list = nodes.list(&ListParams::default()).await?;
        let names: HashSet<String> = node_list
            .items
            .iter()
            .filter_map(|n| n.metadata.name.clone())
            .collect();
        Ok(names)
    }

    /// Check for decommissioned nodes and update ConfigMap
    /// Returns true if any nodes were marked as decommissioned
    async fn mark_decommissioned_nodes(
        &self,
        configmaps: &Api<ConfigMap>,
        cm: &ConfigMap,
        status: &VolumeStatus,
        existing_nodes: &HashSet<String>,
    ) -> Result<bool, kube::Error> {
        let pending = status.pending_nodes();
        let decommissioned: Vec<_> = pending
            .iter()
            .filter(|n| !existing_nodes.contains(**n))
            .map(|n| (*n).clone())
            .collect();

        if decommissioned.is_empty() {
            return Ok(false);
        }

        let cm_name = match cm.metadata.name.as_ref() {
            Some(n) => n,
            None => return Ok(false),
        };

        // Update with optimistic concurrency
        for attempt in 0..MAX_RETRIES {
            let existing = configmaps.get(cm_name).await?;
            let resource_version = existing.metadata.resource_version.clone();
            let labels = existing.metadata.labels.clone();

            let mut updated_status = match VolumeStatus::from_configmap(&existing) {
                Some(s) => s,
                None => return Ok(false),
            };

            for node in &decommissioned {
                updated_status.mark_node_decommissioned(node);
            }

            let patch = ConfigMap {
                metadata: kube::api::ObjectMeta {
                    name: Some(cm_name.to_string()),
                    resource_version,
                    labels,
                    ..Default::default()
                },
                data: Some(updated_status.to_configmap_data()),
                ..Default::default()
            };

            match configmaps
                .replace(cm_name, &PostParams::default(), &patch)
                .await
            {
                Ok(_) => {
                    info!(
                        configmap = %cm_name,
                        volume_id = %status.volume_id,
                        decommissioned_nodes = ?decommissioned,
                        "Marked nodes as decommissioned (no longer exist in cluster)"
                    );
                    emit_event(
                        &self.client,
                        &self.namespace,
                        &status.volume_id,
                        "NodeDecommissioned",
                        &format!(
                            "Node(s) no longer exist in cluster, marked as decommissioned: {:?}",
                            decommissioned
                        ),
                        "Warning",
                    )
                    .await;
                    return Ok(true);
                }
                Err(kube::Error::Api(ref err)) if err.code == 409 => {
                    debug!(
                        attempt = attempt,
                        "Conflict marking decommissioned, retrying"
                    );
                    backoff_sleep(attempt).await;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }

        warn!(configmap = %cm_name, "Max retries exceeded marking decommissioned nodes");
        Ok(false)
    }

    /// Process cleanup ConfigMaps: mark decommissioned nodes and prune completed ones
    pub async fn process_cleanups(&self) -> Result<usize, kube::Error> {
        let configmaps: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let lp = ListParams::default().labels(&format!("{}=cleanup", VOLUME_LABEL));

        let cms = configmaps.list(&lp).await?;

        if cms.items.is_empty() {
            return Ok(0);
        }

        // Get existing nodes once for all ConfigMaps
        let existing_nodes = self.get_existing_nodes().await?;
        debug!(node_count = existing_nodes.len(), "Fetched cluster nodes");

        let mut pruned = 0;

        for cm in cms.items {
            let cm_name = match cm.metadata.name.as_ref() {
                Some(n) => n,
                None => continue,
            };

            let status = match VolumeStatus::from_configmap(&cm) {
                Some(s) => s,
                None => continue,
            };

            // First, check for decommissioned nodes
            if !status.pending_nodes().is_empty() {
                if let Err(e) = self
                    .mark_decommissioned_nodes(&configmaps, &cm, &status, &existing_nodes)
                    .await
                {
                    warn!(
                        configmap = %cm_name,
                        error = %e,
                        "Failed to mark decommissioned nodes"
                    );
                }
            }

            // Re-fetch to get updated status after potential decommissioning
            let current_status = match configmaps.get(cm_name).await {
                Ok(updated_cm) => VolumeStatus::from_configmap(&updated_cm).unwrap_or(status),
                Err(_) => continue, // ConfigMap may have been deleted
            };

            // Prune if complete
            if current_status.is_cleanup_complete() {
                // Emit event before deleting the ConfigMap
                emit_event(
                    &self.client,
                    &self.namespace,
                    &current_status.volume_id,
                    "CleanupComplete",
                    &format!(
                        "All cleanup complete. Completed: {:?}, Failed: {:?}, Decommissioned: {:?}",
                        current_status.nodes_completed,
                        current_status.nodes_failed,
                        current_status.nodes_decommissioned
                    ),
                    "Normal",
                )
                .await;

                match configmaps.delete(cm_name, &Default::default()).await {
                    Ok(_) => {
                        info!(
                            configmap = %cm_name,
                            volume_id = %current_status.volume_id,
                            nodes_with_volume = ?current_status.nodes_with_volume,
                            nodes_completed = ?current_status.nodes_completed,
                            nodes_failed = ?current_status.nodes_failed,
                            nodes_decommissioned = ?current_status.nodes_decommissioned,
                            "Pruned completed cleanup ConfigMap"
                        );
                        pruned += 1;
                    }
                    Err(e) => {
                        warn!(configmap = %cm_name, error = %e, "Failed to prune ConfigMap");
                    }
                }
            }
        }

        Ok(pruned)
    }
}

/// Run the controller cleanup processing loop
/// Checks for decommissioned nodes and prunes completed ConfigMaps
pub async fn run_controller_cleanup_loop(client: Client, namespace: String, interval: Duration) {
    info!(
        interval_secs = interval.as_secs(),
        "Starting controller cleanup processor"
    );

    let controller = CleanupController::new(client, namespace);

    loop {
        tokio::time::sleep(interval).await;

        match controller.process_cleanups().await {
            Ok(count) if count > 0 => {
                info!(count = count, "Pruned cleanup ConfigMaps");
            }
            Ok(_) => {
                debug!("No cleanup ConfigMaps to prune");
            }
            Err(e) => {
                error!(error = %e, "Error processing cleanups");
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
        let lp = ListParams::default().labels(&format!("{}=cleanup", VOLUME_LABEL));

        let cms = configmaps.list(&lp).await?;
        let mut processed = 0;

        for cm in cms.items {
            let cm_name = match cm.metadata.name.as_ref() {
                Some(n) => n.clone(),
                None => continue,
            };

            let status = match VolumeStatus::from_configmap(&cm) {
                Some(s) => s,
                None => continue,
            };

            // Skip if this node doesn't have the volume
            if !status.nodes_with_volume.contains(&self.node_name) {
                continue;
            }

            // Skip if we already processed this
            if status.nodes_completed.contains(&self.node_name)
                || status.nodes_failed.contains(&self.node_name)
            {
                continue;
            }

            // Process cleanup
            let volume_path = self.base_path.join(&status.volume_id);
            let result = self.cleanup_volume_directory(&volume_path).await;

            let success = match result {
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
                            "No directory to clean (already gone)"
                        );
                    }
                    true
                }
                Err(e) => {
                    error!(
                        volume_id = %status.volume_id,
                        node = %self.node_name,
                        error = %e,
                        "Failed to clean up volume directory"
                    );
                    false
                }
            };

            // Update ConfigMap with completion status
            if let Err(e) = mark_node_cleanup_complete(
                &self.client,
                &self.namespace,
                &configmaps,
                &cm_name,
                &self.node_name,
                success,
            )
            .await
            {
                warn!(
                    configmap = %cm_name,
                    error = %e,
                    "Failed to update cleanup status"
                );
            }

            processed += 1;
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

        // Use tokio's blocking task for potentially long rm -rf
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || std::fs::remove_dir_all(path))
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))??;

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
    fn test_volume_status_serialization() {
        let mut status = VolumeStatus::new("nlc-test-123");
        status.add_node("node1");
        status.add_node("node2");
        status.mark_cleanup_requested();
        status.mark_node_completed("node1");
        status.mark_node_decommissioned("node3");

        let data = status.to_configmap_data();
        let json = data.get("status").unwrap();

        let parsed: VolumeStatus = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.volume_id, "nlc-test-123");
        assert_eq!(parsed.nodes_with_volume.len(), 2);
        assert_eq!(parsed.nodes_completed.len(), 1);
        assert_eq!(parsed.nodes_decommissioned.len(), 1);
        assert!(parsed.cleanup_requested_at.is_some());
    }

    #[test]
    fn test_cleanup_complete() {
        let mut status = VolumeStatus::new("nlc-test-123");
        status.add_node("node1");
        status.add_node("node2");

        // Not complete without cleanup request
        assert!(!status.is_cleanup_complete());

        status.mark_cleanup_requested();

        // Not complete without all nodes reporting
        assert!(!status.is_cleanup_complete());

        status.mark_node_completed("node1");
        assert!(!status.is_cleanup_complete());

        status.mark_node_completed("node2");
        assert!(status.is_cleanup_complete());
    }

    #[test]
    fn test_cleanup_complete_with_failures() {
        let mut status = VolumeStatus::new("nlc-test-123");
        status.add_node("node1");
        status.add_node("node2");
        status.mark_cleanup_requested();

        status.mark_node_completed("node1");
        status.mark_node_failed("node2"); // Failed but still "reported"

        assert!(status.is_cleanup_complete());
    }

    #[test]
    fn test_idempotent_operations() {
        let mut status = VolumeStatus::new("nlc-test-123");

        status.add_node("node1");
        status.add_node("node1");
        status.add_node("node1");
        assert_eq!(status.nodes_with_volume.len(), 1);

        status.mark_node_completed("node1");
        status.mark_node_completed("node1");
        assert_eq!(status.nodes_completed.len(), 1);
    }
}
