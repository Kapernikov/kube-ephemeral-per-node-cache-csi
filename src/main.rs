use clap::{Parser, ValueEnum};
use std::path::PathBuf;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

mod cleanup;
mod controller;
mod identity;
mod node;
mod volume;

#[allow(clippy::doc_overindented_list_items)]
#[allow(clippy::doc_lazy_continuation)]
pub mod csi {
    tonic::include_proto!("csi.v1");
}

#[derive(Debug, Clone, ValueEnum)]
enum Mode {
    Controller,
    Node,
}

#[derive(Parser, Debug)]
#[command(name = "node-local-cache")]
#[command(about = "CSI driver for node-local ephemeral cache volumes")]
struct Args {
    /// Run mode: controller or node
    #[arg(long, value_enum)]
    mode: Mode,

    /// Path to CSI socket
    #[arg(long, default_value = "/csi/csi.sock")]
    csi_socket: PathBuf,

    /// Node name (required for node mode)
    #[arg(long, env = "NODE_NAME")]
    node_name: Option<String>,

    /// Base path for cache volumes
    #[arg(long, default_value = "/var/node-local-cache")]
    base_path: PathBuf,

    /// Kubernetes namespace for cleanup coordination
    #[arg(long, env = "POD_NAMESPACE", default_value = "node-local-cache")]
    namespace: String,

    /// Log level
    #[arg(long, default_value = "info")]
    log_level: Level,

    /// Disable cleanup service (for testing only - will leak disk space)
    #[arg(long, default_value = "false")]
    no_cleanup_service: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Initialize logging
    FmtSubscriber::builder()
        .with_max_level(args.log_level)
        .json()
        .init();

    info!(
        mode = ?args.mode,
        socket = %args.csi_socket.display(),
        "Starting node-local-cache CSI driver"
    );

    match args.mode {
        Mode::Controller => {
            info!("Running in controller mode");
            run_controller(&args).await?;
        }
        Mode::Node => {
            let node_name = args
                .node_name
                .clone()
                .ok_or("--node-name is required in node mode")?;
            info!(node = %node_name, "Running in node mode");
            run_node(&args, &node_name).await?;
        }
    }

    Ok(())
}

async fn run_controller(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    use csi::controller_server::ControllerServer;
    use csi::identity_server::IdentityServer;
    use std::time::Duration;
    use tonic::transport::Server;

    let identity_service = identity::IdentityService::new(true); // controller mode

    // Create kube client for cleanup coordination
    let controller_service = if args.no_cleanup_service {
        tracing::warn!(
            "Cleanup service disabled via --no-cleanup-service flag. This will leak disk space!"
        );
        controller::ControllerService::new()
    } else {
        let client = kube::Client::try_default().await.map_err(|e| {
            format!(
                "Failed to create Kubernetes client: {}. \
                Use --no-cleanup-service for testing without cleanup.",
                e
            )
        })?;

        info!(namespace = %args.namespace, "Kubernetes client initialized, cleanup enabled");

        // Start cleanup processor in background (checks for decommissioned nodes, prunes completed)
        tokio::spawn(cleanup::run_controller_cleanup_loop(
            client.clone(),
            args.namespace.clone(),
            Duration::from_secs(60), // check interval
        ));

        let cleanup_ctrl = cleanup::CleanupController::new(client, args.namespace.clone());
        controller::ControllerService::with_cleanup(cleanup_ctrl)
    };

    // Remove existing socket if present
    let _ = std::fs::remove_file(&args.csi_socket);

    // Create parent directory
    if let Some(parent) = args.csi_socket.parent() {
        std::fs::create_dir_all(parent)?;
    }

    info!(socket = %args.csi_socket.display(), "Listening on Unix socket");

    // Use UDS (Unix Domain Socket)
    let uds = tokio::net::UnixListener::bind(&args.csi_socket)?;
    let uds_stream = tokio_stream::wrappers::UnixListenerStream::new(uds);

    Server::builder()
        .add_service(IdentityServer::new(identity_service))
        .add_service(ControllerServer::new(controller_service))
        .serve_with_incoming(uds_stream)
        .await?;

    Ok(())
}

async fn run_node(args: &Args, node_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    use csi::identity_server::IdentityServer;
    use csi::node_server::NodeServer;
    use std::time::Duration;
    use tonic::transport::Server;

    let identity_service = identity::IdentityService::new(false); // node mode

    // Create node service, optionally with cleanup tracking
    let node_service = if args.no_cleanup_service {
        tracing::warn!(
            "Cleanup service disabled via --no-cleanup-service flag. This will leak disk space!"
        );
        node::NodeService::new(node_name.to_string(), args.base_path.clone())
    } else {
        let client = kube::Client::try_default().await.map_err(|e| {
            format!(
                "Failed to create Kubernetes client: {}. \
                Use --no-cleanup-service for testing without cleanup.",
                e
            )
        })?;

        info!(
            namespace = %args.namespace,
            node = %node_name,
            "Starting cleanup watcher"
        );

        // Start cleanup watcher in background (every 10 seconds)
        let cleanup_node = cleanup::CleanupNode::new(
            client.clone(),
            args.namespace.clone(),
            node_name.to_string(),
            args.base_path.clone(),
        );
        tokio::spawn(cleanup_node.run_cleanup_loop(Duration::from_secs(10)));

        // Create node service with cleanup tracking enabled
        node::NodeService::new(node_name.to_string(), args.base_path.clone())
            .with_cleanup(client, args.namespace.clone())
    };

    // Remove existing socket if present
    let _ = std::fs::remove_file(&args.csi_socket);

    // Create parent directory
    if let Some(parent) = args.csi_socket.parent() {
        std::fs::create_dir_all(parent)?;
    }

    info!(socket = %args.csi_socket.display(), "Listening on Unix socket");

    let uds = tokio::net::UnixListener::bind(&args.csi_socket)?;
    let uds_stream = tokio_stream::wrappers::UnixListenerStream::new(uds);

    Server::builder()
        .add_service(IdentityServer::new(identity_service))
        .add_service(NodeServer::new(node_service))
        .serve_with_incoming(uds_stream)
        .await?;

    Ok(())
}
