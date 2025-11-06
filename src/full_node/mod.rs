//! Full Node - Complete stack from Layer 0 (gRPC) to Layer 7 (ManagementRuntime)
//!
//! This module provides a complete Raft node implementation that includes:
//! - Layer 0: gRPC Server
//! - Layer 1: Transport
//! - Layer 2: Cluster Router
//! - Layer 3-4: Raft Node with State Machine
//! - Layer 5: Event Bus
//! - Layer 6: Proposal Router
//! - Layer 7: Application Runtime (ManagementRuntime)

use crate::grpc2::{GrpcMessageSender, GrpcServer};
use crate::management::ManagementRuntime;
use crate::raft::generic2::{
    ClusterRouter, RaftNode, RaftNodeConfig, Transport, TransportLayer,
};
use crate::workflow2::WorkflowRuntime;
use slog::{info, Logger};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tonic::transport::Server;

/// A complete Raft node with all layers from gRPC to ManagementRuntime
///
/// The FullNode manages a management cluster (cluster_id=0) that tracks metadata
/// about workflow execution clusters. When sub-clusters are created via the management
/// runtime, FullNode automatically creates and manages WorkflowRuntime instances.
pub struct FullNode {
    /// Management runtime (Layer 7) - manages WorkflowRuntime sub-clusters
    runtime: Arc<ManagementRuntime<WorkflowRuntime>>,

    /// Raft node handle
    node: Arc<Mutex<RaftNode<crate::management::ManagementStateMachine>>>,

    /// gRPC server handle
    grpc_server_handle: Option<JoinHandle<Result<(), tonic::transport::Error>>>,

    /// Node address
    address: String,

    /// Logger
    logger: Logger,
}

impl FullNode {
    /// Create and start a new full node in bootstrap mode (single-node cluster)
    ///
    /// # Arguments
    /// * `node_id` - Unique identifier for this node
    /// * `address` - Network address to bind to (e.g., "127.0.0.1:50051")
    /// * `logger` - Logger instance
    ///
    /// # Returns
    /// A running FullNode instance
    pub async fn new(
        node_id: u64,
        address: String,
        logger: Logger,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        info!(logger, "Creating FullNode"; "node_id" => node_id, "address" => &address);

        // Layer 1: Create transport with gRPC message sender
        let grpc_sender = Arc::new(GrpcMessageSender::new());
        let transport = Arc::new(TransportLayer::new(grpc_sender));

        // Create mailbox for this node
        let (mailbox_tx, mailbox_rx) = mpsc::channel(1000);

        // Layer 2: Create cluster router
        let cluster_router = Arc::new(ClusterRouter::new());

        // Register this node's cluster (cluster_id = 0 for management)
        cluster_router.register_cluster(0, mailbox_tx).await;

        // Layers 3-7: Create management runtime (includes RaftNode, EventBus, ProposalRouter)
        let config = RaftNodeConfig {
            node_id,
            cluster_id: 0, // Management cluster ID
            snapshot_interval: 100, // Take snapshot every 100 entries
            ..Default::default()
        };

        let (runtime, node) = ManagementRuntime::new(
            config,
            transport.clone(),
            mailbox_rx,
            cluster_router.clone(),
            logger.clone(),
        )?;

        // Run Raft node in background
        let node_clone = node.clone();
        tokio::spawn(async move {
            info!(slog::Logger::root(slog::Discard, slog::o!()), "Starting Raft node event loop");
            let _ = RaftNode::run_from_arc(node_clone).await;
        });

        // Campaign to become leader (for single-node cluster)
        node.lock().await.campaign().await?;

        // Layer 0: Start gRPC server
        let grpc_server = Arc::new(GrpcServer::new(
            cluster_router,
            node_id,
            address.clone(),
        ));

        // Start leader tracker
        grpc_server.start_leader_tracker(node.clone(), transport.clone());

        let addr = address.parse()?;

        info!(logger, "Starting gRPC server"; "address" => &address);

        let grpc_server_clone = grpc_server.clone();
        let grpc_server_handle = tokio::spawn(async move {
            Server::builder()
                .add_service(
                    crate::grpc::server::raft_proto::raft_service_server::RaftServiceServer::new(
                        (*grpc_server_clone).clone(),
                    ),
                )
                .serve(addr)
                .await
        });

        info!(logger, "FullNode started"; "node_id" => node_id, "address" => &address);

        Ok(Self {
            runtime,
            node,
            grpc_server_handle: Some(grpc_server_handle),
            address,
            logger,
        })
    }

