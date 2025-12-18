use clap::{Parser, ValueEnum};
use std::path::PathBuf;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

mod identity;
mod controller;
mod node;
mod volume;

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
            let node_name = args.node_name.clone().ok_or("--node-name is required in node mode")?;
            info!(node = %node_name, "Running in node mode");
            run_node(&args, &node_name).await?;
        }
    }

    Ok(())
}

async fn run_controller(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    use tonic::transport::Server;
    use csi::identity_server::IdentityServer;
    use csi::controller_server::ControllerServer;

    let identity_service = identity::IdentityService::new();
    let controller_service = controller::ControllerService::new();

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
    use tonic::transport::Server;
    use csi::identity_server::IdentityServer;
    use csi::node_server::NodeServer;

    let identity_service = identity::IdentityService::new();
    let node_service = node::NodeService::new(node_name.to_string(), args.base_path.clone());

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
