use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use serde::{Serialize, Deserialize};
use crate::raft::generic::message::{Message, CommandExecutor};
use crate::raft::generic::cluster::RaftCluster;

/// Unified interface for Raft nodes to interact with the transport layer
///
/// This trait provides all necessary operations for RaftCluster and RaftNode to
/// communicate with peers without directly accessing transport internals.
/// Generic over message type M.
pub trait TransportInteraction<M>: Send + Sync
where
    M: Send + Sync + 'static,
{
    /// Send a message to a specific node
    /// Returns error if node doesn't exist or send fails
    fn send_message_to_node(
        &self,
        target_node_id: u64,
        message: M
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Get list of all known node IDs
    /// Used by send_to_leader to scan for the leader
    fn list_nodes(&self) -> Vec<u64>;

    /// Add a peer to the transport (for dynamic membership)
    /// Called by RaftNode when applying AddNode ConfChange
    fn add_peer(
        &self,
        node_id: u64,
        address: String
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Remove a peer from the transport (for dynamic membership)
    /// Called by RaftNode when applying RemoveNode ConfChange
    fn remove_peer(
        &self,
        node_id: u64
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Get discovered voter configuration from peer discovery
    /// Used to initialize joining nodes with proper Raft configuration
    fn get_discovered_voters(&self) -> Vec<u64>;
}

/// Metadata about a node's transport address
/// Embedded in ConfChangeV2.context field for dynamic node discovery
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeMetadata {
    pub node_id: u64,
    pub address: String,  // For GrpcClusterTransport: "host:port"
}

impl NodeMetadata {
    /// Serialize to bytes for ConfChangeV2.context
    pub fn to_bytes(&self) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Deserialize from ConfChangeV2.context bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Box<dyn std::error::Error>> {
        if bytes.is_empty() {
            return Err("Empty metadata bytes".into());
        }
        Ok(serde_json::from_slice(bytes)?)
    }
}

/// Transport layer abstraction for Raft cluster communication
///
/// This trait provides an abstraction for how Raft nodes communicate with each other.
/// Different implementations can provide:
/// - InMemoryClusterTransport: For local testing with multiple nodes in the same process
/// - GrpcClusterTransport: For distributed operation with nodes on different machines
///
/// The transport is responsible for:
/// 1. Creating all communication channels (sender/receiver pairs) for each node
/// 2. Providing channel handles to RaftCluster instances during creation
/// 3. Facilitating message routing between nodes (in-memory or over network)
///
/// Generic over message type M, which is the type of messages being transported.
/// The transport doesn't need to understand the message structure, only how to send/receive it.
pub trait ClusterTransport<M>: Send + Sync
where
    M: Send + Sync + 'static,
{
    /// Create a RaftCluster instance for a specific node with a given executor
    ///
    /// # Arguments
    /// * `node_id` - The unique ID of the node to create
    /// * `executor` - The command executor for this cluster
    ///
    /// # Returns
    /// A RaftCluster instance configured to communicate with peers via this transport
    ///
    /// # Type Parameters
    /// * `E` - The executor type
    fn create_cluster<E>(
        self: &Arc<Self>,
        node_id: u64,
        executor: E,
    ) -> impl std::future::Future<Output = Result<Arc<RaftCluster<E>>, Box<dyn std::error::Error>>> + Send
    where
        E: CommandExecutor + Default + 'static;

    /// Get the list of all node IDs in this transport configuration
    fn node_ids(&self) -> impl std::future::Future<Output = Vec<u64>> + Send;

    /// Start the transport layer (e.g., background message routing tasks)
    fn start(&self) -> impl std::future::Future<Output = Result<(), Box<dyn std::error::Error>>> + Send;

    /// Shutdown the transport layer gracefully
    fn shutdown(&self) -> impl std::future::Future<Output = Result<(), Box<dyn std::error::Error>>> + Send;

    /// Add a peer node dynamically (called when ConfChange is applied)
    /// Returns Ok(()) if peer was added or already exists
    fn add_peer(&self, node_id: u64, address: String) -> Result<(), Box<dyn std::error::Error>>;

    /// Remove a peer node dynamically (called when node is removed)
    fn remove_peer(&self, node_id: u64) -> Result<(), Box<dyn std::error::Error>>;
}

/// In-memory transport for local multi-node testing
///
/// This transport creates multiple RaftCluster instances in the same process,
/// with message routing facilitated by tokio channels. A background task
/// routes messages from each node's sender to the appropriate peer's receiver.
///
/// # Example
/// ```no_run
/// use raftoral::raft::generic::transport::{ClusterTransport, InMemoryClusterTransport};
/// use raftoral::workflow::WorkflowCommandExecutor;
/// use std::sync::Arc;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Create transport for 3-node cluster
/// let transport = Arc::new(InMemoryClusterTransport::<WorkflowCommandExecutor>::new(vec![1, 2, 3]));
/// transport.start().await?;
///
/// // Create cluster instances for each node (executors created internally)
/// let cluster1 = transport.create_cluster(1).await?;
/// let cluster2 = transport.create_cluster(2).await?;
/// let cluster3 = transport.create_cluster(3).await?;
/// # Ok(())
/// # }
/// ```
pub struct InMemoryClusterTransport<M>
where
    M: Send + Sync + 'static,
{
    /// Node IDs in this cluster
    node_ids: Vec<u64>,
    /// Senders for each node (node_id -> sender to that node's receiver)
    /// Shared with RaftNode instances for immediate visibility of changes
    node_senders: Arc<std::sync::RwLock<HashMap<u64, mpsc::UnboundedSender<M>>>>,
    /// Receivers for each node (held until create_cluster is called)
    node_receivers: Arc<tokio::sync::Mutex<HashMap<u64, mpsc::UnboundedReceiver<M>>>>,
    /// Shutdown signal
    shutdown_tx: Option<tokio::sync::broadcast::Sender<()>>,
}

impl<M> InMemoryClusterTransport<M>
where
    M: Send + Sync + 'static,
{
    /// Create a new in-memory transport for the given node IDs
    pub fn new(node_ids: Vec<u64>) -> Self {
        let mut node_senders_map = HashMap::new();
        let mut node_receivers_map = HashMap::new();

        // Create unbounded channels for each node
        for &node_id in &node_ids {
            let (sender, receiver) = mpsc::unbounded_channel();
            node_senders_map.insert(node_id, sender);
            node_receivers_map.insert(node_id, receiver);
        }

        InMemoryClusterTransport {
            node_ids,
            node_senders: Arc::new(std::sync::RwLock::new(node_senders_map)),
            node_receivers: Arc::new(tokio::sync::Mutex::new(node_receivers_map)),
            shutdown_tx: None,
        }
    }
}

// Implement ClusterTransport for InMemoryClusterTransport<Message<C>>
// This restricts M to be Message<C> for some command type C
impl<C> ClusterTransport<Message<C>> for InMemoryClusterTransport<Message<C>>
where
    C: Clone + std::fmt::Debug + Serialize + for<'de> Deserialize<'de> + Send + Sync + 'static,
{
    async fn create_cluster<E>(
        self: &Arc<Self>,
        node_id: u64,
        executor: E,
    ) -> Result<Arc<RaftCluster<E>>, Box<dyn std::error::Error>>
    where
        E: CommandExecutor<Command = C> + Default + 'static,
    {
        // Extract the receiver for this node
        let receiver = {
            let mut receivers = self.node_receivers.lock().await;
            receivers.remove(&node_id)
                .ok_or_else(|| format!("Node {} receiver already claimed or doesn't exist", node_id))?
        };

        // Create transport interface as Arc<dyn TransportInteraction>
        let transport: Arc<dyn TransportInteraction<Message<C>>> = self.clone();

        // Create the cluster with transport interface
        let cluster = RaftCluster::new_with_transport(
            node_id,
            receiver,
            transport,
            executor,
        ).await?;

        Ok(Arc::new(cluster))
    }

    async fn node_ids(&self) -> Vec<u64> {
        self.node_ids.clone()
    }

    async fn start(&self) -> Result<(), Box<dyn std::error::Error>> {
        // For in-memory transport, no background routing is needed
        // Messages are sent directly via the mpsc channels
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), Box<dyn std::error::Error>> {
        // Signal shutdown if we have a sender
        if let Some(tx) = &self.shutdown_tx {
            let _ = tx.send(());
        }
        Ok(())
    }

    fn add_peer(&self, _node_id: u64, _address: String) -> Result<(), Box<dyn std::error::Error>> {
        // In-memory transport has fixed nodes at construction time
        // This is a no-op for testing - nodes are pre-configured
        Ok(())
    }

    fn remove_peer(&self, _node_id: u64) -> Result<(), Box<dyn std::error::Error>> {
        // In-memory transport - no-op
        Ok(())
    }
}

// Implement TransportInteraction for InMemoryClusterTransport
impl<M> TransportInteraction<M> for InMemoryClusterTransport<M>
where
    M: Send + Sync + 'static,
{
    fn send_message_to_node(
        &self,
        target_node_id: u64,
        message: M
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let senders = self.node_senders.read().unwrap();
        let sender = senders.get(&target_node_id)
            .ok_or_else(|| format!("Node {} not found", target_node_id))?;

        sender.send(message)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        Ok(())
    }

    fn list_nodes(&self) -> Vec<u64> {
        self.node_senders.read().unwrap().keys().copied().collect()
    }

    fn add_peer(&self, _node_id: u64, _address: String) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // In-memory transport has fixed nodes at construction time
        // This is a no-op for testing - nodes are pre-configured
        Ok(())
    }

    fn remove_peer(&self, _node_id: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // In-memory transport - no-op
        Ok(())
    }

    fn get_discovered_voters(&self) -> Vec<u64> {
        // In-memory transport doesn't use discovery - return empty
        // Tests using InMemoryClusterTransport don't need this feature
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, Serialize, Deserialize)]
    enum TestCommand {
        Noop,
    }

    #[derive(Default)]
    struct TestExecutor;

    impl CommandExecutor for TestExecutor {
        type Command = TestCommand;

        fn apply_with_index(&self, _command: &Self::Command, _logger: &slog::Logger, _log_index: u64) -> Result<(), Box<dyn std::error::Error>> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_in_memory_transport_creation() {
        let transport = InMemoryClusterTransport::<TestCommand>::new(vec![1, 2, 3]);
        assert_eq!(transport.node_ids().await, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn test_in_memory_transport_cluster_creation() {
        let transport = Arc::new(InMemoryClusterTransport::<TestCommand>::new(vec![1, 2, 3]));
        transport.start().await.expect("Transport start should succeed");

        // Create executor
        let executor1 = TestExecutor::default();

        // Create cluster for node 1
        let cluster1 = transport.create_cluster(1, executor1).await;
        assert!(cluster1.is_ok(), "Should create cluster for node 1");

        // Try to create same node again - should fail
        let executor1_again = TestExecutor::default();
        let cluster1_again = transport.create_cluster(1, executor1_again).await;
        assert!(cluster1_again.is_err(), "Should not allow creating same node twice");
    }
}
