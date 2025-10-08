//! Demonstration of the `#[replicated]` macro for clean replicated variable syntax.
//!
//! This example shows how the proc macro makes replicated variables look almost
//! like regular Rust variables, while automatically handling checkpointing.

use raftoral::{WorkflowRuntime, WorkflowContext, WorkflowError, replicated};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ComputeInput {
    start: i32,
    iterations: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ComputeOutput {
    result: i32,
    history: Vec<i32>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    println!("=== Replicated Macro Demo ===\n");

    // Create single-node runtime for testing
    let runtime = WorkflowRuntime::new_single_node(1).await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await; // Wait for leadership

    // Register workflow using the clean #[replicated] syntax
    runtime.register_workflow_closure(
        "compute_with_macro",
        1,
        |input: ComputeInput, ctx: WorkflowContext| async move {
            println!("Starting workflow with input: {:?}", input);

            // Clean syntax! Looks almost like regular variables
            replicated!(ctx, let mut counter = input.start);
            replicated!(ctx, let mut history = Vec::<i32>::new());

            println!("Initial counter: {}", *counter);

            for i in 0..input.iterations {
                // Read with deref (*)
                let current_value = *counter;

                // Update counter
                counter.set(current_value * 2).await?;
                println!("Iteration {}: counter = {}", i + 1, *counter);

                // Capture the value we want to push
                let value_to_push = *counter;

                // Update history using the update method
                history.update(move |mut h| {
                    h.push(value_to_push);
                    h
                }).await?;
            }

            Ok::<ComputeOutput, WorkflowError>(ComputeOutput {
                result: *counter,
                history: (*history).clone(),
            })
        },
    )?;

    println!("✓ Registered workflow\n");

    // Execute the workflow
    let input = ComputeInput {
        start: 5,
        iterations: 3,
    };

    println!("Executing workflow...");
    let workflow_run = runtime
        .start_workflow::<ComputeInput, ComputeOutput>("compute_with_macro", 1, input)
        .await?;

    let output = workflow_run.wait_for_completion().await?;

    println!("\n=== Results ===");
    println!("Final result: {}", output.result);
    println!("History: {:?}", output.history);
    println!("\n✓ Demo complete!");

    Ok(())
}
