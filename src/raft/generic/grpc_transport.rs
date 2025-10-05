use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, RwLock};
use crate::raft::generic::message::{Message, CommandExecutor};
use crate::raft::generic::cluster::RaftCluster;
use crate::raft::generic::transport::ClusterTransport;

/// Configuration for a node in the gRPC cluster
#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub node_id: u64,
    pub address: String,  // host:port
}

/// Generic gRPC-based transport for distributed Raft clusters
///
/// This transport enables Raft nodes to communicate over network via gRPC.
/// It maintains a mapping of node IDs to network addresses and provides
/// the infrastructure for sending Raft messages between nodes.
///
/// The transport is generic over the CommandExecutor type, but the actual
/// gRPC protocol is defined for specific command types (e.g., WorkflowCommand).
pub struct GrpcClusterTransport<E: CommandExecutor> {
    /// Node configurations (node_id -> address)
    nodes: Arc<RwLock<HashMap<u64, NodeConfig>>>,
    /// Receivers for each node (held until create_cluster is called)
    node_receivers: Arc<Mutex<HashMap<u64, mpsc::UnboundedReceiver<Message<E::Command>>>>>,
    /// Senders for each node (node_id -> sender to that node's receiver)
    node_senders: Arc<RwLock<HashMap<u64, mpsc::UnboundedSender<Message<E::Command>>>>>,
    /// Shutdown signal
    shutdown_tx: Arc<Mutex<Option<tokio::sync::broadcast::Sender<()>>>>,
}

impl<E: CommandExecutor> GrpcClusterTransport<E> {
    /// Create a new gRPC transport with the given node configurations
    pub fn new(nodes: Vec<NodeConfig>) -> Self {
        let mut node_map = HashMap::new();
        let mut node_senders = HashMap::new();
        let mut node_receivers_map = HashMap::new();

        // Create channels for each node
        for node in nodes {
            let (sender, receiver) = mpsc::unbounded_channel();
            node_senders.insert(node.node_id, sender);
            node_receivers_map.insert(node.node_id, receiver);
            node_map.insert(node.node_id, node);
        }

        GrpcClusterTransport {
            nodes: Arc::new(RwLock::new(node_map)),
            node_receivers: Arc::new(Mutex::new(node_receivers_map)),
            node_senders: Arc::new(RwLock::new(node_senders)),
            shutdown_tx: Arc::new(Mutex::new(None)),
        }
    }

    /// Add a new node to the transport configuration
    pub async fn add_node(&self, node: NodeConfig) -> Result<(), Box<dyn std::error::Error>> {
        let mut nodes = self.nodes.write().await;
        let mut senders = self.node_senders.write().await;
        let mut receivers = self.node_receivers.lock().await;

        // Create channel for the new node
        let (sender, receiver) = mpsc::unbounded_channel();

        senders.insert(node.node_id, sender);
        receivers.insert(node.node_id, receiver);
        nodes.insert(node.node_id, node);

        Ok(())
    }

    /// Remove a node from the transport configuration
    pub async fn remove_node(&self, node_id: u64) -> Result<(), Box<dyn std::error::Error>> {
        let mut nodes = self.nodes.write().await;
        let mut senders = self.node_senders.write().await;
        let mut receivers = self.node_receivers.lock().await;

        nodes.remove(&node_id);
        senders.remove(&node_id);
        receivers.remove(&node_id);

        Ok(())
    }

    /// Get the address for a specific node
    pub async fn get_node_address(&self, node_id: u64) -> Option<String> {
        let nodes = self.nodes.read().await;
        nodes.get(&node_id).map(|n| n.address.clone())
    }

    /// Get all node configurations
    pub async fn get_all_nodes(&self) -> Vec<NodeConfig> {
        let nodes = self.nodes.read().await;
        nodes.values().cloned().collect()
    }

    /// Internal method to get a sender for a specific node
    pub async fn get_node_sender(&self, node_id: u64) -> Option<mpsc::UnboundedSender<Message<E::Command>>> {
        let senders = self.node_senders.read().await;
        senders.get(&node_id).cloned()
    }
}

impl<E: CommandExecutor + Default + 'static> ClusterTransport<E> for GrpcClusterTransport<E> {
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
        let senders = self.node_senders.read().await;
        let mut peer_senders = HashMap::new();
        for (&peer_id, sender) in senders.iter() {
            if peer_id != node_id {
                peer_senders.insert(peer_id, sender.clone());
            }
        }

        // Get the sender for this node (for cluster's propose methods to use)
        let self_sender = senders.get(&node_id)
            .ok_or_else(|| format!("Node {} sender not found", node_id))?
            .clone();

        drop(senders); // Release the read lock

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
        let nodes = self.nodes.read().await;
        nodes.keys().copied().collect()
    }

    async fn start(&self) -> Result<(), Box<dyn std::error::Error>> {
        // For now, we don't need background tasks since gRPC clients
        // will send messages directly to node senders
        // In the full implementation, this would start the gRPC servers
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), Box<dyn std::error::Error>> {
        let shutdown_tx = self.shutdown_tx.lock().await;
        if let Some(tx) = shutdown_tx.as_ref() {
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
    async fn test_grpc_transport_creation() {
        let nodes = vec![
            NodeConfig { node_id: 1, address: "127.0.0.1:5001".to_string() },
            NodeConfig { node_id: 2, address: "127.0.0.1:5002".to_string() },
        ];

        let transport = GrpcClusterTransport::<TestExecutor>::new(nodes);

        // Verify we can get node addresses
        assert_eq!(
            transport.get_node_address(1).await,
            Some("127.0.0.1:5001".to_string())
        );
        assert_eq!(
            transport.get_node_address(2).await,
            Some("127.0.0.1:5002".to_string())
        );
    }

    #[tokio::test]
    async fn test_add_remove_node() {
        let transport = GrpcClusterTransport::<TestExecutor>::new(vec![]);

        // Add a node
        let node = NodeConfig { node_id: 1, address: "127.0.0.1:5001".to_string() };
        transport.add_node(node).await.expect("Should add node");

        assert_eq!(
            transport.get_node_address(1).await,
            Some("127.0.0.1:5001".to_string())
        );

        // Remove the node
        transport.remove_node(1).await.expect("Should remove node");
        assert_eq!(transport.get_node_address(1).await, None);
    }
}
