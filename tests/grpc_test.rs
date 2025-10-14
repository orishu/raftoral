use raftoral::raft::generic::grpc_transport::{GrpcClusterTransport, NodeConfig};
use raftoral::raft::generic::transport::ClusterTransport;
use raftoral::workflow::{WorkflowCommandExecutor, WorkflowRuntime};
use raftoral::grpc::start_grpc_server;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

/// Find two available TCP ports for testing
fn find_free_ports() -> (u16, u16) {
    let port1 = port_check::free_local_port().expect("Should find free port 1");
    let port2 = port_check::free_local_port().expect("Should find free port 2");
    (port1, port2)
}

// Disabled: test_grpc_message_passing - Uses old protobuf-specific API that has been replaced with direct protobuf serialization
// Modern message passing is tested in multi_node_test.rs
/*
#[tokio::test]
async fn test_grpc_message_passing() {
    // This test used the old convert_workflow_command_to_proto and RaftMessage APIs
    // which have been replaced with direct protobuf serialization via to_protobuf/from_protobuf
}
*/

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
    transport.start().await.expect("Should start transport");
    let cluster = transport.create_cluster(1).await.expect("Should create cluster");

    // Create workflow runtime
    let runtime = WorkflowRuntime::new(cluster.clone());

    // Start gRPC server
    let _server = start_grpc_server(
        address.clone(),
        transport.clone(),
        cluster,
        1,
        runtime
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
