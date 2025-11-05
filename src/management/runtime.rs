//! Management Runtime (Layer 7)
//!
//! Provides a high-level application interface for managing sub-cluster metadata
//! built on the generic2 Raft infrastructure.

use crate::management::{ManagementEvent, ManagementStateMachine};
use crate::raft::generic2::{EventBus, ProposalRouter, RaftNode, RaftNodeConfig, Transport};
use slog::{info, Logger};
use std::marker::PhantomData;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use uuid::Uuid;

/// High-level runtime for managing sub-cluster metadata
///
/// This runtime provides a simple API for:
/// - Creating and deleting sub-clusters
/// - Managing node membership in sub-clusters
/// - Setting and querying sub-cluster metadata
/// - Subscribing to management events
///
/// The type parameter R represents the sub-cluster runtime type (e.g., KvRuntime)
/// but is only used for type safety - no actual sub-clusters are instantiated yet.
pub struct ManagementRuntime<R> {
    /// Proposal router for submitting operations
    proposal_router: Arc<ProposalRouter<ManagementStateMachine>>,

    /// Event bus for management event notifications
    event_bus: Arc<EventBus<ManagementEvent>>,

    /// Node ID
    node_id: u64,

    /// Cluster ID
    cluster_id: u32,

    /// Logger
    logger: Logger,

    /// Phantom data for sub-cluster runtime type
    _phantom: PhantomData<R>,
}

impl<R> ManagementRuntime<R> {
    /// Create a new Management runtime
    ///
    /// # Arguments
    /// * `config` - Raft node configuration
    /// * `transport` - Transport layer for network communication
    /// * `mailbox_rx` - Mailbox receiver for Raft messages
    /// * `logger` - Logger instance
    ///
    /// # Returns
    /// A tuple of (ManagementRuntime, RaftNode handle for running event loop)
    pub fn new(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc::server::raft_proto::GenericMessage>,
        logger: Logger,
    ) -> Result<
        (
            Self,
            Arc<Mutex<RaftNode<ManagementStateMachine>>>,
        ),
        Box<dyn std::error::Error>,
    > {
        let state_machine = ManagementStateMachine::new();
        let event_bus = Arc::new(EventBus::new(100));

        info!(logger, "Creating Management runtime";
            "node_id" => config.node_id,
            "cluster_id" => config.cluster_id
        );

        // Create RaftNode
        let node = RaftNode::new_single_node(
            config.clone(),
            transport.clone(),
            mailbox_rx,
            state_machine,
            event_bus.clone(),
            logger.clone(),
        )?;

        let node_arc = Arc::new(Mutex::new(node));

        // Create ProposalRouter
        let proposal_router = Arc::new(ProposalRouter::new(
            node_arc.clone(),
            transport,
            config.cluster_id,
            config.node_id,
            logger.clone(),
        ));

        // Start leader tracker in background
        let router_clone = proposal_router.clone();
        tokio::spawn(async move {
            router_clone.run_leader_tracker().await;
        });

        let runtime = Self {
            proposal_router,
            event_bus,
            node_id: config.node_id,
            cluster_id: config.cluster_id,
            logger,
            _phantom: PhantomData,
        };

        Ok((runtime, node_arc))
    }

    /// Create a new Management runtime for a node joining an existing cluster
    ///
    /// # Arguments
    /// * `config` - Raft node configuration
    /// * `transport` - Transport layer for network communication
    /// * `mailbox_rx` - Mailbox receiver for Raft messages
    /// * `initial_voters` - IDs of all nodes in the cluster (including this node)
    /// * `logger` - Logger instance
    ///
    /// # Returns
    /// A tuple of (ManagementRuntime, RaftNode handle for running event loop)
    pub fn new_joining_node(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc::server::raft_proto::GenericMessage>,
        initial_voters: Vec<u64>,
        logger: Logger,
    ) -> Result<
        (
            Self,
            Arc<Mutex<RaftNode<ManagementStateMachine>>>,
        ),
        Box<dyn std::error::Error>,
    > {
        let state_machine = ManagementStateMachine::new();
        let event_bus = Arc::new(EventBus::new(100));

        info!(logger, "Creating Management runtime (joining existing cluster)";
            "node_id" => config.node_id,
            "cluster_id" => config.cluster_id,
            "initial_voters" => ?initial_voters
        );

        // Create RaftNode for joining a multi-node cluster with known peers
        let node = RaftNode::new_multi_node_with_peers(
            config.clone(),
            transport.clone(),
            mailbox_rx,
            state_machine,
            event_bus.clone(),
            initial_voters,
            logger.clone(),
        )?;

        let node_arc = Arc::new(Mutex::new(node));

        // Create ProposalRouter
        let proposal_router = Arc::new(ProposalRouter::new(
            node_arc.clone(),
            transport,
            config.cluster_id,
            config.node_id,
            logger.clone(),
        ));

        // Start leader tracker in background
        let router_clone = proposal_router.clone();
        tokio::spawn(async move {
            router_clone.run_leader_tracker().await;
        });

        let runtime = Self {
            proposal_router,
            event_bus,
            node_id: config.node_id,
            cluster_id: config.cluster_id,
            logger,
            _phantom: PhantomData,
        };

        Ok((runtime, node_arc))
    }

