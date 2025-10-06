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
    /// Address to listen on for gRPC connections (e.g., 0.0.0.0:5001)
    #[arg(short = 'l', long)]
    listen: String,

    /// Advertised address for other nodes to connect to (e.g., 192.168.1.10:5001)
    /// If not specified, uses the listen address
    #[arg(short = 'a', long)]
    advertise: Option<String>,

    /// Node ID (optional, will be auto-assigned if joining existing cluster)
    #[arg(short, long)]
    node_id: Option<u64>,

    /// Addresses of peer nodes to discover (e.g., 192.168.1.10:5001,192.168.1.11:5001)
    #[arg(short, long, value_delimiter = ',')]
    peers: Vec<String>,

    /// Bootstrap a new cluster (use this for the first node)
    #[arg(short, long, default_value_t = false)]
    bootstrap: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Use advertise address if specified, otherwise use listen address
    let advertise_addr = args.advertise.as_ref().unwrap_or(&args.listen).clone();

    println!("🚀 Starting Raftoral node...");
    println!("   Listen: {}", args.listen);
    if args.advertise.is_some() {
        println!("   Advertise: {}", advertise_addr);
    }

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
        address: advertise_addr.clone(),
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

    // If joining an existing cluster, add this node via ConfChange
    if !args.bootstrap && !args.peers.is_empty() {
        println!("   Adding node {} to existing cluster...", node_id);
        match cluster.add_node(node_id, advertise_addr.clone()).await {
            Ok(_) => println!("✓ Node added to cluster"),
            Err(e) => {
                eprintln!("⚠ Failed to add node to cluster: {}", e);
                eprintln!("   Continuing anyway - node may already be in cluster");
            }
        }
    }

    // Start gRPC server
    let server_handle = start_grpc_server(
        args.listen.clone(),
        transport.clone(),
        cluster.clone(),
        node_id,
    ).await?;
    println!("✓ gRPC server listening on {}", args.listen);

    // Create workflow runtime
    let _runtime = WorkflowRuntime::new(cluster.clone());
    println!("✓ Workflow runtime ready");

    println!("\n🎉 Node {} is running!", node_id);
    println!("   Press Ctrl+C to shutdown gracefully\n");

    // Wait for shutdown signal
    signal::ctrl_c().await?;
    println!("\n🛑 Shutdown signal received, stopping gracefully...");

    // Remove this node from the cluster before shutting down
    println!("   Removing node {} from cluster...", node_id);
    match cluster.remove_node(node_id).await {
        Ok(_) => println!("✓ Node removed from cluster"),
        Err(e) => eprintln!("⚠ Failed to remove node from cluster: {}", e),
    }

    server_handle.shutdown();
    println!("✓ Server stopped");

    Ok(())
}
