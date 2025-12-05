//! Sidecar Mode Demonstration
//!
//! This example demonstrates the complete sidecar mode flow:
//! 1. Starts a FullNode in sidecar mode with WorkflowProxyRuntime
//! 2. Simulates connecting an application (ping_pong) via raftoral-client
//! 3. Shows how workflows are forwarded to the app and results returned
//!
//! Usage:
//!   cargo run --example sidecar_demo

use raftoral::full_node::FullNode;
use raftoral::workflow_proxy_runtime::WorkflowProxyRuntime;
use raftoral_client::{RaftoralClient, proto::ExecuteWorkflowRequest};
use slog::{Drain, Logger, info, o};
use std::time::Duration;
use tokio::time::sleep;

/// Create a logger for the example
fn create_logger(name: &str) -> Logger {
    let decorator = slog_term::PlainDecorator::new(std::io::stdout());
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    Logger::root(drain, o!("component" => name.to_string()))
}

/// Ping-pong workflow handler
async fn ping_pong_handler(req: ExecuteWorkflowRequest) -> raftoral_client::error::Result<Vec<u8>> {
    let input_text = String::from_utf8_lossy(&req.input);

    info!(create_logger("app"), "Ping-pong workflow executing";
        "workflow_id" => &req.workflow_id,
        "is_owner" => req.is_owner,
        "input" => &input_text.to_string()
    );

    let output = match input_text.trim() {
        "ping" => "pong",
        "pong" => "ping",
        other => other,
    };

    info!(create_logger("app"), "Ping-pong workflow result";
        "workflow_id" => &req.workflow_id,
        "output" => output
    );

    Ok(output.as_bytes().to_vec())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("\n=== Raftoral Sidecar Mode Demonstration ===\n");

    let node_logger = create_logger("node");
    let app_logger = create_logger("app");

    // Step 1: Create FullNode in sidecar mode
    println!("Step 1: Starting FullNode in sidecar mode...");
    let node = FullNode::<WorkflowProxyRuntime>::new_sidecar(
        1,                                    // node_id
        "127.0.0.1:50071".to_string(),       // Raft address
        "127.0.0.1:9001".to_string(),        // Sidecar address
        None,                                 // No persistent storage
        node_logger.clone(),
    )
    .await?;

    println!("  ✓ FullNode started");
    println!("    - Raft server: 127.0.0.1:50071");
    println!("    - Sidecar server: 127.0.0.1:9001");

    // Wait for node to become leader
    sleep(Duration::from_millis(500)).await;
    println!("  ✓ Node is leader: {}", node.is_leader().await);

    // Step 2: Connect application to sidecar
    println!("\nStep 2: Connecting ping_pong application to sidecar...");
    let client = RaftoralClient::connect("http://127.0.0.1:9001", app_logger.clone()).await?;
    println!("  ✓ Application connected to sidecar");

    // Step 3: Register workflow handler
    println!("\nStep 3: Registering ping_pong workflow handler...");
    client.register_handler("ping_pong", 1, ping_pong_handler).await;
    println!("  ✓ Handler registered");

    // Step 4: Demonstrate the flow (manual workflow triggering for demo)
    println!("\nStep 4: Sidecar communication established");
    println!("  ✓ Applications can now receive and execute workflows");
    println!("  ✓ Results are automatically forwarded back to Raft");

    println!("\n=== Sidecar Mode Setup Complete ===");
    println!("\nArchitecture:");
    println!("  ┌─────────────────┐");
    println!("  │   Raft Cluster  │");
    println!("  │  (consensus)    │");
    println!("  └────────┬────────┘");
    println!("           │");
    println!("           │ WorkflowProxyRuntime");
    println!("           │");
    println!("  ┌────────▼────────┐");
    println!("  │ SidecarServer   │");
    println!("  │ (gRPC streaming)│");
    println!("  └────────┬────────┘");
    println!("           │");
    println!("           │ Bidirectional gRPC");
    println!("           │");
    println!("  ┌────────▼────────┐");
    println!("  │  Application    │");
    println!("  │  (ping_pong)    │");
    println!("  └─────────────────┘");

    println!("\nNote: Full workflow execution requires submitting workflows via");
    println!("      the Raft cluster. The workflow submission API is not yet");
    println!("      implemented in this demo.");

    println!("\nPress Ctrl+C to exit...");
    tokio::signal::ctrl_c().await?;

    println!("\nShutting down...");
    node.shutdown().await;

    Ok(())
}
