//! Demonstration of the `checkpoint!` and `checkpoint_compute!` macros.
//!
//! This example shows how the checkpoint macros create replicated variables with
//! explicit keys that remain stable across workflow versions.
//!
//! Run with:
//! ```
//! cargo run --example checkpoint_macro_demo
//! ```

use raftoral::runtime::{RaftoralConfig, RaftoralGrpcRuntime};
use raftoral::workflow::{WorkflowContext, WorkflowError};
use raftoral::{checkpoint, checkpoint_compute};
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
    computed_value: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    println!("=== Checkpoint Macro Demo ===\n");

    // Bootstrap a single-node cluster for testing
    let config = RaftoralConfig::bootstrap("127.0.0.1:0".to_string(), Some(1));
    let runtime = RaftoralGrpcRuntime::start(config).await?;

    // Register workflow demonstrating both checkpoint! and checkpoint_compute!
    runtime
        .workflow_runtime()
        .register_workflow_closure(
        "compute_with_macro",
        1,
        |input: ComputeInput, ctx: WorkflowContext| async move {
            println!("Starting workflow with input: {:?}", input);

            // checkpoint! - Clean syntax with explicit keys that stay stable across versions
            let mut counter = checkpoint!(ctx, "counter", input.start);
            let mut history = checkpoint!(ctx, "history", Vec::<i32>::new());

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

            // checkpoint_compute! - Side-effect computation (executed once, result replicated)
            let computed = checkpoint_compute!(ctx, "computed_timestamp", || async {
                // Simulate expensive computation or external API call
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                format!("Computed at timestamp: {}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis())
            });

            println!("Computed value: {}", *computed);

            Ok::<ComputeOutput, WorkflowError>(ComputeOutput {
                result: *counter,
                history: (*history).clone(),
                computed_value: (*computed).clone(),
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
        .workflow_runtime()
        .start_workflow::<ComputeInput, ComputeOutput>("compute_with_macro", 1, input)
        .await?;

    let output = workflow_run.wait_for_completion().await?;

    println!("\n=== Results ===");
    println!("Final result: {}", output.result);
    println!("History: {:?}", output.history);
    println!("Computed: {}", output.computed_value);
    println!("\n✓ Demo complete!");

    // Shutdown the runtime
    runtime.shutdown().await?;

    Ok(())
}
