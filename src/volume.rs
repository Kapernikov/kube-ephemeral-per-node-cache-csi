use std::path::{Path, PathBuf};
use tonic::Status;
use uuid::Uuid;

/// Volume ID prefix
const VOLUME_ID_PREFIX: &str = "nlc-";

/// Generate a new volume ID
pub fn generate_volume_id() -> String {
    format!("{}{}", VOLUME_ID_PREFIX, Uuid::new_v4())
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
pub fn is_mounted(path: &Path) -> Result<bool, Status> {
    let mounts = std::fs::read_to_string("/proc/mounts").map_err(|e| {
        Status::internal(format!("Failed to read /proc/mounts: {}", e))
    })?;

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
        let id = generate_volume_id();
        assert!(id.starts_with(VOLUME_ID_PREFIX));
        assert_eq!(id.len(), 4 + 36); // "nlc-" + UUID
    }

    #[test]
    fn test_validate_volume_id() {
        // Valid IDs
        assert!(validate_volume_id("nlc-550e8400-e29b-41d4-a716-446655440000"));
        assert!(validate_volume_id(&generate_volume_id()));

        // Invalid IDs
        assert!(!validate_volume_id("invalid"));
        assert!(!validate_volume_id("cv-550e8400-e29b-41d4-a716-446655440000")); // wrong prefix
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