    /// Create and start a new full node in join mode (joining existing cluster)
    ///
    /// # Arguments
    /// * `address` - Network address to bind to (e.g., "127.0.0.1:50051")
    /// * `seed_addresses` - Addresses of existing cluster nodes to discover
    /// * `logger` - Logger instance
    ///
    /// # Returns
    /// A running FullNode instance that has joined the cluster
    pub async fn new_joining(
        address: String,
        seed_addresses: Vec<String>,
        logger: Logger,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        info!(logger, "Creating FullNode in join mode";
            "address" => &address,
            "seeds" => ?seed_addresses
        );

        // Discover existing peers
        use crate::grpc::bootstrap::{discover_peers, next_node_id};
        let discovered_peers = discover_peers(seed_addresses).await;

        if discovered_peers.is_empty() {
            return Err("Failed to discover any peers".into());
        }

        // Determine our node ID
        let node_id = next_node_id(&discovered_peers);
        info!(logger, "Assigned node ID"; "node_id" => node_id);

        // Find management leader
        let leader_peer = discovered_peers.iter()
            .find(|p| p.management_leader_node_id != 0)
            .ok_or("No management leader found in discovered peers")?;

        info!(logger, "Found management leader";
            "leader_node_id" => leader_peer.management_leader_node_id,
            "leader_address" => &leader_peer.management_leader_address
        );

        // Layer 1: Create transport with gRPC message sender
        let grpc_sender = Arc::new(GrpcMessageSender::new());
        let transport = Arc::new(TransportLayer::new(grpc_sender));

        // Add all discovered peers to transport
        for peer in &discovered_peers {
            transport.add_peer(peer.node_id, peer.address.clone()).await;
        }

        // Create mailbox for this node
        let (mailbox_tx, mailbox_rx) = mpsc::channel(1000);

        // Layer 2: Create cluster router
        let cluster_router = Arc::new(ClusterRouter::new());

        // Register this node's cluster (cluster_id = 0 for management)
        cluster_router.register_cluster(0, mailbox_tx).await;

        // Collect voter node IDs for initial conf_state
        let initial_voters: Vec<u64> = discovered_peers.iter()
            .map(|p| p.node_id)
            .chain(std::iter::once(node_id)) // Include ourselves
            .collect();

        info!(logger, "Joining management cluster with voters"; "voters" => ?initial_voters);

        // Layers 3-7: Create management runtime in joining mode
        let config = RaftNodeConfig {
            node_id,
            cluster_id: 0, // Management cluster ID
            snapshot_interval: 100,
            ..Default::default()
        };

        let (runtime, node) = ManagementRuntime::new_joining_node(
            config,
            transport.clone(),
            mailbox_rx,
            initial_voters,
            cluster_router.clone(),
            logger.clone(),
        )?;

        // Run Raft node in background
        let node_clone = node.clone();
        tokio::spawn(async move {
            info!(slog::Logger::root(slog::Discard, slog::o!()), "Starting Raft node event loop");
            let _ = RaftNode::run_from_arc(node_clone).await;
        });

        // Do NOT campaign - we're joining an existing cluster
        // The leader will add us via add_node()

        // Layer 0: Start gRPC server
        let grpc_server = Arc::new(GrpcServer::new(
            cluster_router,
            node_id,
            address.clone(),
        ));

        // Start leader tracker
        grpc_server.start_leader_tracker(node.clone(), transport.clone());

        let addr = address.parse()?;

        info!(logger, "Starting gRPC server"; "address" => &address);

        let grpc_server_clone = grpc_server.clone();
        let grpc_server_handle = tokio::spawn(async move {
            Server::builder()
                .add_service(
                    crate::grpc::server::raft_proto::raft_service_server::RaftServiceServer::new(
                        (*grpc_server_clone).clone(),
                    ),
                )
                .serve(addr)
                .await
        });

        info!(logger, "FullNode started in join mode"; "node_id" => node_id, "address" => &address);

        Ok(Self {
            runtime,
            node,
            grpc_server_handle: Some(grpc_server_handle),
            address,
            logger,
        })
    }

    /// Get a reference to the management runtime
    pub fn runtime(&self) -> &Arc<ManagementRuntime<WorkflowRuntime>> {
        &self.runtime
    }

    /// Get the node address
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Get the node ID
    pub fn node_id(&self) -> u64 {
        self.runtime.node_id()
    }

    /// Check if this node is the leader
    pub async fn is_leader(&self) -> bool {
        self.runtime.is_leader().await
    }

