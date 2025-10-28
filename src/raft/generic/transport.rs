use std::collections::HashMap;
use std::sync::Arc;
use std::any::Any;
use tokio::sync::mpsc;
use serde::{Serialize, Deserialize};

/// Unified interface for Raft nodes to interact with the transport layer
///
/// This trait provides all necessary operations for RaftCluster and RaftNode to
/// communicate with peers without directly accessing transport internals.
/// Generic over message type M - any type that can serve as a message.
///
/// Note: Requires Any for downcasting to concrete types (e.g., InMemoryClusterTransport)
/// in specific scenarios like testing transports that need special initialization.
pub trait TransportInteraction<M>: Send + Sync + Any
where
    M: Send + Sync + 'static,
{
    /// Send a message to a specific node with cluster ID for routing
    /// Returns error if node doesn't exist or send fails
    ///
    /// # Arguments
    /// * `target_node_id` - The node to send the message to
    /// * `message` - The message to send
    /// * `cluster_id` - The cluster ID for multi-cluster routing (0 = management, 1+ = execution)
    fn send_message_to_node(
        &self,
        target_node_id: u64,
        message: M,
        cluster_id: u32,
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
    /// Senders for each node (node_id -> sender to that node's mailbox)
    /// Populated when RaftCluster instances register themselves
    node_senders: Arc<std::sync::RwLock<HashMap<u64, mpsc::UnboundedSender<M>>>>,
}

impl<M> InMemoryClusterTransport<M>
where
    M: Send + Sync + 'static,
{
    /// Create a new in-memory transport
    pub fn new() -> Self {
        InMemoryClusterTransport {
            node_senders: Arc::new(std::sync::RwLock::new(HashMap::new())),
        }
    }

    /// Register a node's mailbox sender with the transport
    /// Called by RaftCluster during initialization
    pub fn register_node(&self, node_id: u64, sender: mpsc::UnboundedSender<M>) {
        self.node_senders.write().unwrap().insert(node_id, sender);
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
        message: M,
        _cluster_id: u32,  // In-memory transport doesn't use cluster_id (single process)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::generic::message::Message;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_in_memory_transport_creation() {
        let transport = InMemoryClusterTransport::<Message<String>>::new();
        assert_eq!(transport.list_nodes().len(), 0);
    }

    #[tokio::test]
    async fn test_in_memory_transport_register_node() {
        let transport = InMemoryClusterTransport::<Message<String>>::new();

        // Create a channel and register node
        let (sender, _receiver) = mpsc::unbounded_channel();
        transport.register_node(1, sender);

        assert_eq!(transport.list_nodes().len(), 1);
        assert!(transport.list_nodes().contains(&1));
    }
}
