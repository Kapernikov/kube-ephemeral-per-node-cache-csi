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

    // Try to create kube client for cleanup coordination
    let controller_service = match kube::Client::try_default().await {
        Ok(client) => {
            info!(namespace = %args.namespace, "Kubernetes client initialized, cleanup enabled");

            // Start cleanup pruner in background
            // Prune ConfigMaps older than 5 minutes, check every minute
            tokio::spawn(cleanup::run_controller_prune_loop(
                client.clone(),
                args.namespace.clone(),
                Duration::from_secs(60),  // check interval
                Duration::from_secs(300), // TTL (5 minutes)
            ));

            let cleanup_ctrl = cleanup::CleanupController::new(client, args.namespace.clone());
            controller::ControllerService::with_cleanup(cleanup_ctrl)
        }
        Err(e) => {
            info!(error = %e, "Kubernetes client not available, cleanup disabled");
            controller::ControllerService::new()
        }
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
    let node_service = node::NodeService::new(node_name.to_string(), args.base_path.clone());

    // Start cleanup watcher in background if kube client is available
    if let Ok(client) = kube::Client::try_default().await {
        info!(
            namespace = %args.namespace,
            node = %node_name,
            "Starting cleanup watcher"
        );
        let cleanup_node = cleanup::CleanupNode::new(
            client,
            args.namespace.clone(),
            node_name.to_string(),
            args.base_path.clone(),
        );
        // Run cleanup loop in background (every 10 seconds)
        tokio::spawn(cleanup_node.run_cleanup_loop(Duration::from_secs(10)));
    } else {
        info!("Kubernetes client not available, cleanup watcher disabled");
    }

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
