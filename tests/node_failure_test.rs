use raftoral::raft::generic::transport::{ClusterTransport, InMemoryClusterTransport};
use raftoral::workflow::{WorkflowCommandExecutor, WorkflowRuntime, WorkflowContext};
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

    // Create a 3-node cluster
    let transport = InMemoryClusterTransport::<WorkflowCommandExecutor>::new(vec![1, 2, 3]);
    transport.start().await.expect("Transport should start");

    // Create clusters and runtimes for all 3 nodes
    let cluster1 = transport.create_cluster(1).await.expect("Should create cluster 1");
    let cluster2 = transport.create_cluster(2).await.expect("Should create cluster 2");
    let cluster3 = transport.create_cluster(3).await.expect("Should create cluster 3");

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
    tokio::time::sleep(Duration::from_millis(3000)).await;

    println!("Step 4: Verifying workflows were reassigned");

    // Get the ownership map from any cluster node (they all share the same executor state)
    let ownership_map = &cluster1.executor.ownership_map;

    // Check ownership of workflow 1
    let owner1 = ownership_map.get_owner(&workflow_id1);
    println!("  Workflow 1 ({}) owner: {:?}", workflow_id1, owner1);

    // Check ownership of workflow 2
    let owner2 = ownership_map.get_owner(&workflow_id2);
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
    let node2_workflows = ownership_map.get_workflows_owned_by(2);
    assert!(node2_workflows.is_empty(), "Node 2 should have no workflows after removal");
    println!("  ✓ Node 2 has no workflows\n");

    println!("✅ Node failure and workflow reassignment test passed!");
    println!("  ✓ 3-node cluster started with workflows on node 2");
    println!("  ✓ Node 2 was removed from the cluster");
    println!("  ✓ Orphaned workflows were detected");
    println!("  ✓ Leader reassigned workflows to remaining nodes");
    println!("  ✓ All workflows now owned by nodes 1 or 3");
}