    /// Create a new sub-cluster
    ///
    /// # Arguments
    /// * `cluster_id` - Unique identifier for the sub-cluster
    /// * `node_ids` - Initial node IDs to include in the sub-cluster
    ///
    /// # Returns
    /// * `Ok(())` - Operation completed successfully
    /// * `Err(String)` - Operation failed
    pub async fn create_sub_cluster(
        &self,
        cluster_id: Uuid,
        node_ids: Vec<u64>,
    ) -> Result<(), String> {
        info!(self.logger, "Creating sub-cluster";
            "cluster_id" => %cluster_id,
            "node_ids" => ?node_ids
        );

        let command = crate::management::state_machine::ManagementCommand::CreateSubCluster {
            cluster_id,
            node_ids,
        };

        self.proposal_router
            .propose_and_wait(command)
            .await
            .map_err(|e| format!("{}", e))
    }

    /// Delete a sub-cluster
    ///
    /// # Arguments
    /// * `cluster_id` - Unique identifier of the sub-cluster to delete
    ///
    /// # Returns
    /// * `Ok(())` - Operation completed successfully
    /// * `Err(String)` - Operation failed
    pub async fn delete_sub_cluster(&self, cluster_id: Uuid) -> Result<(), String> {
        info!(self.logger, "Deleting sub-cluster"; "cluster_id" => %cluster_id);

        let command = crate::management::state_machine::ManagementCommand::DeleteSubCluster {
            cluster_id,
        };

        self.proposal_router
            .propose_and_wait(command)
            .await
            .map_err(|e| format!("{}", e))
    }

    /// Add a node to a sub-cluster
    ///
    /// # Arguments
    /// * `cluster_id` - Sub-cluster identifier
    /// * `node_id` - Node ID to add
    ///
    /// # Returns
    /// * `Ok(())` - Operation completed successfully
    /// * `Err(String)` - Operation failed
    pub async fn add_node_to_sub_cluster(
        &self,
        cluster_id: Uuid,
        node_id: u64,
    ) -> Result<(), String> {
        info!(self.logger, "Adding node to sub-cluster";
            "cluster_id" => %cluster_id,
            "node_id" => node_id
        );

        let command =
            crate::management::state_machine::ManagementCommand::AddNodeToSubCluster {
                cluster_id,
                node_id,
            };

        self.proposal_router
            .propose_and_wait(command)
            .await
            .map_err(|e| format!("{}", e))
    }

    /// Remove a node from a sub-cluster
    ///
    /// # Arguments
    /// * `cluster_id` - Sub-cluster identifier
    /// * `node_id` - Node ID to remove
    ///
    /// # Returns
    /// * `Ok(())` - Operation completed successfully
    /// * `Err(String)` - Operation failed
    pub async fn remove_node_from_sub_cluster(
        &self,
        cluster_id: Uuid,
        node_id: u64,
    ) -> Result<(), String> {
        info!(self.logger, "Removing node from sub-cluster";
            "cluster_id" => %cluster_id,
            "node_id" => node_id
        );

        let command =
            crate::management::state_machine::ManagementCommand::RemoveNodeFromSubCluster {
                cluster_id,
                node_id,
            };

        self.proposal_router
            .propose_and_wait(command)
            .await
            .map_err(|e| format!("{}", e))
    }

