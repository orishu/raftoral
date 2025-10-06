use raftoral::raft::generic::grpc_transport::{GrpcClusterTransport, NodeConfig};
use raftoral::raft::generic::transport::ClusterTransport;
use raftoral::workflow::{WorkflowCommandExecutor, WorkflowRuntime, WorkflowContext};
use raftoral::grpc::{start_grpc_server, discover_peers};
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ConcatInput {
    strings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ConcatOutput {
    result: String,
}

#[tokio::test]
async fn test_three_node_grpc_cluster_bootstrap() {
    println!("=== Three-Node gRPC Cluster Bootstrap Test ===\n");

    // Step 1: Bootstrap first node
    println!("Step 1: Bootstrapping first node (node 1)");

    let port1 = port_check::free_local_port().expect("Should find free port for node 1");
    let addr1 = format!("127.0.0.1:{}", port1);
    println!("  Node 1 address: {}", addr1);

    let transport1 = Arc::new(GrpcClusterTransport::<WorkflowCommandExecutor>::new(vec![
        NodeConfig { node_id: 1, address: addr1.clone() },
    ]));
    transport1.start().await.expect("Transport 1 should start");

    let cluster1 = transport1.create_cluster(1).await.expect("Should create cluster 1");

    let server1 = start_grpc_server(
        addr1.clone(),
        transport1.clone(),
        cluster1.clone(),
        1
    ).await.expect("Should start server 1");

    let runtime1 = WorkflowRuntime::new(cluster1.clone());
    println!("✓ Node 1 bootstrapped and running\n");

    // Give node 1 time to become leader
    sleep(Duration::from_millis(1000)).await;

    // Step 2: Add second node
    println!("Step 2: Adding second node (node 2)");

    // Discover the first node
    let discovered = discover_peers(vec![addr1.clone()]).await;
    println!("  Discovered {} peer(s)", discovered.len());
    assert_eq!(discovered.len(), 1, "Should discover node 1");
    assert_eq!(discovered[0].node_id, 1, "Discovered node should be node 1");

    let port2 = port_check::free_local_port().expect("Should find free port for node 2");
    let addr2 = format!("127.0.0.1:{}", port2);
    println!("  Node 2 address: {}", addr2);

    // Create transport with node 2 AND node 1 (discovered peer)
    // This ensures node 2's Raft instance knows about node 1 from the start
    let transport2 = Arc::new(GrpcClusterTransport::<WorkflowCommandExecutor>::new(vec![
        NodeConfig { node_id: 1, address: addr1.clone() },
        NodeConfig { node_id: 2, address: addr2.clone() },
    ]));
    transport2.start().await.expect("Transport 2 should start");

    let cluster2 = transport2.create_cluster(2).await.expect("Should create cluster 2");

    let server2 = start_grpc_server(
        addr2.clone(),
        transport2.clone(),
        cluster2.clone(),
        2
    ).await.expect("Should start server 2");

    let runtime2 = WorkflowRuntime::new(cluster2.clone());

    // Add node 2 to the cluster via node 1 (the leader)
    println!("  Adding node 2 to cluster via ConfChange...");
    cluster1.add_node(2, addr2.clone()).await
        .expect("Should add node 2 to cluster");

    println!("✓ Node 2 added to cluster\n");

    // Give time for the ConfChange to propagate and nodes to sync
    sleep(Duration::from_millis(2000)).await;

    // Step 3: Add third node
    println!("Step 3: Adding third node (node 3)");

    // Discover both existing nodes
    let discovered = discover_peers(vec![addr1.clone(), addr2.clone()]).await;
    println!("  Discovered {} peer(s)", discovered.len());
    assert!(discovered.len() >= 1, "Should discover at least one node");

    let port3 = port_check::free_local_port().expect("Should find free port for node 3");
    let addr3 = format!("127.0.0.1:{}", port3);
    println!("  Node 3 address: {}", addr3);

    // Create transport with node 3 AND the discovered peers
    // This ensures node 3's Raft instance knows about all nodes from the start
    let transport3 = Arc::new(GrpcClusterTransport::<WorkflowCommandExecutor>::new(vec![
        NodeConfig { node_id: 1, address: addr1.clone() },
        NodeConfig { node_id: 2, address: addr2.clone() },
        NodeConfig { node_id: 3, address: addr3.clone() },
    ]));
    transport3.start().await.expect("Transport 3 should start");

    let cluster3 = transport3.create_cluster(3).await.expect("Should create cluster 3");

    let server3 = start_grpc_server(
        addr3.clone(),
        transport3.clone(),
        cluster3.clone(),
        3
    ).await.expect("Should start server 3");

    let runtime3 = WorkflowRuntime::new(cluster3.clone());

    // Add node 3 to the cluster
    // Note: add_node automatically routes to the leader, so we can call it from any node
    println!("  Adding node 3 to cluster via ConfChange...");
    cluster1.add_node(3, addr3.clone()).await
        .expect("Should add node 3 to cluster");

    println!("✓ Node 3 added to cluster\n");

    // Give time for the ConfChange to propagate and nodes to sync
    sleep(Duration::from_millis(2000)).await;

    // Step 4: Verify cluster membership
    println!("Step 4: Verifying cluster membership");
    let node_ids_1 = cluster1.get_node_ids();
    let node_ids_2 = cluster2.get_node_ids();
    let node_ids_3 = cluster3.get_node_ids();

    println!("  Node 1 sees: {:?}", node_ids_1);
    println!("  Node 2 sees: {:?}", node_ids_2);
    println!("  Node 3 sees: {:?}", node_ids_3);

    // Verify all nodes see the same 3-node cluster
    assert_eq!(node_ids_1.len(), 3, "Node 1 should see 3 nodes");
    assert_eq!(node_ids_2.len(), 3, "Node 2 should see 3 nodes");
    assert_eq!(node_ids_3.len(), 3, "Node 3 should see 3 nodes");

    // Verify they all see the same set of nodes (order may differ)
    let mut sorted_1 = node_ids_1.clone();
    let mut sorted_2 = node_ids_2.clone();
    let mut sorted_3 = node_ids_3.clone();
    sorted_1.sort();
    sorted_2.sort();
    sorted_3.sort();
    assert_eq!(sorted_1, vec![1, 2, 3], "Node 1 should see nodes 1, 2, 3");
    assert_eq!(sorted_2, vec![1, 2, 3], "Node 2 should see nodes 1, 2, 3");
    assert_eq!(sorted_3, vec![1, 2, 3], "Node 3 should see nodes 1, 2, 3");

    println!("✓ All nodes see the same 3-node cluster\n");

    // Step 5: Register workflow on all nodes
    println!("Step 5: Registering string concatenation workflow on all nodes");

    let concat_fn = |input: ConcatInput, _context: WorkflowContext| async move {
        let result = input.strings.join("");
        Ok::<ConcatOutput, raftoral::WorkflowError>(ConcatOutput { result })
    };

    runtime1.register_workflow_closure("concat", 1, concat_fn.clone())
        .expect("Should register workflow on runtime 1");
    runtime2.register_workflow_closure("concat", 1, concat_fn.clone())
        .expect("Should register workflow on runtime 2");
    runtime3.register_workflow_closure("concat", 1, concat_fn)
        .expect("Should register workflow on runtime 3");

    println!("✓ Workflow registered on all 3 nodes\n");

    // Step 6: Execute workflow from node 2
    println!("Step 6: Starting workflow from node 2");

    let input = ConcatInput {
        strings: vec!["hello".to_string(), " world!".to_string()],
    };

    let workflow_run = runtime2.start_workflow::<ConcatInput, ConcatOutput>(
        "concat",
        1,
        input.clone()
    ).await.expect("Should start workflow");

    let workflow_id = workflow_run.workflow_id().to_string();
    println!("  Workflow started: {}", workflow_id);
    println!("  Input: {:?}", input.strings);

    // Wait for workflow completion
    println!("  Waiting for workflow completion...");
    let result = workflow_run.wait_for_completion().await
        .expect("Workflow should complete successfully");

    println!("  Result: {}", result.result);

    // Step 7: Verify result
    println!("\nStep 7: Verifying result");
    assert_eq!(result.result, "hello world!", "Should concatenate strings correctly");

    println!("✓ Result verified: '{}'", result.result);

    // Step 8: Verify workflow state on all nodes
    println!("\nStep 8: Verifying workflow state consistency across all nodes");

    // Give a moment for all nodes to apply the WorkflowEnd command
    sleep(Duration::from_millis(500)).await;

    let status1 = cluster1.executor.get_workflow_status(&workflow_id);
    let status2 = cluster2.executor.get_workflow_status(&workflow_id);
    let status3 = cluster3.executor.get_workflow_status(&workflow_id);

    println!("  Node 1 status: {:?}", status1);
    println!("  Node 2 status: {:?}", status2);
    println!("  Node 3 status: {:?}", status3);

    use raftoral::workflow::WorkflowStatus;
    assert_eq!(status1, Some(WorkflowStatus::Completed), "Node 1 should see workflow as completed");
    assert_eq!(status2, Some(WorkflowStatus::Completed), "Node 2 should see workflow as completed");
    assert_eq!(status3, Some(WorkflowStatus::Completed), "Node 3 should see workflow as completed");

    println!("✓ All nodes see workflow as completed");

    println!("\n=== Test Passed! ===");
    println!("Successfully bootstrapped a 3-node gRPC cluster and executed a workflow");

    // Keep servers alive until end of test
    drop(server1);
    drop(server2);
    drop(server3);
}
