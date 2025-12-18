use std::process::{Child, Command};
use std::time::Duration;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

#[allow(clippy::doc_overindented_list_items)]
#[allow(clippy::doc_lazy_continuation)]
mod csi {
    tonic::include_proto!("csi.v1");
}

use csi::controller_client::ControllerClient;
use csi::identity_client::IdentityClient;
use csi::{CapacityRange, CreateVolumeRequest, DeleteVolumeRequest, GetPluginInfoRequest};

use std::sync::atomic::{AtomicU32, Ordering};

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

fn socket_path() -> String {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("/tmp/csi-integration-test-{}.sock", id)
}

struct TestServer {
    child: Child,
    socket: String,
}

impl TestServer {
    fn start(mode: &str) -> Self {
        let socket = socket_path();

        // Clean up any existing socket
        let _ = std::fs::remove_file(&socket);

        let mut cmd = Command::new("./target/debug/node-local-cache");
        cmd.arg("--mode")
            .arg(mode)
            .arg("--csi-socket")
            .arg(&socket)
            .arg("--log-level")
            .arg("warn");

        if mode == "node" {
            cmd.arg("--node-name").arg("test-node");
        }

        let child = cmd.spawn().expect("Failed to start server");

        // Wait for socket to be created
        for _ in 0..50 {
            if std::path::Path::new(&socket).exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        Self { child, socket }
    }

    fn socket_path(&self) -> &str {
        &self.socket
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

async fn connect_to_socket(socket: &str) -> Channel {
    let socket_path = socket.to_string();

    Endpoint::try_from("http://[::]:50051")
        .unwrap()
        .connect_with_connector(service_fn(move |_: Uri| {
            let path = socket_path.clone();
            async move {
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                    UnixStream::connect(path).await?,
                ))
            }
        }))
        .await
        .expect("Failed to connect to socket")
}

#[tokio::test]
async fn test_identity_get_plugin_info() {
    let server = TestServer::start("controller");
    let channel = connect_to_socket(server.socket_path()).await;

    let mut client = IdentityClient::new(channel);
    let response = client
        .get_plugin_info(GetPluginInfoRequest {})
        .await
        .expect("GetPluginInfo failed");

    let info = response.into_inner();
    assert_eq!(info.name, "node-local-cache.csi.io");
    assert!(!info.vendor_version.is_empty());
    println!(
        "✓ Identity: name={}, version={}",
        info.name, info.vendor_version
    );
}

#[tokio::test]
async fn test_identity_probe() {
    let server = TestServer::start("controller");
    let channel = connect_to_socket(server.socket_path()).await;

    let mut client = IdentityClient::new(channel);
    let response = client
        .probe(csi::ProbeRequest {})
        .await
        .expect("Probe failed");

    let probe = response.into_inner();
    assert_eq!(probe.ready, Some(true));
    println!("✓ Probe: ready={:?}", probe.ready);
}

#[tokio::test]
async fn test_controller_create_delete_volume() {
    let server = TestServer::start("controller");
    let channel = connect_to_socket(server.socket_path()).await;

    let mut client = ControllerClient::new(channel);

    // Create volume
    let create_response = client
        .create_volume(CreateVolumeRequest {
            name: "test-volume".to_string(),
            capacity_range: Some(CapacityRange {
                required_bytes: 1024 * 1024 * 100, // 100MB
                limit_bytes: 0,
            }),
            volume_capabilities: vec![],
            parameters: Default::default(),
            secrets: Default::default(),
            volume_content_source: None,
            accessibility_requirements: None,
            mutable_parameters: Default::default(),
        })
        .await
        .expect("CreateVolume failed");

    let volume = create_response
        .into_inner()
        .volume
        .expect("No volume in response");
    assert!(volume.volume_id.starts_with("nlc-"));
    assert_eq!(volume.capacity_bytes, 1024 * 1024 * 100);
    assert!(
        volume.accessible_topology.is_empty(),
        "Should have no topology constraints"
    );
    println!(
        "✓ CreateVolume: id={}, capacity={}",
        volume.volume_id, volume.capacity_bytes
    );

    // Delete volume
    let delete_response = client
        .delete_volume(DeleteVolumeRequest {
            volume_id: volume.volume_id.clone(),
            secrets: Default::default(),
        })
        .await
        .expect("DeleteVolume failed");

    println!("✓ DeleteVolume: id={}", volume.volume_id);
    let _ = delete_response; // Just checking it succeeded
}

#[tokio::test]
async fn test_volume_id_format() {
    let server = TestServer::start("controller");
    let channel = connect_to_socket(server.socket_path()).await;

    let mut client = ControllerClient::new(channel);

    // Create multiple volumes and verify unique IDs
    let mut ids = Vec::new();
    for i in 0..3 {
        let response = client
            .create_volume(CreateVolumeRequest {
                name: format!("test-volume-{}", i),
                capacity_range: None,
                volume_capabilities: vec![],
                parameters: Default::default(),
                secrets: Default::default(),
                volume_content_source: None,
                accessibility_requirements: None,
                mutable_parameters: Default::default(),
            })
            .await
            .expect("CreateVolume failed");

        let volume = response.into_inner().volume.expect("No volume");
        assert!(volume.volume_id.starts_with("nlc-"));
        assert_eq!(volume.volume_id.len(), 40); // "nlc-" + 36 char UUID
        ids.push(volume.volume_id);
    }

    // Verify all IDs are unique
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "Volume IDs should be unique");
    println!("✓ Generated {} unique volume IDs", ids.len());
}
