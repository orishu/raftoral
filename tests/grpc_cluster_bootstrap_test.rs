use raftoral::raft::generic::grpc_transport::{GrpcClusterTransport, NodeConfig};
use raftoral::workflow::WorkflowContext;
use raftoral::grpc::{start_grpc_server, discover_peers};
use raftoral::nodemanager::NodeManager;
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

    let transport1 = Arc::new(GrpcClusterTransport::new(vec![
        NodeConfig { node_id: 1, address: addr1.clone() },
    ]));
    transport1.start().await.expect("Transport 1 should start");

    // Phase 3: Use NodeManager
    let node_manager1 = Arc::new(NodeManager::new(transport1.clone(), 1).await.expect("Should create node manager 1"));
    let cluster1 = node_manager1.workflow_cluster.clone();
    let runtime1 = node_manager1.workflow_runtime();

    let server1 = start_grpc_server(
        addr1.clone(),
        node_manager1.clone(),
        1,
    ).await.expect("Should start server 1");
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
    println!("  Discovered voters: {:?}", discovered[0].voters);

    let port2 = port_check::free_local_port().expect("Should find free port for node 2");
    let addr2 = format!("127.0.0.1:{}", port2);
    println!("  Node 2 address: {}", addr2);

    // IMPORTANT: Create transport with ONLY node 2 initially
    let transport2 = Arc::new(GrpcClusterTransport::new(vec![
        NodeConfig { node_id: 2, address: addr2.clone() },
    ]));
    transport2.start().await.expect("Transport 2 should start");

    // Set discovered voters from discovery
    transport2.set_discovered_voters(discovered[0].voters.clone());

    // Phase 3: Add node 1 to transport BEFORE creating cluster
    // This prevents node 2 from being detected as single-node and campaigning
    transport2.add_node(NodeConfig { node_id: 1, address: addr1.clone() }).await
        .expect("Should add node 1 to transport 2");

    // Give time for gRPC client to be created
    sleep(Duration::from_millis(100)).await;

    // Phase 3: Use NodeManager
    let node_manager2 = Arc::new(NodeManager::new(transport2.clone(), 2).await.expect("Should create node manager 2"));
    let cluster2 = node_manager2.workflow_cluster.clone();
    let runtime2 = node_manager2.workflow_runtime();

    let server2 = start_grpc_server(
        addr2.clone(),
        node_manager2.clone(),
        2,
    ).await.expect("Should start server 2");

    // Give server time to be fully ready
    sleep(Duration::from_millis(200)).await;

    // Add node 2 to the cluster via node 1 (the leader)
    // Node 1 will propose the ConfChange and send it to node 2
    println!("  Adding node 2 to cluster as voter via ConfChange...");
    cluster1.add_node(2, addr2.clone()).await
        .expect("Should add node 2 as voter");

    println!("✓ Node 2 added as voter\n");

    // Wait for node 2 to catch up
    sleep(Duration::from_millis(2000)).await;

    // Step 3: Add third node
    println!("Step 3: Adding third node (node 3)");

    // Discover both existing nodes
    let discovered = discover_peers(vec![addr1.clone(), addr2.clone()]).await;
    println!("  Discovered {} peer(s)", discovered.len());
    assert!(discovered.len() >= 1, "Should discover at least one node");
    println!("  Discovered voters from first peer: {:?}", discovered[0].voters);

    let port3 = port_check::free_local_port().expect("Should find free port for node 3");
    let addr3 = format!("127.0.0.1:{}", port3);
    println!("  Node 3 address: {}", addr3);

    // IMPORTANT: Create transport with ONLY node 3 initially
    let transport3 = Arc::new(GrpcClusterTransport::new(vec![
        NodeConfig { node_id: 3, address: addr3.clone() },
    ]));
    transport3.start().await.expect("Transport 3 should start");

    // Set discovered voters from discovery
    transport3.set_discovered_voters(discovered[0].voters.clone());

    // Add existing cluster members to transport BEFORE creating cluster
    // This prevents node 3 from being detected as single-node and campaigning
    transport3.add_node(NodeConfig { node_id: 1, address: addr1.clone() }).await
        .expect("Should add node 1 to transport 3");
    transport3.add_node(NodeConfig { node_id: 2, address: addr2.clone() }).await
        .expect("Should add node 2 to transport 3");

    // Give time for gRPC clients to be created
    sleep(Duration::from_millis(100)).await;

    // Phase 3: Use NodeManager
    let node_manager3 = Arc::new(NodeManager::new(transport3.clone(), 3).await.expect("Should create node manager 3"));
    let cluster3 = node_manager3.workflow_cluster.clone();
    let runtime3 = node_manager3.workflow_runtime();

    let server3 = start_grpc_server(
        addr3.clone(),
        node_manager3.clone(),
        3,
    ).await.expect("Should start server 3");

    // Give node 3's server a moment to be fully ready before adding to cluster
    sleep(Duration::from_millis(500)).await;

    // Add node 3 to the cluster via the existing cluster (node 1)
    // Note: add_node automatically routes to the leader
    println!("  Adding node 3 to cluster as voter via ConfChange...");
    cluster1.add_node(3, addr3.clone()).await
        .expect("Should add node 3 as voter");

    println!("✓ Node 3 added as voter\n");

    // Wait for node 3 to catch up
    sleep(Duration::from_millis(5000)).await;

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
