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
/// Uses proc-mounts crate which handles the simpler /proc/mounts format
/// (more robust than /proc/self/mountinfo parsing in complex container environments)
#[allow(clippy::result_large_err)]
pub fn is_mounted(path: &Path) -> Result<bool, Status> {
    use proc_mounts::MountIter;

    let mounts = MountIter::new()
        .map_err(|e| Status::internal(format!("Failed to read /proc/mounts: {}", e)))?;

    for mount in mounts {
        match mount {
            Ok(info) => {
                if info.dest == path {
                    return Ok(true);
                }
            }
            Err(e) => {
                // Log but continue - one bad line shouldn't fail the whole check
                tracing::warn!("Failed to parse mount entry: {}", e);
            }
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

    #[test]
    fn test_parse_k3s_mounts() {
        // Test that proc-mounts can parse a synthetic k3s /proc/mounts file
        // This file contains complex overlay mounts with very long options,
        // similar to what's seen in real k3s environments
        use proc_mounts::MountIter;

        let mounts_path = Path::new("testdata/k3s-mounts.txt");
        if !mounts_path.exists() {
            // Skip test if file not present (CI environment)
            return;
        }

        let mounts =
            MountIter::new_from_file(mounts_path).expect("Should be able to open k3s mounts file");

        let mut count = 0;
        let mut errors = 0;
        for mount in mounts {
            match mount {
                Ok(_) => count += 1,
                Err(e) => {
                    eprintln!("Parse error: {}", e);
                    errors += 1;
                }
            }
        }

        // Synthetic file has ~30 entries (plus comment lines which are skipped)
        assert!(count >= 25, "Expected >=25 mounts, got {}", count);
        // Should have zero parse errors
        assert_eq!(errors, 0, "Got parse errors: {}", errors);

        // Verify we can find specific known paths
        let mounts = MountIter::new_from_file(mounts_path).unwrap();
        let paths: Vec<PathBuf> = mounts.filter_map(|m| m.ok()).map(|m| m.dest).collect();

        // These paths should exist in the test file
        assert!(paths.contains(&PathBuf::from("/proc")));
        assert!(paths.contains(&PathBuf::from("/sys")));
        // Verify complex overlay paths are parsed
        assert!(paths
            .iter()
            .any(|p| p.to_string_lossy().contains("containerd")));
    }
}
