use raftoral::raft::generic::grpc_transport::{GrpcClusterTransport, NodeConfig};
use raftoral::workflow::{WorkflowCommandExecutor, WorkflowCommand};
use std::sync::Arc;
use tokio::time::{sleep, Duration};

#[tokio::test]
async fn test_snapshot_based_bootstrap() {
    println!("=== Testing Snapshot-Based Bootstrap ===\n");

    // Step 1: Bootstrap node 1
    println!("Step 1: Bootstrapping node 1");
    let port1 = port_check::free_local_port().expect("Should find free port for node 1");
    let addr1 = format!("127.0.0.1:{}", port1);

    let transport1 = Arc::new(GrpcClusterTransport::new(vec![
        NodeConfig { node_id: 1, address: addr1.clone() },
    ]));
    transport1.start().await.expect("Transport 1 should start");

    // Phase 3: Use NodeManager pattern
    let receiver1 = transport1.extract_typed_receiver::<WorkflowCommand>(1).expect("Should extract receiver");
    let executor1 = WorkflowCommandExecutor::default();
    let transport_ref1: Arc<dyn raftoral::raft::generic::transport::TransportInteraction<raftoral::raft::generic::message::Message<WorkflowCommand>>> = transport1.clone();
    let cluster1 = Arc::new(raftoral::raft::RaftCluster::new(1, receiver1, transport_ref1, executor1).await.expect("Should create cluster"));

    // Step 2: Add MANY dummy entries to force snapshot threshold
    println!("Step 2: Adding 1500 dummy entries to trigger snapshot");
    use raftoral::workflow::CheckpointData;

    for i in 0..1500 {
        let dummy_checkpoint = WorkflowCommand::SetCheckpoint(CheckpointData {
            workflow_id: format!("dummy_workflow_{}", i),
            key: format!("key_{}", i),
            value: vec![i as u8],
        });

        if let Err(e) = cluster1.propose_command(dummy_checkpoint).await {
            eprintln!("Failed to propose entry {}: {}", i, e);
        }

        // Log progress every 100 entries
        if i % 100 == 0 {
            println!("  Added {} entries", i);
        }
    }

    println!("✓ Added 1500 dummy entries");

    // Give time for commits
    sleep(Duration::from_millis(2000)).await;

    // Step 3: Add node 2 as learner
    println!("\nStep 3: Adding node 2 as learner");
    let port2 = port_check::free_local_port().expect("Should find free port for node 2");
    let addr2 = format!("127.0.0.1:{}", port2);

    let transport2 = Arc::new(GrpcClusterTransport::new(vec![
        NodeConfig { node_id: 2, address: addr2.clone() },
    ]));
    transport2.start().await.expect("Transport 2 should start");

    // Add node 1 to transport before creating cluster
    transport2.add_node(NodeConfig { node_id: 1, address: addr1.clone() }).await
        .expect("Should add node 1 to transport 2");

    sleep(Duration::from_millis(100)).await;

    // Phase 3: Use NodeManager pattern
    let receiver2 = transport2.extract_typed_receiver::<WorkflowCommand>(2).expect("Should extract receiver");
    let executor2 = WorkflowCommandExecutor::default();
    let transport_ref2: Arc<dyn raftoral::raft::generic::transport::TransportInteraction<raftoral::raft::generic::message::Message<WorkflowCommand>>> = transport2.clone();
    let cluster2 = Arc::new(raftoral::raft::RaftCluster::new(2, receiver2, transport_ref2, executor2).await.expect("Should create cluster"));

    // Add node 2 to cluster via node 1
    println!("  Adding node 2 as learner via ConfChange...");
    cluster1.add_node(2, addr2.clone()).await
        .expect("Should add node 2 as learner");

    println!("✓ Node 2 added as learner");

    // Step 4: Wait and check if snapshot was sent
    println!("\nStep 4: Waiting for log replication/snapshot (10 seconds)...");
    sleep(Duration::from_millis(10000)).await;

    // Check configurations
    let node_ids_1 = cluster1.get_node_ids();
    let node_ids_2 = cluster2.get_node_ids();

    println!("  Node 1 sees: {:?}", node_ids_1);
    println!("  Node 2 sees: {:?}", node_ids_2);

    if node_ids_2.is_empty() {
        println!("\n❌ Node 2 still has empty config - snapshot approach also failed");
    } else {
        println!("\n✓ Node 2 has received configuration! Snapshot approach worked!");
    }

    println!("\n=== Test Complete ===");
}
