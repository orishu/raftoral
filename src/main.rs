use raftoral::{WorkflowCluster, WorkflowCommand, start_workflow, end_workflow};
use tokio::time::{sleep, Duration};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting Raftoral single-node cluster...");

    let cluster = WorkflowCluster::new_single_node(1).await?;
    println!("Raft cluster initialized with node ID: 1");

    // Give the cluster time to initialize
    sleep(Duration::from_millis(500)).await;

    println!("Testing workflow functionality...");

    // Test starting a workflow using the high-level API
    let workflow_id = "demo_workflow";
    println!("Starting workflow: {}", workflow_id);

    match start_workflow(workflow_id, &cluster).await {
        Ok(()) => println!("Successfully started workflow: {}", workflow_id),
        Err(e) => eprintln!("Failed to start workflow: {}", e),
    }

    sleep(Duration::from_millis(500)).await;

    // Test setting a checkpoint using low-level command
    println!("Setting checkpoint for workflow...");
    let checkpoint_command = WorkflowCommand::SetCheckpoint {
        workflow_id: workflow_id.to_string(),
        key: "step1".to_string(),
        value: b"checkpoint data for step 1".to_vec(),
    };

    match cluster.propose_command(checkpoint_command).await {
        Ok(success) => {
            if success {
                println!("Successfully set checkpoint");
            } else {
                println!("Failed to set checkpoint");
            }
        },
        Err(e) => eprintln!("Error setting checkpoint: {}", e),
    }

    sleep(Duration::from_millis(500)).await;

    // Test ending the workflow
    println!("Ending workflow: {}", workflow_id);
    match end_workflow(workflow_id, &cluster).await {
        Ok(()) => println!("Successfully ended workflow: {}", workflow_id),
        Err(e) => eprintln!("Failed to end workflow: {}", e),
    }

    println!("All workflow operations completed. Waiting for final processing...");
    sleep(Duration::from_secs(1)).await;

    println!("Raftoral workflow demo completed successfully!");

    Ok(())
}
