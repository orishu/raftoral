use std::time::Duration;
use raftoral::{WorkflowRuntime, ReplicatedVar};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workflow_id = "simple_example";

    // Create a workflow runtime with integrated cluster
    let workflow_runtime = WorkflowRuntime::new_single_node(1).await?;

    // Wait for leadership establishment
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start the workflow using WorkflowRuntime
    let workflow_run = workflow_runtime.start(workflow_id).await?;

    println!("Started workflow: {}", workflow_id);

    // Create and initialize a replicated variable with direct computation
    let result_var = ReplicatedVar::with_computation(
        "result",
        &workflow_run,
        || async {
            // Some "complex" computation
            println!("Performing computation...");
            42 + 58
        }
    ).await?;

    println!("Stored computation result: {}", result_var.get());

    // Create another replicated variable for a message using direct value
    let message_var = ReplicatedVar::with_value(
        "message",
        &workflow_run,
        "Workflow completed successfully!".to_string()
    ).await?;

    println!("Message: {}", message_var.get());

    // End the workflow with the computed result
    let final_result: i32 = workflow_run.finish_with(result_var.get()).await?;

    println!("Workflow '{}' completed with result: {:?}!", workflow_id, final_result);

    Ok(())
}