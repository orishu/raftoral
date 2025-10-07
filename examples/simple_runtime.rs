//! Simple example demonstrating the RaftoralGrpcRuntime API.
//!
//! This example shows how to:
//! 1. Bootstrap a new cluster
//! 2. Register a simple workflow
//! 3. Execute the workflow
//! 4. Gracefully shutdown
//!
//! Run with:
//! ```
//! cargo run --example simple_runtime
//! ```

use raftoral::runtime::{RaftoralConfig, RaftoralGrpcRuntime};
use raftoral::workflow::{WorkflowContext, WorkflowError};
use serde::{Deserialize, Serialize};
use tokio::signal;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ComputeInput {
    value: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ComputeOutput {
    result: i32,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logger
    env_logger::init();

    println!("=== Simple Runtime Example ===\n");

    // Create configuration for a new cluster
    let config = RaftoralConfig::bootstrap("127.0.0.1:7001".to_string(), Some(1));

    // Start the runtime - this handles all initialization
    let runtime = RaftoralGrpcRuntime::start(config).await?;

    // Register a simple computation workflow
    let compute_fn = |input: ComputeInput, _ctx: WorkflowContext| async move {
        // Simple computation: multiply by 2
        let result = input.value * 2;
        Ok::<ComputeOutput, WorkflowError>(ComputeOutput { result })
    };

    runtime
        .workflow_runtime()
        .register_workflow_closure("compute", 1, compute_fn)?;
    println!("✓ Registered 'compute' workflow");

    // Execute the workflow
    println!("\nExecuting workflow...");
    let input = ComputeInput { value: 21 };

    let workflow_run = runtime
        .workflow_runtime()
        .start_workflow::<ComputeInput, ComputeOutput>("compute", 1, input)
        .await?;

    println!("Started workflow: {}", workflow_run.workflow_id());

    let output = workflow_run.wait_for_completion().await?;
    println!("✓ Workflow completed: {} * 2 = {}", 21, output.result);

    println!("\nPress Ctrl+C to shutdown...");
    signal::ctrl_c().await?;

    // Gracefully shutdown
    runtime.shutdown().await?;

    Ok(())
}