    /// Set metadata for a sub-cluster
    ///
    /// # Arguments
    /// * `cluster_id` - Sub-cluster identifier
    /// * `key` - Metadata key
    /// * `value` - Metadata value
    ///
    /// # Returns
    /// * `Ok(())` - Operation completed successfully
    /// * `Err(String)` - Operation failed
    pub async fn set_metadata(
        &self,
        cluster_id: Uuid,
        key: String,
        value: String,
    ) -> Result<(), String> {
        info!(self.logger, "Setting sub-cluster metadata";
            "cluster_id" => %cluster_id,
            "key" => &key,
            "value" => &value
        );

        let command = crate::management::state_machine::ManagementCommand::SetMetadata {
            cluster_id,
            key,
            value,
        };

        self.proposal_router
            .propose_and_wait(command)
            .await
            .map_err(|e| format!("{}", e))
    }

    /// Delete metadata for a sub-cluster
    ///
    /// # Arguments
    /// * `cluster_id` - Sub-cluster identifier
    /// * `key` - Metadata key to delete
    ///
    /// # Returns
    /// * `Ok(())` - Operation completed successfully
    /// * `Err(String)` - Operation failed
    pub async fn delete_metadata(&self, cluster_id: Uuid, key: String) -> Result<(), String> {
        info!(self.logger, "Deleting sub-cluster metadata";
            "cluster_id" => %cluster_id,
            "key" => &key
        );

        let command = crate::management::state_machine::ManagementCommand::DeleteMetadata {
            cluster_id,
            key,
        };

        self.proposal_router
            .propose_and_wait(command)
            .await
            .map_err(|e| format!("{}", e))
    }

    /// Get sub-cluster metadata (reads from local state machine)
    ///
    /// # Arguments
    /// * `cluster_id` - Sub-cluster identifier
    ///
    /// # Returns
    /// * `Some(metadata)` - Sub-cluster exists with this metadata
    /// * `None` - Sub-cluster does not exist
    pub async fn get_sub_cluster(
        &self,
        cluster_id: &Uuid,
    ) -> Option<crate::management::state_machine::SubClusterMetadata> {
        let node_arc = self.proposal_router.node();
        let node = node_arc.lock().await;
        let sm = node.state_machine().clone();
        drop(node);

        sm.lock().await.get_sub_cluster(cluster_id).cloned()
    }

    /// List all sub-cluster IDs (reads from local state machine)
    ///
    /// # Returns
    /// Vector of all sub-cluster IDs
    pub async fn list_sub_clusters(&self) -> Vec<Uuid> {
        let node_arc = self.proposal_router.node();
        let node = node_arc.lock().await;
        let sm = node.state_machine().clone();
        drop(node);

        sm.lock().await.list_sub_clusters()
    }

    /// Get all sub-cluster metadata (reads from local state machine)
    ///
    /// # Returns
    /// HashMap of all sub-cluster metadata
    pub async fn get_all_sub_clusters(
        &self,
    ) -> std::collections::HashMap<Uuid, crate::management::state_machine::SubClusterMetadata>
    {
        let node_arc = self.proposal_router.node();
        let node = node_arc.lock().await;
        let sm = node.state_machine().clone();
        drop(node);

        sm.lock().await.get_all_sub_clusters().clone()
    }

    /// Subscribe to management events
    ///
    /// # Returns
    /// Receiver for ManagementEvent notifications
    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<ManagementEvent> {
        self.event_bus.subscribe()
    }

    /// Check if this node is the leader
    pub async fn is_leader(&self) -> bool {
        self.proposal_router.is_leader().await
    }

    /// Get the current leader ID (if known)
    pub async fn leader_id(&self) -> Option<u64> {
        self.proposal_router.leader_id().await
    }

    /// Get this node's ID
    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    /// Get the cluster ID
    pub fn cluster_id(&self) -> u32 {
        self.cluster_id
    }

