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
    ClusterRouter, RaftNode, RaftNodeConfig, TransportLayer,
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
    /// Create and start a new full node
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
        let grpc_server = GrpcServer::new_with_node_id(cluster_router, node_id);
        let addr = address.parse()?;

        info!(logger, "Starting gRPC server"; "address" => &address);

        let grpc_server_handle = tokio::spawn(async move {
            Server::builder()
                .add_service(
                    crate::grpc::server::raft_proto::raft_service_server::RaftServiceServer::new(
                        grpc_server,
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
}
