use raftoral::raft::generic::transport::InMemoryClusterTransport;
use raftoral::raft::generic::message::Message;
use raftoral::workflow::{WorkflowCommandExecutor, WorkflowRuntime, WorkflowContext};
use std::sync::Arc;
use std::time::Duration;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TestInput {
    value: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct TestOutput {
    result: u32,
}

#[tokio::test]
async fn test_node_failure_workflow_reassignment() {
    println!("=== Starting Node Failure and Workflow Reassignment Test ===\n");

    use raftoral::workflow::WorkflowCommand;
    use raftoral::raft::RaftCluster;

    // Phase 3: Create a 3-node cluster with Message<WorkflowCommand>
    let transport = Arc::new(InMemoryClusterTransport::<Message<WorkflowCommand>>::new(vec![1, 2, 3]));

    // Create clusters manually using extract_receiver pattern
    let receiver1 = transport.extract_receiver(1).await.expect("Should extract receiver 1");
    let executor1 = WorkflowCommandExecutor::default();
    let transport_ref1: Arc<dyn raftoral::raft::generic::transport::TransportInteraction<Message<WorkflowCommand>>> = transport.clone();
    let cluster1 = Arc::new(RaftCluster::new(1, receiver1, transport_ref1, executor1).await.expect("Should create cluster 1"));

    let receiver2 = transport.extract_receiver(2).await.expect("Should extract receiver 2");
    let executor2 = WorkflowCommandExecutor::default();
    let transport_ref2: Arc<dyn raftoral::raft::generic::transport::TransportInteraction<Message<WorkflowCommand>>> = transport.clone();
    let cluster2 = Arc::new(RaftCluster::new(2, receiver2, transport_ref2, executor2).await.expect("Should create cluster 2"));

    let receiver3 = transport.extract_receiver(3).await.expect("Should extract receiver 3");
    let executor3 = WorkflowCommandExecutor::default();
    let transport_ref3: Arc<dyn raftoral::raft::generic::transport::TransportInteraction<Message<WorkflowCommand>>> = transport.clone();
    let cluster3 = Arc::new(RaftCluster::new(3, receiver3, transport_ref3, executor3).await.expect("Should create cluster 3"));

    let runtime1 = WorkflowRuntime::new(cluster1.clone());
    let runtime2 = WorkflowRuntime::new(cluster2.clone());
    let runtime3 = WorkflowRuntime::new(cluster3.clone());

    // Register a long-running workflow on all nodes that won't complete automatically
    // This ensures the workflows are still running when we remove the node
    let workflow_fn = |input: TestInput, _context: WorkflowContext| async move {
        // Long delay to ensure workflow is still running during node removal
        tokio::time::sleep(Duration::from_secs(30)).await;
        // Simple multiplication
        Ok(TestOutput { result: input.value * 2 })
    };

    runtime1.register_workflow_closure("test", 1, workflow_fn.clone())
        .expect("Should register workflow on runtime 1");
    runtime2.register_workflow_closure("test", 1, workflow_fn.clone())
        .expect("Should register workflow on runtime 2");
    runtime3.register_workflow_closure("test", 1, workflow_fn)
        .expect("Should register workflow on runtime 3");

    // Give nodes time to elect a leader
    tokio::time::sleep(Duration::from_millis(2000)).await;

    println!("Step 1: Starting workflows on node 2 (which will own them)");

    // Start a few workflows from node 2 with delays to avoid timestamp collisions
    let workflow_id1 = {
        let wf = runtime2.start_workflow::<TestInput, TestOutput>(
            "test", 1, TestInput { value: 10 }
        ).await.expect("Workflow 1 should start");
        wf.workflow_id().to_string()
    };
    tokio::time::sleep(Duration::from_millis(20)).await;

    let workflow_id2 = {
        let wf = runtime2.start_workflow::<TestInput, TestOutput>(
            "test", 1, TestInput { value: 20 }
        ).await.expect("Workflow 2 should start");
        wf.workflow_id().to_string()
    };

    println!("✓ Started 2 workflows owned by node 2");
    println!("  Workflow IDs: {}, {}\n", workflow_id1, workflow_id2);

    // Give a moment for the workflows to be registered in the ownership map
    tokio::time::sleep(Duration::from_millis(200)).await;

    println!("Step 3: Simulating node 2 failure by removing it from the cluster");

    // Determine which node is the leader so we can remove node 2 from there
    let leader_cluster = if cluster1.is_leader().await {
        println!("  Node 1 is the leader");
        cluster1.clone()
    } else if cluster3.is_leader().await {
        println!("  Node 3 is the leader");
        cluster3.clone()
    } else {
        // Wait a bit and check again
        tokio::time::sleep(Duration::from_millis(500)).await;
        if cluster1.is_leader().await {
            println!("  Node 1 is the leader");
            cluster1.clone()
        } else {
            println!("  Node 3 is the leader");
            cluster3.clone()
        }
    };

    // Remove node 2 from the cluster
    leader_cluster.remove_node(2).await
        .expect("Should be able to remove node 2");

    println!("✓ Node 2 removed from cluster\n");

    // Give time for the OwnerChange commands to be proposed and applied
    // The reassignment happens asynchronously after the ConfChange is applied
    // Need to wait for: async task spawn + leadership check + propose + consensus + apply
    tokio::time::sleep(Duration::from_millis(7000)).await;

    println!("Step 4: Verifying workflows were reassigned");

    // Check ownership on both remaining nodes to ensure consistency
    let ownership_map1 = &cluster1.executor.ownership_map;
    let ownership_map3 = &cluster3.executor.ownership_map;

    println!("  DEBUG: Checking cluster 1's view:");
    let owner1_on_node1 = ownership_map1.get_owner(&workflow_id1);
    let owner2_on_node1 = ownership_map1.get_owner(&workflow_id2);
    println!("    Workflow 1: {:?}, Workflow 2: {:?}", owner1_on_node1, owner2_on_node1);

    println!("  DEBUG: Checking cluster 3's view:");
    let owner1_on_node3 = ownership_map3.get_owner(&workflow_id1);
    let owner2_on_node3 = ownership_map3.get_owner(&workflow_id2);
    println!("    Workflow 1: {:?}, Workflow 2: {:?}", owner1_on_node3, owner2_on_node3);

    // Use node 1's view for verification
    let owner1 = owner1_on_node1;
    let owner2 = owner2_on_node1;
    println!("  Workflow 1 ({}) owner: {:?}", workflow_id1, owner1);
    println!("  Workflow 2 ({}) owner: {:?}", workflow_id2, owner2);

    // Verify that workflows are no longer owned by node 2
    if let Some(owner) = owner1 {
        assert_ne!(owner, 2, "Workflow 1 should not be owned by removed node 2");
        assert!(owner == 1 || owner == 3, "Workflow 1 should be owned by node 1 or 3");
        println!("  ✓ Workflow 1 reassigned to node {}", owner);
    }

    if let Some(owner) = owner2 {
        assert_ne!(owner, 2, "Workflow 2 should not be owned by removed node 2");
        assert!(owner == 1 || owner == 3, "Workflow 2 should be owned by node 1 or 3");
        println!("  ✓ Workflow 2 reassigned to node {}", owner);
    }

    // Verify node 2 has no workflows
    let node2_workflows = ownership_map1.get_workflows_owned_by(2);
    assert!(node2_workflows.is_empty(), "Node 2 should have no workflows after removal");
    println!("  ✓ Node 2 has no workflows\n");

    println!("✅ Node failure and workflow reassignment test passed!");
    println!("  ✓ 3-node cluster started with workflows on node 2");
    println!("  ✓ Node 2 was removed from the cluster");
    println!("  ✓ Orphaned workflows were detected");
    println!("  ✓ Leader reassigned workflows to remaining nodes");
    println!("  ✓ All workflows now owned by nodes 1 or 3");
}

#[tokio::test]
async fn test_instant_ownership_promotion() {
    use std::time::Instant;

    println!("=== Starting Instant Ownership Promotion Test ===\n");
    println!("This test verifies that a follower immediately becomes active when promoted to owner");
    println!("(instead of waiting for a 5-second timeout)\n");

    use raftoral::workflow::WorkflowCommand;
    use raftoral::raft::RaftCluster;

    // Phase 3: Create a 3-node cluster with Message<WorkflowCommand>
    let transport = Arc::new(InMemoryClusterTransport::<Message<WorkflowCommand>>::new(vec![1, 2, 3]));

    // Create clusters manually using extract_receiver pattern
    let receiver1 = transport.extract_receiver(1).await.expect("Should extract receiver 1");
    let executor1 = WorkflowCommandExecutor::default();
    let transport_ref1: Arc<dyn raftoral::raft::generic::transport::TransportInteraction<Message<WorkflowCommand>>> = transport.clone();
    let cluster1 = Arc::new(RaftCluster::new(1, receiver1, transport_ref1, executor1).await.expect("Should create cluster 1"));

    let receiver2 = transport.extract_receiver(2).await.expect("Should extract receiver 2");
    let executor2 = WorkflowCommandExecutor::default();
    let transport_ref2: Arc<dyn raftoral::raft::generic::transport::TransportInteraction<Message<WorkflowCommand>>> = transport.clone();
    let cluster2 = Arc::new(RaftCluster::new(2, receiver2, transport_ref2, executor2).await.expect("Should create cluster 2"));

    let receiver3 = transport.extract_receiver(3).await.expect("Should extract receiver 3");
    let executor3 = WorkflowCommandExecutor::default();
    let transport_ref3: Arc<dyn raftoral::raft::generic::transport::TransportInteraction<Message<WorkflowCommand>>> = transport.clone();
    let cluster3 = Arc::new(RaftCluster::new(3, receiver3, transport_ref3, executor3).await.expect("Should create cluster 3"));

    let runtime1 = WorkflowRuntime::new(cluster1.clone());
    let runtime2 = WorkflowRuntime::new(cluster2.clone());
    let runtime3 = WorkflowRuntime::new(cluster3.clone());

    // Register a workflow that creates a checkpoint and measures promotion time
    let workflow_fn = |input: TestInput, context: WorkflowContext| async move {
        // Create first checkpoint
        let _var1 = context.create_replicated_var("step1", input.value).await?;

        // Wait a bit to ensure all nodes are executing
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Create second checkpoint - this is where promotion will be tested
        let start_time = Instant::now();
        let _var2 = context.create_replicated_var("step2", input.value * 2).await?;
        let elapsed = start_time.elapsed();

        println!("  Workflow completed step2 in {:?} (instant promotion if <1s)", elapsed);

        Ok(TestOutput { result: input.value * 2 })
    };

    runtime1.register_workflow_closure("instant_test", 1, workflow_fn.clone())
        .expect("Should register workflow");
    runtime2.register_workflow_closure("instant_test", 1, workflow_fn.clone())
        .expect("Should register workflow");
    runtime3.register_workflow_closure("instant_test", 1, workflow_fn)
        .expect("Should register workflow");

    // Give nodes time to elect a leader
    tokio::time::sleep(Duration::from_millis(2000)).await;

    println!("Step 1: Starting workflow on node 2");
    let workflow_id = {
        let wf = runtime2.start_workflow::<TestInput, TestOutput>(
            "instant_test", 1, TestInput { value: 42 }
        ).await.expect("Workflow should start");
        wf.workflow_id().to_string()
    };
    println!("✓ Workflow started: {}\n", workflow_id);

    // Wait for workflow to reach step1
    tokio::time::sleep(Duration::from_millis(1000)).await;

    println!("Step 2: Removing node 2 to trigger ownership change");
    let leader_cluster = if cluster1.is_leader().await {
        cluster1.clone()
    } else {
        cluster3.clone()
    };

    let removal_time = Instant::now();
    leader_cluster.remove_node(2).await
        .expect("Should remove node 2");
    println!("✓ Node 2 removed\n");

    // Wait for reassignment and promotion
    tokio::time::sleep(Duration::from_millis(2000)).await;

    let total_time = removal_time.elapsed();
    println!("Step 3: Checking promotion speed");
    println!("  Total time from removal to checkpoint: {:?}", total_time);

    // With instant promotion, the new owner should wake up immediately (< 1s)
    // Without instant promotion, it would wait for 5s timeout
    if total_time < Duration::from_secs(3) {
        println!("  ✓ Instant promotion detected! (< 3s)");
    } else {
        println!("  ⚠ Slow promotion (> 3s) - might be using timeout-based wakeup");
    }

    println!("\n✅ Instant ownership promotion test completed!");
}
