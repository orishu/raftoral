use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use serde::{Serialize, Deserialize};

/// Unified interface for Raft nodes to interact with the transport layer
///
/// This trait provides all necessary operations for RaftCluster and RaftNode to
/// communicate with peers without directly accessing transport internals.
/// Generic over message type M - any type that can serve as a message.
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

/// In-memory transport for local multi-node testing
///
/// This transport creates multiple RaftCluster instances in the same process,
/// with message routing facilitated by tokio channels.
///
/// Generic over:
/// - M: Message type being transported
///
/// # Example
/// ```no_run
/// use raftoral::raft::generic::transport::InMemoryClusterTransport;
/// use raftoral::raft::generic::message::Message;
/// use raftoral::raft::generic::cluster::RaftCluster;
/// use raftoral::workflow::{WorkflowCommandExecutor, WorkflowCommand};
/// use std::sync::Arc;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Create transport for 3-node cluster
/// let transport = Arc::new(InMemoryClusterTransport::<Message<WorkflowCommand>>::new(vec![1, 2, 3]));
///
/// // Extract receiver and create cluster manually
/// let receiver = transport.extract_receiver(1).await?;
/// let executor = WorkflowCommandExecutor::default();
/// let cluster1 = RaftCluster::new(1, receiver, transport.clone(), executor).await?;
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
    /// Receivers for each node (held until extract_receiver is called)
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

    /// Extract the receiver for a specific node
    /// This removes the receiver from the internal map, so it can only be called once per node
    pub async fn extract_receiver(
        &self,
        node_id: u64,
    ) -> Result<mpsc::UnboundedReceiver<M>, Box<dyn std::error::Error>> {
        let mut receivers = self.node_receivers.lock().await;
        receivers.remove(&node_id)
            .ok_or_else(|| format!("Node {} receiver already extracted or doesn't exist", node_id).into())
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
    use crate::raft::generic::message::Message;

    #[tokio::test]
    async fn test_in_memory_transport_creation() {
        let transport = InMemoryClusterTransport::<Message<String>>::new(vec![1, 2, 3]);
        assert_eq!(transport.list_nodes().len(), 3);
    }

    #[tokio::test]
    async fn test_in_memory_transport_extract_receiver() {
        let transport = InMemoryClusterTransport::<Message<String>>::new(vec![1, 2, 3]);

        // Extract receiver for node 1
        let receiver1 = transport.extract_receiver(1).await;
        assert!(receiver1.is_ok(), "Should extract receiver for node 1");

        // Try to extract same receiver again - should fail
        let receiver1_again = transport.extract_receiver(1).await;
        assert!(receiver1_again.is_err(), "Should not allow extracting same receiver twice");
    }
}
