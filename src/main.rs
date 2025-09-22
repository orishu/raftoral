use raftoral::{RaftCluster, PlaceholderCommand};
use tokio::time::{sleep, Duration};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting Raftoral single-node cluster...");

    let cluster = RaftCluster::new_single_node(1).await?;
    println!("Raft cluster initialized with node ID: 1");

    // Give the cluster time to initialize
    sleep(Duration::from_millis(500)).await;

    println!("Proposing placeholder commands...");

    for i in 1..=3 {
        let cmd = PlaceholderCommand {
            id: i,
            data: format!("Test command {}", i),
        };

        match cluster.propose_placeholder_command(cmd).await {
            Ok(success) => {
                if success {
                    println!("Successfully proposed command {}", i);
                } else {
                    println!("Failed to propose command {}", i);
                }
            },
            Err(e) => {
                eprintln!("Error proposing command {}: {}", i, e);
            }
        }

        sleep(Duration::from_millis(300)).await;
    }

    println!("Testing other command types...");

    // Test workflow commands
    cluster.propose_workflow_start(100, b"workflow payload".to_vec()).await?;
    sleep(Duration::from_millis(200)).await;

    cluster.propose_checkpoint(100, "step1".to_string(), b"checkpoint data".to_vec()).await?;
    sleep(Duration::from_millis(200)).await;

    cluster.propose_workflow_end(100).await?;

    println!("All commands proposed. Waiting for final processing...");
    sleep(Duration::from_secs(2)).await;

    println!("Raftoral demo completed successfully!");

    Ok(())
}
