use raftoral::{WorkflowCluster, RoleChange};
use tokio::time::{sleep, Duration};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Role Change Demo - Starting cluster...");

    let cluster = WorkflowCluster::new_single_node(1).await?;

    // Subscribe to role changes
    let mut role_changes = cluster.subscribe_role_changes();

    // Spawn a task to listen for role changes
    let role_listener = tokio::spawn(async move {
        while let Ok(role_change) = role_changes.recv().await {
            match role_change {
                RoleChange::BecameLeader(node_id) => {
                    println!("🎉 Node {} became LEADER!", node_id);
                },
                RoleChange::BecameFollower(node_id) => {
                    println!("👥 Node {} became FOLLOWER", node_id);
                },
                RoleChange::BecameCandidate(node_id) => {
                    println!("🗳️  Node {} became CANDIDATE", node_id);
                },
            }
        }
    });

    // Give the cluster time to initialize and establish leadership
    sleep(Duration::from_millis(500)).await;

    // Check leadership status
    if cluster.is_leader().await {
        println!("✅ This node is currently the leader");
    } else {
        println!("❌ This node is not the leader");
    }

    println!("Role change demo completed!");

    // Stop the role listener
    role_listener.abort();

    Ok(())
}