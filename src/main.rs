use raftoral::{WorkflowCommand, CheckpointData, WorkflowRuntime};
use tokio::time::{sleep, Duration};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting Raftoral single-node cluster...");

    // Create workflow runtime with integrated cluster
    let workflow_runtime = WorkflowRuntime::new_single_node(1).await?;
    println!("Raft cluster initialized with node ID: 1");

    // Give the cluster time to initialize
    sleep(Duration::from_millis(500)).await;

    println!("Testing workflow functionality...");

    // Test starting a workflow using the WorkflowRuntime API
    let workflow_id = "demo_workflow";
    println!("Starting workflow: {}", workflow_id);

    match workflow_runtime.start(workflow_id, ()).await {
        Ok(workflow_run) => {
            println!("Successfully started workflow: {}", workflow_id);

            sleep(Duration::from_millis(500)).await;

            // Test setting a checkpoint using low-level command via runtime
            println!("Setting checkpoint for workflow...");
            let checkpoint_command = WorkflowCommand::SetCheckpoint(CheckpointData {
                workflow_id: workflow_id.to_string(),
                key: "step1".to_string(),
                value: b"checkpoint data for step 1".to_vec(),
            });

            match workflow_runtime.cluster.propose_and_sync(checkpoint_command).await {
                Ok(()) => println!("Successfully set checkpoint"),
                Err(e) => eprintln!("Error setting checkpoint: {}", e),
            }

            // Test ending the workflow
            println!("Ending workflow: {}", workflow_id);
            match workflow_run.finish().await {
                Ok(()) => println!("Successfully ended workflow: {}", workflow_id),
                Err(e) => eprintln!("Failed to end workflow: {}", e),
            }
        },
        Err(e) => eprintln!("Failed to start workflow: {}", e),
    }

    println!("All workflow operations completed. Waiting for final processing...");
    sleep(Duration::from_secs(1)).await;

    println!("Raftoral workflow demo completed successfully!");

    Ok(())
}
