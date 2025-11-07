//! Key-Value Store Runtime (Layer 7)
//!
//! Provides a high-level application interface for a distributed key-value store
//! built on the generic2 Raft infrastructure.

use crate::raft::generic2::{
    EventBus, KvCommand, KvEvent, KvStateMachine, ProposalRouter, RaftNode,
    RaftNodeConfig, Transport,
};
use slog::{info, Logger};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};

/// High-level runtime for the distributed key-value store
///
/// This runtime provides a simple API for:
/// - Setting and getting key-value pairs
/// - Managing node lifecycle (start, stop, add/remove nodes)
/// - Subscribing to state change events
pub struct KvRuntime {
    /// Proposal router for submitting operations
    proposal_router: Arc<ProposalRouter<KvStateMachine>>,

    /// Event bus for state change notifications
    event_bus: Arc<EventBus<KvEvent>>,

    /// Node ID
    node_id: u64,

    /// Cluster ID
    cluster_id: u32,

    /// Logger
    logger: Logger,
}

impl KvRuntime {
    /// Create a new KV runtime
    ///
    /// # Arguments
    /// * `config` - Raft node configuration
    /// * `transport` - Transport layer for network communication
    /// * `mailbox_rx` - Mailbox receiver for Raft messages
    /// * `logger` - Logger instance
    ///
    /// # Returns
    /// A tuple of (KvRuntime, RaftNode handle for running event loop)
    pub fn new(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc::server::raft_proto::GenericMessage>,
        logger: Logger,
    ) -> Result<(Self, Arc<Mutex<RaftNode<KvStateMachine>>>), Box<dyn std::error::Error>> {
        let state_machine = KvStateMachine::new();
        let event_bus = Arc::new(EventBus::new(100));

        info!(logger, "Creating KV runtime";
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
        };

        Ok((runtime, node_arc))
    }

    /// Create a new KV runtime for a node joining an existing cluster
    ///
    /// This variant creates a node that will join an existing cluster rather than
    /// starting as a single-node cluster.
    ///
    /// # Arguments
    /// * `config` - Raft node configuration
    /// * `transport` - Transport layer for network communication
    /// * `mailbox_rx` - Mailbox receiver for Raft messages
    /// * `initial_voters` - IDs of all nodes in the cluster (including this node)
    /// * `logger` - Logger instance
    ///
    /// # Returns
    /// A tuple of (KvRuntime, RaftNode handle for running event loop)
    pub fn new_joining_node(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc::server::raft_proto::GenericMessage>,
        initial_voters: Vec<u64>,
        logger: Logger,
    ) -> Result<(Self, Arc<Mutex<RaftNode<KvStateMachine>>>), Box<dyn std::error::Error>> {
        let state_machine = KvStateMachine::new();
        let event_bus = Arc::new(EventBus::new(100));

        info!(logger, "Creating KV runtime (joining existing cluster)";
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
        };

        Ok((runtime, node_arc))
    }

    /// Set a key-value pair
    ///
    /// # Arguments
    /// * `key` - The key to set
    /// * `value` - The value to associate with the key
    ///
    /// # Returns
    /// * `Ok(())` - Operation completed successfully
    /// * `Err(String)` - Operation failed
    pub async fn set(&self, key: String, value: String) -> Result<(), String> {
        info!(self.logger, "Setting key-value pair"; "key" => &key, "value" => &value);

        let command = KvCommand::Set {
            key: key.clone(),
            value: value.clone(),
        };

        self.proposal_router.propose_and_wait(command).await
            .map_err(|e| format!("{}", e))
    }

    /// Delete a key
    ///
    /// # Arguments
    /// * `key` - The key to delete
    ///
    /// # Returns
    /// * `Ok(())` - Operation completed successfully
    /// * `Err(String)` - Operation failed
    pub async fn delete(&self, key: String) -> Result<(), String> {
        info!(self.logger, "Deleting key"; "key" => &key);

        let command = KvCommand::Delete { key: key.clone() };

        self.proposal_router.propose_and_wait(command).await
            .map_err(|e| format!("{}", e))
    }

    /// Get a value by key (reads from local state machine)
    ///
    /// Note: This performs a local read and may return stale data if this node
    /// is not the leader or is behind. For strongly consistent reads, use get_consistent().
    ///
    /// # Arguments
    /// * `key` - The key to look up
    ///
    /// # Returns
    /// * `Some(value)` - Key exists with this value
    /// * `None` - Key does not exist
    pub async fn get(&self, key: &str) -> Option<String> {
        let node_arc = self.proposal_router.node();
        let node = node_arc.lock().await;
        let sm = node.state_machine().clone();
        drop(node);

        sm.lock().await.get(key).cloned()
    }

    /// Get all keys (reads from local state machine)
    ///
    /// # Returns
    /// Vector of all keys in the store
    pub async fn keys(&self) -> Vec<String> {
        let node_arc = self.proposal_router.node();
        let node = node_arc.lock().await;
        let sm = node.state_machine().clone();
        drop(node);

        sm.lock().await.keys()
    }

