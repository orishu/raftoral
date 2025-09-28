use raftoral::{WorkflowCommandExecutor, RaftCluster, WorkflowCommand};
use std::time::Instant;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Precise Sync Demo - Testing improved propose_and_sync");

    let executor = WorkflowCommandExecutor::new();
    let cluster = RaftCluster::new_single_node(1, executor).await?;

    // Test multiple rapid commands to demonstrate precise tracking
    let commands = vec![
        WorkflowCommand::WorkflowStart { workflow_id: "workflow_1".to_string() },
        WorkflowCommand::SetCheckpoint {
            workflow_id: "workflow_1".to_string(),
            key: "step1".to_string(),
            value: b"checkpoint data 1".to_vec()
        },
        WorkflowCommand::SetCheckpoint {
            workflow_id: "workflow_1".to_string(),
            key: "step2".to_string(),
            value: b"checkpoint data 2".to_vec()
        },
        WorkflowCommand::WorkflowEnd { workflow_id: "workflow_1".to_string() },
    ];

    println!("Submitting {} commands with precise tracking...", commands.len());

    let start_time = Instant::now();

    // Submit all commands using propose_and_sync for precise tracking
    for (i, command) in commands.into_iter().enumerate() {
        let cmd_start = Instant::now();

        match cluster.propose_and_sync(command).await {
            Ok(_) => {
                println!("✅ Command {} completed in {:?}", i + 1, cmd_start.elapsed());
            },
            Err(e) => {
                println!("❌ Command {} failed: {}", i + 1, e);
            }
        }
    }

    let total_time = start_time.elapsed();
    println!("🎉 All commands completed in {:?} with precise tracking!", total_time);
    println!("No heuristic sleeps - each command waited for exact completion!");

    Ok(())
}