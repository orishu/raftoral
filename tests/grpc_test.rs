use raftoral::raft::generic::grpc_transport::{GrpcClusterTransport, NodeConfig};
use raftoral::raft::generic::transport::ClusterTransport;
use raftoral::workflow::WorkflowCommandExecutor;
use raftoral::grpc::{start_grpc_server, RaftClient};
use raftoral::grpc::server::convert_workflow_command_to_proto;
use raftoral::grpc::server::raft_proto::RaftMessage;
use raftoral::workflow::commands::{WorkflowCommand, WorkflowStartData};
use std::sync::Arc;
use tokio::time::{sleep, Duration};

/// Find two available TCP ports for testing
fn find_free_ports() -> (u16, u16) {
    let port1 = port_check::free_local_port().expect("Should find free port 1");
    let port2 = port_check::free_local_port().expect("Should find free port 2");
    (port1, port2)
}

#[tokio::test]
async fn test_grpc_message_passing() {
    // Find two free ports
    let (port1, port2) = find_free_ports();

    let node1_addr = format!("127.0.0.1:{}", port1);
    let node2_addr = format!("127.0.0.1:{}", port2);

    // Create transport with two nodes
    let nodes = vec![
        NodeConfig { node_id: 1, address: node1_addr.clone() },
        NodeConfig { node_id: 2, address: node2_addr.clone() },
    ];

    let transport = Arc::new(GrpcClusterTransport::<WorkflowCommandExecutor>::new(nodes));

    // Create clusters
    let cluster1 = transport.create_cluster(1).await.expect("Should create cluster 1");
    let cluster2 = transport.create_cluster(2).await.expect("Should create cluster 2");

    // Start gRPC servers for both nodes in background
    let _server1 = start_grpc_server(
        node1_addr.clone(),
        transport.clone(),
        cluster1,
        1
    ).await.expect("Should start server 1");

    let _server2 = start_grpc_server(
        node2_addr.clone(),
        transport.clone(),
        cluster2,
        2
    ).await.expect("Should start server 2");

    // Give servers time to start
    sleep(Duration::from_millis(500)).await;

    // Create a gRPC client to send message from node 1 to node 2
    let mut client = RaftClient::connect(node2_addr.clone()).await
        .expect("Should connect to node 2");

    // Create a test workflow command
    let workflow_cmd = WorkflowCommand::WorkflowStart(WorkflowStartData {
        workflow_id: "test-workflow".to_string(),
        workflow_type: "test-type".to_string(),
        version: 1,
        input: vec![1, 2, 3, 4],
        owner_node_id: 1,
    });

    // Convert to proto format
    let proto_cmd = convert_workflow_command_to_proto(&workflow_cmd);

    // Create a Raft message (we'll send an empty one for this test)
    let raft_msg = RaftMessage {
        raft_message: vec![],  // Empty for this basic test
        command: Some(proto_cmd),
    };

    // Send the message
    let result = client.send_raft_message(raft_msg).await;

    // Verify the message was sent successfully
    assert!(result.is_ok(), "Should successfully send message: {:?}", result.err());

    println!("✓ Successfully sent gRPC message from node 1 to node 2");
    println!("✓ Message contained WorkflowStart command");
}

#[tokio::test]
async fn test_grpc_transport_node_management() {
    let (port1, _) = find_free_ports();

    let node1_addr = format!("127.0.0.1:{}", port1);

    // Create transport with one node
    let nodes = vec![
        NodeConfig { node_id: 1, address: node1_addr.clone() },
    ];

    let transport = GrpcClusterTransport::<WorkflowCommandExecutor>::new(nodes);

    // Verify initial node
    assert_eq!(
        transport.get_node_address(1).await,
        Some(node1_addr.clone())
    );

    // Add a second node dynamically
    let port2 = port_check::free_local_port().expect("Should find free port");
    let node2_addr = format!("127.0.0.1:{}", port2);

    let node2 = NodeConfig { node_id: 2, address: node2_addr.clone() };
    transport.add_node(node2).await.expect("Should add node 2");

    assert_eq!(
        transport.get_node_address(2).await,
        Some(node2_addr.clone())
    );

    // Remove node 1
    transport.remove_node(1).await.expect("Should remove node 1");
    assert_eq!(transport.get_node_address(1).await, None);

    // Node 2 should still be there
    assert_eq!(
        transport.get_node_address(2).await,
        Some(node2_addr.clone())
    );

    println!("✓ Successfully added and removed nodes from transport");
}

#[tokio::test]
async fn test_grpc_transport_node_ids() {
    let nodes = vec![
        NodeConfig { node_id: 1, address: "127.0.0.1:5001".to_string() },
        NodeConfig { node_id: 3, address: "127.0.0.1:5003".to_string() },
        NodeConfig { node_id: 2, address: "127.0.0.1:5002".to_string() },
    ];

    let transport = GrpcClusterTransport::<WorkflowCommandExecutor>::new(nodes);

    // Get node IDs - should work now with async method
    let mut node_ids = transport.node_ids().await;
    node_ids.sort(); // Sort since HashMap doesn't guarantee order

    assert_eq!(node_ids, vec![1, 2, 3]);
    println!("✓ Successfully retrieved node IDs from gRPC transport");
}

#[tokio::test]
async fn test_discovery_rpc() {
    use raftoral::grpc::bootstrap::{discover_peer, RaftRole};

    let port = port_check::free_local_port().expect("Should find free port");
    let address = format!("127.0.0.1:{}", port);

    // Create a simple transport with one node
    let nodes = vec![
        NodeConfig { node_id: 1, address: address.clone() },
    ];

    let transport = Arc::new(GrpcClusterTransport::<WorkflowCommandExecutor>::new(nodes));
    let cluster = transport.create_cluster(1).await.expect("Should create cluster");

    // Start gRPC server
    let _server = start_grpc_server(
        address.clone(),
        transport.clone(),
        cluster,
        1
    ).await.expect("Should start server");

    // Give server time to start
    sleep(Duration::from_millis(500)).await;

    // Discover the peer
    let discovered = discover_peer(&address).await.expect("Should discover peer");

    assert_eq!(discovered.node_id, 1);
    assert_eq!(discovered.address, address);
    assert_eq!(discovered.highest_known_node_id, 1);
    // Role should be either Follower or Leader
    assert!(
        discovered.role == RaftRole::Follower || discovered.role == RaftRole::Leader,
        "Role should be Follower or Leader, got {:?}",
        discovered.role
    );

    println!("✓ Successfully discovered peer: node_id={}, role={:?}", discovered.node_id, discovered.role);
}