    /// Get all key-value pairs (reads from local state machine)
    ///
    /// # Returns
    /// HashMap of all key-value pairs
    pub async fn get_all(&self) -> HashMap<String, String> {
        let node_arc = self.proposal_router.node();
        let node = node_arc.lock().await;
        let sm = node.state_machine().clone();
        drop(node);

        let sm_locked = sm.lock().await;
        sm_locked.keys().into_iter()
            .filter_map(|k| sm_locked.get(&k).map(|v| (k.clone(), v.clone())))
            .collect()
    }

    /// Subscribe to state change events
    ///
    /// # Returns
    /// Receiver for KvEvent notifications
    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<KvEvent> {
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

    /// Add a node to the cluster
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
        info!(self.logger, "Adding node to cluster"; "node_id" => node_id, "address" => &address);
        self.proposal_router.add_node(node_id, address).await
    }

    /// Remove a node from the cluster
    ///
    /// # Arguments
    /// * `node_id` - ID of the node to remove
    ///
    /// # Returns
    /// Receiver that will be notified when the configuration change completes
    pub async fn remove_node(&self, node_id: u64) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        info!(self.logger, "Removing node from cluster"; "node_id" => node_id);
        self.proposal_router.remove_node(node_id).await
    }
}

/// Dummy registry for KvRuntime (not actually used, but required by trait)
pub struct KvRegistry;

// Implement SubClusterRuntime for use with ManagementRuntime
impl crate::management::SubClusterRuntime for KvRuntime {
    type StateMachine = KvStateMachine;
    type Registry = KvRegistry;

    fn new_single_node(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc::server::raft_proto::GenericMessage>,
        _registry: Arc<tokio::sync::Mutex<Self::Registry>>,
        logger: slog::Logger,
    ) -> Result<(Self, Arc<Mutex<RaftNode<Self::StateMachine>>>), Box<dyn std::error::Error>> {
        KvRuntime::new(config, transport, mailbox_rx, logger)
    }

    fn new_joining_node(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc::server::raft_proto::GenericMessage>,
        initial_voters: Vec<u64>,
        _registry: Arc<tokio::sync::Mutex<Self::Registry>>,
        logger: slog::Logger,
    ) -> Result<(Self, Arc<Mutex<RaftNode<Self::StateMachine>>>), Box<dyn std::error::Error>> {
        KvRuntime::new_joining_node(config, transport, mailbox_rx, initial_voters, logger)
    }

    async fn add_node(&self, node_id: u64, address: String)
        -> Result<oneshot::Receiver<Result<(), String>>, String> {
        self.add_node(node_id, address).await
    }

    async fn remove_node(&self, node_id: u64)
        -> Result<oneshot::Receiver<Result<(), String>>, String> {
        self.remove_node(node_id).await
    }

    fn node_id(&self) -> u64 {
        self.node_id()
    }

    fn cluster_id(&self) -> u32 {
        self.cluster_id()
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
    async fn test_kv_runtime_basic_operations() {
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
        server.register_node(1, move |msg| {
            tx_clone.try_send(msg).map_err(|e| TransportError::SendFailed {
                node_id: 1,
                reason: format!("Failed to send: {}", e),
            })
        }).await;

        let (runtime, node) = KvRuntime::new(config, transport, rx, logger).unwrap();

        // Campaign to become leader
        node.lock().await.campaign().await.expect("Campaign should succeed");

        // Run node in background
        let node_clone = node.clone();
        let _node_handle = tokio::spawn(async move {
            use crate::raft::generic2::RaftNode;
            let _ = RaftNode::run_from_arc(node_clone).await;
        });

        // Wait for node to become leader
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        assert!(runtime.is_leader().await, "Node should be leader");

        // Test set operation
        runtime.set("key1".to_string(), "value1".to_string())
            .await
            .expect("Set should succeed");

        // Test get operation
        let value = runtime.get("key1").await;
        assert_eq!(value, Some("value1".to_string()));

        // Test another set
        runtime.set("key2".to_string(), "value2".to_string())
            .await
            .expect("Set should succeed");

        // Test keys
        let keys = runtime.keys().await;
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&"key1".to_string()));
        assert!(keys.contains(&"key2".to_string()));

        // Test get_all
        let all = runtime.get_all().await;
        assert_eq!(all.len(), 2);
        assert_eq!(all.get("key1"), Some(&"value1".to_string()));
        assert_eq!(all.get("key2"), Some(&"value2".to_string()));

        // Test delete
        runtime.delete("key1".to_string())
            .await
            .expect("Delete should succeed");

        let value = runtime.get("key1").await;
        assert_eq!(value, None);

        let keys = runtime.keys().await;
        assert_eq!(keys.len(), 1);
        assert!(keys.contains(&"key2".to_string()));
    }
}
