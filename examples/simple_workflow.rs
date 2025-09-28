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

    // Create a replicated variable to store our computation result
    let result_var = ReplicatedVar::new("result", &workflow_run, 0i32);

    // Simulate some computation and store the result
    let computation_result = 42 + 58; // Some "complex" computation
    let stored_result = result_var.set(computation_result).await?;

    println!("Stored computation result: {}", stored_result);

    // Create another replicated variable for a message
    let message_var = ReplicatedVar::new("message", &workflow_run, String::new());
    let message = message_var.set("Workflow completed successfully!".to_string()).await?;

    println!("Message: {}", message);

    // End the workflow with the computed result
    let final_result: i32 = workflow_run.finish_with(stored_result).await?;

    println!("Workflow '{}' completed with result: {:?}!", workflow_id, final_result);

    Ok(())
}