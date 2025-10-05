use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use crate::raft::generic::message::{Message, CommandExecutor};
use crate::raft::generic::cluster::RaftCluster;

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
pub trait ClusterTransport<E: CommandExecutor>: Send + Sync {
    /// Create a RaftCluster instance for a specific node
    ///
    /// # Arguments
    /// * `node_id` - The unique ID of the node to create
    ///
    /// # Returns
    /// A RaftCluster instance configured to communicate with peers via this transport
    fn create_cluster(
        &self,
        node_id: u64,
    ) -> impl std::future::Future<Output = Result<Arc<RaftCluster<E>>, Box<dyn std::error::Error>>> + Send;

    /// Get the list of all node IDs in this transport configuration
    fn node_ids(&self) -> impl std::future::Future<Output = Vec<u64>> + Send;

    /// Start the transport layer (e.g., background message routing tasks)
    fn start(&self) -> impl std::future::Future<Output = Result<(), Box<dyn std::error::Error>>> + Send;

    /// Shutdown the transport layer gracefully
    fn shutdown(&self) -> impl std::future::Future<Output = Result<(), Box<dyn std::error::Error>>> + Send;
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
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Create transport for 3-node cluster
/// let transport = InMemoryClusterTransport::<WorkflowCommandExecutor>::new(vec![1, 2, 3]);
/// transport.start().await?;
///
/// // Create cluster instances for each node (executors created internally)
/// let cluster1 = transport.create_cluster(1).await?;
/// let cluster2 = transport.create_cluster(2).await?;
/// let cluster3 = transport.create_cluster(3).await?;
/// # Ok(())
/// # }
/// ```
pub struct InMemoryClusterTransport<E: CommandExecutor> {
    /// Node IDs in this cluster
    node_ids: Vec<u64>,
    /// Senders for each node (node_id -> sender to that node's receiver)
    node_senders: HashMap<u64, mpsc::UnboundedSender<Message<E::Command>>>,
    /// Receivers for each node (held until create_cluster is called)
    node_receivers: Arc<tokio::sync::Mutex<HashMap<u64, mpsc::UnboundedReceiver<Message<E::Command>>>>>,
    /// Shutdown signal
    shutdown_tx: Option<tokio::sync::broadcast::Sender<()>>,
}

impl<E: CommandExecutor + 'static> InMemoryClusterTransport<E> {
    /// Create a new in-memory transport for the given node IDs
    pub fn new(node_ids: Vec<u64>) -> Self {
        let mut node_senders = HashMap::new();
        let mut node_receivers_map = HashMap::new();

        // Create unbounded channels for each node
        for &node_id in &node_ids {
            let (sender, receiver) = mpsc::unbounded_channel();
            node_senders.insert(node_id, sender);
            node_receivers_map.insert(node_id, receiver);
        }

        InMemoryClusterTransport {
            node_ids,
            node_senders,
            node_receivers: Arc::new(tokio::sync::Mutex::new(node_receivers_map)),
            shutdown_tx: None,
        }
    }
}

impl<E: CommandExecutor + Default + 'static> ClusterTransport<E> for InMemoryClusterTransport<E> {
    async fn create_cluster(
        &self,
        node_id: u64,
    ) -> Result<Arc<RaftCluster<E>>, Box<dyn std::error::Error>> {
        // Create a new executor instance
        let executor = E::default();
        // Extract the receiver for this node
        let receiver = {
            let mut receivers = self.node_receivers.lock().await;
            receivers.remove(&node_id)
                .ok_or_else(|| format!("Node {} receiver already claimed or doesn't exist", node_id))?
        };

        // Build peer senders (all OTHER nodes, excluding self)
        let mut peer_senders = HashMap::new();
        for (&peer_id, sender) in &self.node_senders {
            if peer_id != node_id {
                peer_senders.insert(peer_id, sender.clone());
            }
        }

        // Get the sender for this node (for cluster's propose methods to use)
        let self_sender = self.node_senders.get(&node_id)
            .ok_or_else(|| format!("Node {} sender not found", node_id))?
            .clone();

        // Create the cluster with the receiver, peer senders, and self sender
        let cluster = RaftCluster::new_with_transport(
            node_id,
            receiver,
            peer_senders,
            self_sender,
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
        let transport = InMemoryClusterTransport::<TestExecutor>::new(vec![1, 2, 3]);
        assert_eq!(transport.node_ids().await, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn test_in_memory_transport_cluster_creation() {
        let transport = InMemoryClusterTransport::<TestExecutor>::new(vec![1, 2, 3]);
        transport.start().await.expect("Transport start should succeed");

        // Create cluster for node 1
        let cluster1 = transport.create_cluster(1).await;
        assert!(cluster1.is_ok(), "Should create cluster for node 1");

        // Try to create same node again - should fail
        let cluster1_again = transport.create_cluster(1).await;
        assert!(cluster1_again.is_err(), "Should not allow creating same node twice");
    }
}
