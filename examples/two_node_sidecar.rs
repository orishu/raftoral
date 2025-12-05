//! Two-Node Sidecar Cluster Demonstration
//!
//! This example demonstrates:
//! 1. Starting a bootstrap FullNode in sidecar mode
//! 2. Starting a second FullNode that joins the cluster in sidecar mode
//! 3. Both nodes running with their own sidecar servers
//!
//! Usage:
//!   cargo run --example two_node_sidecar

use raftoral::full_node::FullNode;
use raftoral::workflow_proxy_runtime::WorkflowProxyRuntime;
use slog::{Drain, Logger, o};
use std::time::Duration;
use tokio::time::sleep;

/// Create a logger for the example
fn create_logger(name: &str) -> Logger {
    let decorator = slog_term::PlainDecorator::new(std::io::stdout());
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    Logger::root(drain, o!("node" => name.to_string()))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("\n=== Two-Node Raftoral Sidecar Cluster Demonstration ===\n");

    // Node 1: Bootstrap mode (creates single-node cluster)
    println!("Step 1: Creating Node 1 (bootstrap mode)...");
    let node1_logger = create_logger("node1");
    let node1 = FullNode::<WorkflowProxyRuntime>::new_sidecar(
        1,                                    // node_id
        "127.0.0.1:50081".to_string(),       // Raft address
        "127.0.0.1:9011".to_string(),        // Sidecar address
        None,                                 // No persistent storage
        node1_logger.clone(),
    )
    .await?;

    println!("  ✓ Node 1 started");
    println!("    - Raft server: 127.0.0.1:50081");
    println!("    - Sidecar server: 127.0.0.1:9011");

    // Wait for node 1 to become leader
    sleep(Duration::from_millis(500)).await;
    let is_leader = node1.is_leader().await;
    println!("  ✓ Node 1 is leader: {}", is_leader);

    // Node 2: Join mode (discovers and joins via node 1)
    println!("\nStep 2: Creating Node 2 (join mode)...");
    let node2_logger = create_logger("node2");
    let node2 = FullNode::<WorkflowProxyRuntime>::new_joining_sidecar(
        "127.0.0.1:50082".to_string(),       // Raft address
        "127.0.0.1:9012".to_string(),        // Sidecar address
        vec!["127.0.0.1:50081".to_string()], // Seed addresses
        None,                                 // No persistent storage
        node2_logger.clone(),
    )
    .await?;

    println!("  ✓ Node 2 started");
    println!("    - Node ID: {}", node2.node_id());
    println!("    - Raft server: 127.0.0.1:50082");
    println!("    - Sidecar server: 127.0.0.1:9012");

    // Wait for cluster to stabilize
    println!("\nStep 3: Waiting for cluster to stabilize...");
    sleep(Duration::from_secs(2)).await;

    // Verify cluster status
    println!("\nStep 4: Verifying cluster status...");
    println!("  - Node 1 is leader: {}", node1.is_leader().await);
    println!("  - Node 2 is leader: {}", node2.is_leader().await);

    // List sub-clusters (should have at least one auto-created by ClusterManager)
    let clusters = node1.runtime().list_sub_clusters().await;
    println!("\n  ✓ Execution clusters created by ClusterManager: {:?}", clusters);

    if !clusters.is_empty() {
        let cluster_id = clusters[0];
        if let Some(metadata) = node1.runtime().get_sub_cluster(&cluster_id).await {
            println!("    - Cluster {} nodes: {:?}", cluster_id, metadata.node_ids);
        }
    }

    println!("\n=== Two-Node Sidecar Cluster Ready ===");
    println!("\nCluster Architecture:");
    println!("  ┌──────────────────────────────────────┐");
    println!("  │     Raft Management Cluster          │");
    println!("  │   (Node 1: Leader, Node 2: Follower) │");
    println!("  └────────┬─────────────────┬───────────┘");
    println!("           │                 │");
    println!("           │                 │");
    println!("  ┌────────▼────────┐ ┌──────▼──────────┐");
    println!("  │ SidecarServer   │ │ SidecarServer   │");
    println!("  │  (Port 9011)    │ │  (Port 9012)    │");
    println!("  └─────────────────┘ └─────────────────┘");
    println!("           │                 │");
    println!("           │                 │");
    println!("    [Applications can connect to either sidecar]");

    println!("\nApplications can connect to:");
    println!("  - http://127.0.0.1:9011 (Node 1)");
    println!("  - http://127.0.0.1:9012 (Node 2)");

    println!("\nPress Ctrl+C to exit...");
    tokio::signal::ctrl_c().await?;

    println!("\nShutting down...");
    node1.shutdown().await;
    node2.shutdown().await;

    Ok(())
}