    /// Add a node to the management cluster
    ///
    /// # Arguments
    /// * `node_id` - ID of the node to add
    /// * `address` - Network address of the node
    ///
    /// # Returns
    /// Receiver that will be notified when the configuration change completes
    pub async fn add_node(
        &self,
        node_id: u64,
        address: String,
    ) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        info!(self.logger, "Adding node to management cluster"; "node_id" => node_id, "address" => &address);
        self.proposal_router.add_node(node_id, address).await
    }

    /// Remove a node from the management cluster
    ///
    /// # Arguments
    /// * `node_id` - ID of the node to remove
    ///
    /// # Returns
    /// Receiver that will be notified when the configuration change completes
    pub async fn remove_node(
        &self,
        node_id: u64,
    ) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        info!(self.logger, "Removing node from management cluster"; "node_id" => node_id);
        self.proposal_router.remove_node(node_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::generic2::{InProcessMessageSender, InProcessServer, TransportLayer};
    use crate::raft::generic2::errors::TransportError;

    fn create_logger() -> Logger {
        use slog::Drain;
        let decorator = slog_term::PlainDecorator::new(std::io::stdout());
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        Logger::root(drain, slog::o!())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_management_runtime_basic_operations() {
        let logger = create_logger();
        let server = Arc::new(InProcessServer::new());
        let transport = Arc::new(TransportLayer::new(Arc::new(InProcessMessageSender::new(
            server.clone(),
        ))));

        let (tx, rx) = mpsc::channel(100);

        let config = RaftNodeConfig {
            node_id: 1,
            cluster_id: 0,
            snapshot_interval: 0, // Disable snapshots for this test
            ..Default::default()
        };

        // Register node with server
        let tx_clone = tx.clone();
        server
            .register_node(1, move |msg| {
                tx_clone.try_send(msg).map_err(|e| TransportError::SendFailed {
                    node_id: 1,
                    reason: format!("Failed to send: {}", e),
                })
            })
            .await;

        // Use () as placeholder for sub-cluster type since we're not instantiating any
        let (runtime, node) =
            ManagementRuntime::<()>::new(config, transport, rx, logger).unwrap();

        // Campaign to become leader
        node.lock()
            .await
            .campaign()
            .await
            .expect("Campaign should succeed");

        // Run node in background
        let node_clone = node.clone();
        let _node_handle = tokio::spawn(async move {
            use crate::raft::generic2::RaftNode;
            let _ = RaftNode::run_from_arc(node_clone).await;
        });

        // Wait for node to become leader
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        assert!(runtime.is_leader().await, "Node should be leader");

        // Test create sub-cluster
        let cluster_id = Uuid::new_v4();
        runtime
            .create_sub_cluster(cluster_id, vec![1, 2, 3])
            .await
            .expect("Create sub-cluster should succeed");

        // Test get sub-cluster
        let metadata = runtime.get_sub_cluster(&cluster_id).await;
        assert!(metadata.is_some());
        assert_eq!(metadata.as_ref().unwrap().node_ids, vec![1, 2, 3]);

        // Test list sub-clusters
        let clusters = runtime.list_sub_clusters().await;
        assert_eq!(clusters.len(), 1);
        assert!(clusters.contains(&cluster_id));

        // Test add node to sub-cluster
        runtime
            .add_node_to_sub_cluster(cluster_id, 4)
            .await
            .expect("Add node should succeed");

        let metadata = runtime.get_sub_cluster(&cluster_id).await.unwrap();
        assert_eq!(metadata.node_ids, vec![1, 2, 3, 4]);

        // Test set metadata
        runtime
            .set_metadata(cluster_id, "type".to_string(), "kv".to_string())
            .await
            .expect("Set metadata should succeed");

        let metadata = runtime.get_sub_cluster(&cluster_id).await.unwrap();
        assert_eq!(
            metadata.metadata.get("type"),
            Some(&"kv".to_string())
        );

        // Test delete metadata
        runtime
            .delete_metadata(cluster_id, "type".to_string())
            .await
            .expect("Delete metadata should succeed");

        let metadata = runtime.get_sub_cluster(&cluster_id).await.unwrap();
        assert!(metadata.metadata.get("type").is_none());

        // Test remove node from sub-cluster
        runtime
            .remove_node_from_sub_cluster(cluster_id, 4)
            .await
            .expect("Remove node should succeed");

        let metadata = runtime.get_sub_cluster(&cluster_id).await.unwrap();
        assert_eq!(metadata.node_ids, vec![1, 2, 3]);

        // Test delete sub-cluster
        runtime
            .delete_sub_cluster(cluster_id)
            .await
            .expect("Delete sub-cluster should succeed");

        let metadata = runtime.get_sub_cluster(&cluster_id).await;
        assert!(metadata.is_none());

        let clusters = runtime.list_sub_clusters().await;
        assert_eq!(clusters.len(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_management_runtime_with_kv_runtime_type() {
        // This test demonstrates ManagementRuntime parameterized with KvRuntime
        // to track metadata about KV sub-clusters (not actually creating them yet)
        use crate::kv::KvRuntime;

        let logger = create_logger();
        let server = Arc::new(InProcessServer::new());
        let transport = Arc::new(TransportLayer::new(Arc::new(InProcessMessageSender::new(
            server.clone(),
        ))));

        let (tx, rx) = mpsc::channel(100);

        let config = RaftNodeConfig {
            node_id: 1,
            cluster_id: 0,  // Management cluster
            snapshot_interval: 0,
            ..Default::default()
        };

        // Register node with server
        let tx_clone = tx.clone();
        server
            .register_node(1, move |msg| {
                tx_clone.try_send(msg).map_err(|e| TransportError::SendFailed {
                    node_id: 1,
                    reason: format!("Failed to send: {}", e),
                })
            })
            .await;

        // Create ManagementRuntime typed to manage KvRuntime sub-clusters
        let (runtime, node) =
            ManagementRuntime::<KvRuntime>::new(config, transport, rx, logger).unwrap();

        // Campaign to become leader
        node.lock()
            .await
            .campaign()
            .await
            .expect("Campaign should succeed");

        // Run node in background
        let node_clone = node.clone();
        let _node_handle = tokio::spawn(async move {
            use crate::raft::generic2::RaftNode;
            let _ = RaftNode::run_from_arc(node_clone).await;
        });

        // Wait for node to become leader
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        assert!(runtime.is_leader().await, "Node should be leader");

        // Track metadata for three KV execution clusters
        let kv_cluster_1 = Uuid::new_v4();
        let kv_cluster_2 = Uuid::new_v4();
        let kv_cluster_3 = Uuid::new_v4();

        // Create first KV cluster with nodes 1,2,3
        runtime
            .create_sub_cluster(kv_cluster_1, vec![1, 2, 3])
            .await
            .expect("Create first KV cluster should succeed");

        runtime
            .set_metadata(kv_cluster_1, "type".to_string(), "kv".to_string())
            .await
            .expect("Set metadata should succeed");

        runtime
            .set_metadata(kv_cluster_1, "region".to_string(), "us-west".to_string())
            .await
            .expect("Set metadata should succeed");

        // Create second KV cluster with nodes 4,5,6
        runtime
            .create_sub_cluster(kv_cluster_2, vec![4, 5, 6])
            .await
            .expect("Create second KV cluster should succeed");

        runtime
            .set_metadata(kv_cluster_2, "type".to_string(), "kv".to_string())
            .await
            .expect("Set metadata should succeed");

        runtime
            .set_metadata(kv_cluster_2, "region".to_string(), "us-east".to_string())
            .await
            .expect("Set metadata should succeed");

        // Create third KV cluster with nodes 7,8,9
        runtime
            .create_sub_cluster(kv_cluster_3, vec![7, 8, 9])
            .await
            .expect("Create third KV cluster should succeed");

        runtime
            .set_metadata(kv_cluster_3, "type".to_string(), "kv".to_string())
            .await
            .expect("Set metadata should succeed");

        runtime
            .set_metadata(kv_cluster_3, "region".to_string(), "eu-west".to_string())
            .await
            .expect("Set metadata should succeed");

        // Verify all clusters are tracked
        let all_clusters = runtime.list_sub_clusters().await;
        assert_eq!(all_clusters.len(), 3);
        assert!(all_clusters.contains(&kv_cluster_1));
        assert!(all_clusters.contains(&kv_cluster_2));
        assert!(all_clusters.contains(&kv_cluster_3));

        // Verify metadata for each cluster
        let meta1 = runtime.get_sub_cluster(&kv_cluster_1).await.unwrap();
        assert_eq!(meta1.node_ids, vec![1, 2, 3]);
        assert_eq!(meta1.metadata.get("type"), Some(&"kv".to_string()));
        assert_eq!(meta1.metadata.get("region"), Some(&"us-west".to_string()));

        let meta2 = runtime.get_sub_cluster(&kv_cluster_2).await.unwrap();
        assert_eq!(meta2.node_ids, vec![4, 5, 6]);
        assert_eq!(meta2.metadata.get("region"), Some(&"us-east".to_string()));

        let meta3 = runtime.get_sub_cluster(&kv_cluster_3).await.unwrap();
        assert_eq!(meta3.node_ids, vec![7, 8, 9]);
        assert_eq!(meta3.metadata.get("region"), Some(&"eu-west".to_string()));

        // Test get_all_sub_clusters
        let all_metadata = runtime.get_all_sub_clusters().await;
        assert_eq!(all_metadata.len(), 3);

        // Delete one cluster and verify
        runtime
            .delete_sub_cluster(kv_cluster_2)
            .await
            .expect("Delete should succeed");

        let remaining_clusters = runtime.list_sub_clusters().await;
        assert_eq!(remaining_clusters.len(), 2);
        assert!(!remaining_clusters.contains(&kv_cluster_2));
    }
}
