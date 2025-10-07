//! Example gRPC client for running workflows remotely.
//!
//! This demonstrates how to invoke workflows via gRPC on any node in the cluster.
//!
//! Run with:
//! ```
//! # Start a node first:
//! RUST_LOG=info cargo run -- --listen 127.0.0.1:7001 --bootstrap
//!
//! # Then run this client:
//! cargo run --example grpc_workflow_client
//! ```

use raftoral::grpc::server::raft_proto::{
    workflow_management_client::WorkflowManagementClient,
    RunWorkflowRequest,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== gRPC Workflow Client Example ===\n");

    // Connect to any node in the cluster
    let node_address = "http://127.0.0.1:7001";
    println!("Connecting to node at {}", node_address);

    let mut client = WorkflowManagementClient::connect(node_address).await?;
    println!("✓ Connected\n");

    // Prepare workflow input (must be JSON)
    let input_json = serde_json::to_string("ping")?;

    println!("Calling workflow: ping_pong (v1)");
    println!("Input: {}", input_json);

    // Call the workflow
    let request = tonic::Request::new(RunWorkflowRequest {
        workflow_type: "ping_pong".to_string(),
        version: 1,
        input_json,
    });

    let response = client.run_workflow_sync(request).await?;
    let result = response.into_inner();

    println!("\nWorkflow Response:");
    if result.success {
        println!("✓ Success!");
        println!("Result: {}", result.result_json);
    } else {
        println!("✗ Failed!");
        println!("Error: {}", result.error);
    }

    Ok(())
}
