use clap::Parser;
use raftoral::raft::generic::grpc_transport::{GrpcClusterTransport, NodeConfig};
use raftoral::raft::generic::transport::ClusterTransport;
use raftoral::workflow::WorkflowCommandExecutor;
use raftoral::grpc::{start_grpc_server, discover_peers, bootstrap};
use raftoral::WorkflowRuntime;
use std::sync::Arc;
use tokio::signal;

#[derive(Parser, Debug)]
#[command(name = "raftoral")]
#[command(about = "Raftoral distributed workflow orchestration", long_about = None)]
struct Args {
    /// Address to bind the gRPC server to (e.g., 127.0.0.1:5001)
    #[arg(short, long)]
    address: String,

    /// Node ID (optional, will be auto-assigned if joining existing cluster)
    #[arg(short, long)]
    node_id: Option<u64>,

    /// Addresses of peer nodes to discover (e.g., 127.0.0.1:5001,127.0.0.1:5002)
    #[arg(short, long, value_delimiter = ',')]
    peers: Vec<String>,

    /// Bootstrap a new cluster (use this for the first node)
    #[arg(short, long, default_value_t = false)]
    bootstrap: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    println!("🚀 Starting Raftoral node...");
    println!("   Address: {}", args.address);

    let node_id = if args.bootstrap {
        // Bootstrap mode: start as first node with ID 1
        let node_id = args.node_id.unwrap_or(1);
        println!("   Mode: Bootstrap (starting new cluster)");
        println!("   Node ID: {}", node_id);
        node_id
    } else {
        // Join mode: discover peers and get node ID
        println!("   Mode: Join existing cluster");

        if args.peers.is_empty() {
            eprintln!("Error: --peers is required when not in bootstrap mode");
            std::process::exit(1);
        }

        println!("   Discovering peers: {:?}", args.peers);
        let discovered = discover_peers(args.peers.clone()).await;

        if discovered.is_empty() {
            eprintln!("Error: Could not discover any peers");
            std::process::exit(1);
        }

        let node_id = if let Some(id) = args.node_id {
            println!("   Using provided node ID: {}", id);
            id
        } else {
            let id = bootstrap::next_node_id(&discovered);
            println!("   Auto-assigned node ID: {}", id);
            id
        };

        node_id
    };

    // Build initial node configurations
    let mut nodes = vec![NodeConfig {
        node_id,
        address: args.address.clone(),
    }];

    // If we have peers, add them to the initial configuration
    if !args.peers.is_empty() {
        let discovered = discover_peers(args.peers.clone()).await;
        for peer in discovered {
            nodes.push(NodeConfig {
                node_id: peer.node_id,
                address: peer.address,
            });
        }
    }

    println!("   Cluster nodes: {} total", nodes.len());

    // Create transport and cluster
    let transport = Arc::new(GrpcClusterTransport::<WorkflowCommandExecutor>::new(nodes));
    transport.start().await?;

    let cluster = transport.create_cluster(node_id).await?;
    println!("✓ Raft cluster initialized");

    // Start gRPC server
    let server_handle = start_grpc_server(
        args.address.clone(),
        transport.clone(),
        cluster.clone(),
        node_id,
    ).await?;
    println!("✓ gRPC server listening on {}", args.address);

    // Create workflow runtime
    let _runtime = WorkflowRuntime::new(cluster);
    println!("✓ Workflow runtime ready");

    println!("\n🎉 Node {} is running!", node_id);
    println!("   Press Ctrl+C to shutdown gracefully\n");

    // Wait for shutdown signal
    signal::ctrl_c().await?;
    println!("\n🛑 Shutdown signal received, stopping gracefully...");

    server_handle.shutdown();
    println!("✓ Server stopped");

    Ok(())
}
