use clap::Parser;
use raftoral::raft::generic::grpc_transport::{GrpcClusterTransport, NodeConfig};
use raftoral::raft::generic::transport::ClusterTransport;
use raftoral::workflow::{WorkflowCommandExecutor, WorkflowContext};
use raftoral::grpc::{start_grpc_server, discover_peers, bootstrap};
use raftoral::{WorkflowRuntime, WorkflowError};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::signal;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PingInput {
    message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PongOutput {
    message: String,
}

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

    let (node_id, _discovered_peers) = if args.bootstrap {
        // Bootstrap mode: start as first node with ID 1
        let node_id = args.node_id.unwrap_or(1);
        println!("   Mode: Bootstrap (starting new cluster)");
        println!("   Node ID: {}", node_id);
        (node_id, Vec::new())
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

        println!("   Discovered {} peer(s)", discovered.len());
        for peer in &discovered {
            println!("     - Node {}: {}", peer.node_id, peer.address);
        }

        let node_id = if let Some(id) = args.node_id {
            println!("   Using provided node ID: {}", id);
            id
        } else {
            let id = bootstrap::next_node_id(&discovered);
            println!("   Auto-assigned node ID: {}", id);
            id
        };

        (node_id, discovered)
    };

    // Build initial node configuration - only this node
    // Other nodes will be added dynamically via add_peer when they join
    let nodes = vec![NodeConfig {
        node_id,
        address: advertise_addr.clone(),
    }];

    println!("   Initial transport nodes: {}", nodes.len());

    // Create transport and start it
    let transport = Arc::new(GrpcClusterTransport::<WorkflowCommandExecutor>::new(nodes));
    transport.start().await?;
    println!("✓ Transport started");

    // Create cluster (this starts the Raft event loop)
    let cluster = transport.create_cluster(node_id).await?;
    println!("✓ Raft cluster initialized");

    // Start gRPC server BEFORE add_node (server must be ready to receive messages)
    let server_handle = start_grpc_server(
        args.listen.clone(),
        transport.clone(),
        cluster.clone(),
        node_id,
    ).await?;
    println!("✓ gRPC server listening on {}", args.listen);

    // If joining an existing cluster, add this node via ConfChange
    // This must happen AFTER the server is started so we can receive Raft messages
    if !args.bootstrap && !args.peers.is_empty() {
        println!("   Adding node {} to existing cluster...", node_id);
        // add_node automatically routes to the leader
        match cluster.add_node(node_id, advertise_addr.clone()).await {
            Ok(_) => println!("✓ Node added to cluster"),
            Err(e) => {
                eprintln!("⚠ Failed to add node to cluster: {}", e);
                eprintln!("   Continuing anyway - node may already be in cluster");
            }
        }
    }

    // Create workflow runtime and register workflows
    let runtime = WorkflowRuntime::new(cluster.clone());
    println!("✓ Workflow runtime ready");

    // Register ping/pong workflow
    // Input: { "message": "ping" } -> Output: { "message": "pong" }
    // Any other input returns an error
    let ping_pong_fn = |input: PingInput, _context: WorkflowContext| async move {
        if input.message == "ping" {
            Ok::<PongOutput, WorkflowError>(PongOutput {
                message: "pong".to_string(),
            })
        } else {
            Err(WorkflowError::ClusterError(format!(
                "Expected 'ping', got '{}'",
                input.message
            )))
        }
    };

    runtime
        .register_workflow_closure("ping_pong", 1, ping_pong_fn)
        .expect("Failed to register ping_pong workflow");
    println!("✓ Registered ping_pong workflow (v1)");

    println!("\n🎉 Node {} is running!", node_id);
    println!("   Cluster size: {} nodes", cluster.get_node_ids().await.len());
    println!("   Press Ctrl+C to shutdown gracefully\n");

    // Give the cluster a moment to stabilize (especially important for joining nodes)
    if !args.bootstrap {
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    }

    // Example: Execute ping/pong workflow to test the system
    // Note: This works from any node - start_workflow automatically routes to the leader
    println!("📌 Testing ping/pong workflow...");

    let test_input = PingInput {
        message: "ping".to_string(),
    };

    match runtime
        .start_workflow::<PingInput, PongOutput>("ping_pong", 1, test_input)
        .await
    {
        Ok(workflow_run) => {
            println!("   Started ping/pong workflow: {}", workflow_run.workflow_id());

            match workflow_run.wait_for_completion().await {
                Ok(output) => {
                    println!("   ✓ Workflow completed: got '{}'", output.message);
                }
                Err(e) => {
                    eprintln!("   ✗ Workflow failed: {}", e);
                }
            }
        }
        Err(e) => {
            eprintln!("   ✗ Failed to start workflow: {}", e);
        }
    }
    println!();

    // Wait for shutdown signal
    signal::ctrl_c().await?;
    println!("\n🛑 Shutdown signal received, stopping gracefully...");

    // Remove this node from the cluster before shutting down
    if !args.bootstrap || cluster.get_node_ids().await.len() > 1 {
        println!("   Removing node {} from cluster...", node_id);
        match cluster.remove_node(node_id).await {
            Ok(_) => println!("✓ Node removed from cluster"),
            Err(e) => eprintln!("⚠ Failed to remove node from cluster: {}", e),
        }
    } else {
        println!("   (Skipping node removal - last node in cluster)");
    }

    server_handle.shutdown();
    println!("✓ Server stopped");

    Ok(())
}
