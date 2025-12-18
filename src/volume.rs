use std::path::{Path, PathBuf};
use tonic::Status;
use uuid::Uuid;

/// Volume ID prefix
const VOLUME_ID_PREFIX: &str = "nlc-";

/// Namespace UUID for generating deterministic volume IDs (UUIDv5)
/// Generated specifically for this driver: uuidgen output for "node-local-cache.csi.io"
const VOLUME_ID_NAMESPACE: Uuid = Uuid::from_bytes([
    0x7a, 0x3e, 0x8f, 0x2b, 0x5c, 0x41, 0x4d, 0x9a, 0xb8, 0x6f, 0x1e, 0x4a, 0x9c, 0x2d, 0x7b, 0x5e,
]);

/// Generate a deterministic volume ID from a PVC name
/// Uses UUIDv5 to ensure idempotency - same name always produces same ID
pub fn generate_volume_id(name: &str) -> String {
    let uuid = Uuid::new_v5(&VOLUME_ID_NAMESPACE, name.as_bytes());
    format!("{}{}", VOLUME_ID_PREFIX, uuid)
}

/// Validate a volume ID format
pub fn validate_volume_id(id: &str) -> bool {
    if !id.starts_with(VOLUME_ID_PREFIX) {
        return false;
    }

    let uuid_part = &id[VOLUME_ID_PREFIX.len()..];
    Uuid::parse_str(uuid_part).is_ok()
}

/// Construct the volume directory path
pub fn volume_path(base: &Path, volume_id: &str) -> PathBuf {
    base.join(volume_id)
}

/// Check if a path is a mount point by reading /proc/mounts
#[allow(clippy::result_large_err)]
pub fn is_mounted(path: &Path) -> Result<bool, Status> {
    let mounts = std::fs::read_to_string("/proc/mounts")
        .map_err(|e| Status::internal(format!("Failed to read /proc/mounts: {}", e)))?;

    let path_str = path.to_string_lossy();

    for line in mounts.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[1] == path_str {
            return Ok(true);
        }
    }

    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_volume_id() {
        let id = generate_volume_id("pvc-12345");
        assert!(id.starts_with(VOLUME_ID_PREFIX));
        assert_eq!(id.len(), 4 + 36); // "nlc-" + UUID
    }

    #[test]
    fn test_generate_volume_id_deterministic() {
        // Same input should produce same output (idempotency)
        let id1 = generate_volume_id("pvc-abc-123");
        let id2 = generate_volume_id("pvc-abc-123");
        assert_eq!(id1, id2);

        // Different input should produce different output
        let id3 = generate_volume_id("pvc-def-456");
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_validate_volume_id() {
        // Valid IDs
        assert!(validate_volume_id(
            "nlc-550e8400-e29b-41d4-a716-446655440000"
        ));
        assert!(validate_volume_id(&generate_volume_id("test-pvc")));

        // Invalid IDs
        assert!(!validate_volume_id("invalid"));
        assert!(!validate_volume_id(
            "cv-550e8400-e29b-41d4-a716-446655440000"
        )); // wrong prefix
        assert!(!validate_volume_id("nlc-not-a-uuid"));
        assert!(!validate_volume_id(""));
    }

    #[test]
    fn test_volume_path() {
        let base = Path::new("/var/node-local-cache");
        let id = "nlc-550e8400-e29b-41d4-a716-446655440000";
        let path = volume_path(base, id);
        assert_eq!(
            path,
            PathBuf::from("/var/node-local-cache/nlc-550e8400-e29b-41d4-a716-446655440000")
        );
    }
}