    /// Shutdown the node
    pub async fn shutdown(mut self) {
        info!(self.logger, "Shutting down FullNode");

        if let Some(handle) = self.grpc_server_handle.take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_logger() -> Logger {
        use slog::Drain;
        let decorator = slog_term::PlainDecorator::new(std::io::stdout());
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        Logger::root(drain, slog::o!())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_full_node_single_node_cluster() {
        let logger = create_logger();

        // Create a full node with gRPC server
        let node = FullNode::new(
            1,
            "127.0.0.1:50051".to_string(),
            logger.clone(),
        )
        .await
        .expect("Failed to create FullNode");

        // Wait for node to become leader
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        assert!(node.is_leader().await, "Node should be leader");

        // Create a sub-cluster using the management runtime
        let cluster_id = node.runtime()
            .create_sub_cluster(vec![1, 2, 3])
            .await
            .expect("Create sub-cluster should succeed");
        assert_eq!(cluster_id, 1); // First cluster should get ID 1

        // Set metadata using the management runtime
        node.runtime()
            .set_metadata(cluster_id, "type".to_string(), "kv".to_string())
            .await
            .expect("Set metadata should succeed");

        node.runtime()
            .set_metadata(cluster_id, "region".to_string(), "us-west".to_string())
            .await
            .expect("Set metadata should succeed");

        // Verify metadata was set correctly
        let metadata = node.runtime()
            .get_sub_cluster(&cluster_id)
            .await
            .expect("Sub-cluster should exist");

        assert_eq!(metadata.node_ids, vec![1, 2, 3]);
        assert_eq!(metadata.metadata.get("type"), Some(&"kv".to_string()));
        assert_eq!(metadata.metadata.get("region"), Some(&"us-west".to_string()));

        // Delete metadata
        node.runtime()
            .delete_metadata(cluster_id, "region".to_string())
            .await
            .expect("Delete metadata should succeed");

        let metadata = node.runtime()
            .get_sub_cluster(&cluster_id)
            .await
            .expect("Sub-cluster should exist");

        assert!(metadata.metadata.get("region").is_none());
        assert_eq!(metadata.metadata.get("type"), Some(&"kv".to_string()));

        // Delete sub-cluster
        node.runtime()
            .delete_sub_cluster(cluster_id)
            .await
            .expect("Delete sub-cluster should succeed");

        let metadata = node.runtime().get_sub_cluster(&cluster_id).await;
        assert!(metadata.is_none());

        // Shutdown
        node.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_two_node_cluster_with_discovery() {
        let logger = create_logger();

        println!("\n=== Starting two-node FullNode integration test ===\n");

        // Node 1: Bootstrap mode (creates single-node cluster)
        println!("Creating Node 1 (bootstrap mode) on 127.0.0.1:50061...");
        let node1 = FullNode::new(
            1,
            "127.0.0.1:50061".to_string(),
            logger.clone(),
        )
        .await
        .expect("Failed to create node 1");

        // Wait for node 1 to become leader
        println!("Waiting for Node 1 to become leader...");
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        assert!(node1.is_leader().await, "Node 1 should be leader");
        println!("✓ Node 1 is leader");

        // Node 2: Join mode (discovers and joins via node 1)
        println!("\nCreating Node 2 (join mode) on 127.0.0.1:50062...");
        let node2 = FullNode::new_joining(
            "127.0.0.1:50062".to_string(),
            vec!["127.0.0.1:50061".to_string()],
            logger.clone(),
        )
        .await
        .expect("Failed to create node 2");

        println!("Node 2 created, node_id: {}", node2.node_id());

        // Add node 2 to the management cluster via node 1 (as leader)
        println!("\nAdding Node 2 to management cluster via Node 1...");
        let add_rx = node1.runtime()
            .add_node(2, "127.0.0.1:50062".to_string())
            .await
            .expect("Should initiate add_node");

        // Wait for the configuration change to complete
        match add_rx.await {
            Ok(Ok(_)) => println!("✓ Node 2 added to management cluster"),
            Ok(Err(e)) => println!("⚠ Add node returned error: {}", e),
            Err(e) => println!("⚠ Add node oneshot error: {}", e),
        }

        // Wait for cluster to stabilize
        println!("\nWaiting for cluster to stabilize...");
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        // Create a sub-cluster from node 1 (the leader)
        println!("\nCreating sub-cluster with nodes [1, 2] via Node 1...");
        let cluster_id = node1.runtime()
            .create_sub_cluster(vec![1, 2])
            .await
            .expect("Create sub-cluster should succeed");

        println!("✓ Sub-cluster created with ID: {}", cluster_id);
        assert_eq!(cluster_id, 1, "First sub-cluster should have ID 1");

        // Wait for both nodes to observe the sub-cluster creation event
        println!("\nWaiting for both nodes to observe sub-cluster creation...");
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        // Verify node 1 has the sub-cluster
        println!("\nVerifying Node 1 observed sub-cluster...");
        let metadata1 = node1.runtime()
            .get_sub_cluster(&cluster_id)
            .await
            .expect("Node 1 should have sub-cluster metadata");
        println!("✓ Node 1 metadata: node_ids={:?}", metadata1.node_ids);
        assert_eq!(metadata1.node_ids, vec![1, 2]);

        // Verify node 2 has the sub-cluster
        println!("\nVerifying Node 2 observed sub-cluster...");
        let metadata2 = node2.runtime()
            .get_sub_cluster(&cluster_id)
            .await
            .expect("Node 2 should have sub-cluster metadata");
        println!("✓ Node 2 metadata: node_ids={:?}", metadata2.node_ids);
        assert_eq!(metadata2.node_ids, vec![1, 2]);

        println!("\n✓ Both nodes successfully observed the sub-cluster creation!");

        println!("\n=== Test complete - shutting down nodes ===");

        // Cleanup
        node1.shutdown().await;
        node2.shutdown().await;

        println!("✓ All tests passed!\n");
    }
}
