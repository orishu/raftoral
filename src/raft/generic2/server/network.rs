//! In-process network for multi-node testing (Layer 0)
//!
//! Provides a shared message bus that connects multiple in-process nodes,
//! enabling full Raft consensus simulation in memory.

use crate::grpc::server::raft_proto::GenericMessage;
use crate::raft::generic2::errors::TransportError;
use crate::raft::generic2::transport::MessageSender;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Shared in-process network that routes messages between nodes
///
/// This acts as a message bus where each node registers its mailbox,
/// and messages are routed directly through async channels.
///
/// # Architecture
/// ```text
/// Node 1 ---send--> InProcessNetwork ---route--> Node 2's mailbox
/// Node 2 ---send--> InProcessNetwork ---route--> Node 1's mailbox
/// ```
///
/// # Usage
/// ```rust,ignore
/// let network = Arc::new(InProcessNetwork::new());
///
/// // Each node registers its mailbox sender
/// let (tx1, rx1) = mpsc::channel(100);
/// network.register_node(1, tx1).await;
///
/// let (tx2, rx2) = mpsc::channel(100);
/// network.register_node(2, tx2).await;
///
/// // Messages sent to node 2 will be routed to rx2
/// network.send_to_node(2, message).await?;
/// ```
pub struct InProcessNetwork {
    /// Routes: node_id → mailbox sender for that node
    routes: Arc<Mutex<HashMap<u64, mpsc::Sender<GenericMessage>>>>,
}

impl InProcessNetwork {
    /// Create a new in-process network
    pub fn new() -> Self {
        Self {
            routes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a node's mailbox sender with the network
    ///
    /// # Arguments
    /// * `node_id` - The ID of the node
    /// * `tx` - The sender half of the node's mailbox channel
    ///
    /// Messages sent to this node_id will be routed to this sender.
    pub async fn register_node(&self, node_id: u64, tx: mpsc::Sender<GenericMessage>) {
        self.routes.lock().await.insert(node_id, tx);
    }

    /// Unregister a node from the network
    pub async fn unregister_node(&self, node_id: u64) {
        self.routes.lock().await.remove(&node_id);
    }

    /// Send a message to a specific node
    ///
    /// This looks up the target node's mailbox and sends the message.
    /// Uses async send() so it will wait if the channel is temporarily full.
    ///
    /// # Arguments
    /// * `to_node_id` - Target node ID
    /// * `message` - The message to send
    ///
    /// # Returns
    /// * `Ok(())` - Message sent successfully
    /// * `Err(TransportError::PeerNotFound)` - Target node not registered
    /// * `Err(TransportError::SendFailed)` - Channel closed or send failed
    pub async fn send_to_node(
        &self,
        to_node_id: u64,
        message: GenericMessage,
    ) -> Result<(), TransportError> {
        // Look up the target node's sender
        let sender = {
            let routes = self.routes.lock().await;
            routes.get(&to_node_id).cloned()
        };

        match sender {
            Some(tx) => {
                // Send to the node's mailbox (async, will wait if needed)
                tx.send(message).await.map_err(|e| TransportError::SendFailed {
                    node_id: to_node_id,
                    reason: format!("Failed to send to node {}: {}", to_node_id, e),
                })
            }
            None => Err(TransportError::PeerNotFound {
                node_id: to_node_id,
            }),
        }
    }

    /// Get the number of registered nodes
    pub async fn node_count(&self) -> usize {
        self.routes.lock().await.len()
    }
}

impl Default for InProcessNetwork {
    fn default() -> Self {
        Self::new()
    }
}

/// MessageSender implementation for InProcessNetwork
///
/// This sends messages through the shared network by parsing the node ID
/// from the address string.
pub struct InProcessNetworkSender {
    network: Arc<InProcessNetwork>,
}

impl InProcessNetworkSender {
    /// Create a new InProcessNetworkSender
    pub fn new(network: Arc<InProcessNetwork>) -> Self {
        Self { network }
    }

    /// Parse node_id from address string
    ///
    /// For in-process testing, we accept formats like:
    /// - "node1", "node2" (simple format used in tests)
    /// - "node:1", "node:2" (explicit format)
    /// - "1", "2" (just the number)
    fn parse_node_id(address: &str) -> Result<u64, TransportError> {
        // Try parsing as direct number first
        if let Ok(id) = address.parse::<u64>() {
            return Ok(id);
        }

        // Try "node1" format
        if let Some(id_str) = address.strip_prefix("node") {
            if let Ok(id) = id_str.parse::<u64>() {
                return Ok(id);
            }
        }

        // Try "node:1" format
        if let Some(id_str) = address.strip_prefix("node:") {
            if let Ok(id) = id_str.parse::<u64>() {
                return Ok(id);
            }
        }

        Err(TransportError::SerializationError {
            reason: format!(
                "Invalid in-process address '{}', expected 'node<id>', 'node:<id>', or '<id>'",
                address
            ),
        })
    }
}

#[tonic::async_trait]
impl MessageSender for InProcessNetworkSender {
    async fn send(&self, address: &str, message: GenericMessage) -> Result<(), TransportError> {
        // Parse node_id from address
        let node_id = Self::parse_node_id(address)?;

        // Route message through network
        self.network.send_to_node(node_id, message).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_network_register_and_count() {
        let network = InProcessNetwork::new();
        assert_eq!(network.node_count().await, 0);

        let (tx1, _rx1) = mpsc::channel(10);
        network.register_node(1, tx1).await;
        assert_eq!(network.node_count().await, 1);

        let (tx2, _rx2) = mpsc::channel(10);
        network.register_node(2, tx2).await;
        assert_eq!(network.node_count().await, 2);
    }

    #[tokio::test]
    async fn test_network_unregister() {
        let network = InProcessNetwork::new();

        let (tx1, _rx1) = mpsc::channel(10);
        let (tx2, _rx2) = mpsc::channel(10);

        network.register_node(1, tx1).await;
        network.register_node(2, tx2).await;
        assert_eq!(network.node_count().await, 2);

        network.unregister_node(1).await;
        assert_eq!(network.node_count().await, 1);
    }

    #[tokio::test]
    async fn test_network_send_to_node() {
        let network = Arc::new(InProcessNetwork::new());

        let (tx1, mut rx1) = mpsc::channel(10);
        network.register_node(1, tx1).await;

        // Send message to node 1
        let msg = GenericMessage {
            cluster_id: 42,
            message: None,
        };

        network.send_to_node(1, msg.clone()).await.unwrap();

        // Verify message was received
        let received = rx1.recv().await.unwrap();
        assert_eq!(received.cluster_id, 42);
    }

    #[tokio::test]
    async fn test_network_send_to_unknown_node() {
        let network = Arc::new(InProcessNetwork::new());

        let msg = GenericMessage {
            cluster_id: 1,
            message: None,
        };

        let result = network.send_to_node(99, msg).await;
        assert!(result.is_err());

        match result {
            Err(TransportError::PeerNotFound { node_id }) => {
                assert_eq!(node_id, 99);
            }
            _ => panic!("Expected PeerNotFound error"),
        }
    }

    #[tokio::test]
    async fn test_network_multiple_messages() {
        let network = Arc::new(InProcessNetwork::new());

        let (tx1, mut rx1) = mpsc::channel(10);
        network.register_node(1, tx1).await;

        // Send multiple messages
        for i in 0..5 {
            let msg = GenericMessage {
                cluster_id: i,
                message: None,
            };
            network.send_to_node(1, msg).await.unwrap();
        }

        // Verify all messages received in order
        for i in 0..5 {
            let received = rx1.recv().await.unwrap();
            assert_eq!(received.cluster_id, i);
        }
    }

    #[tokio::test]
    async fn test_network_sender_parse_address() {
        assert_eq!(InProcessNetworkSender::parse_node_id("1").unwrap(), 1);
        assert_eq!(InProcessNetworkSender::parse_node_id("42").unwrap(), 42);
        assert_eq!(InProcessNetworkSender::parse_node_id("node1").unwrap(), 1);
        assert_eq!(InProcessNetworkSender::parse_node_id("node42").unwrap(), 42);
        assert_eq!(InProcessNetworkSender::parse_node_id("node:1").unwrap(), 1);
        assert_eq!(InProcessNetworkSender::parse_node_id("node:42").unwrap(), 42);

        assert!(InProcessNetworkSender::parse_node_id("invalid").is_err());
        assert!(InProcessNetworkSender::parse_node_id("nodeabc").is_err());
    }

    #[tokio::test]
    async fn test_network_sender_send() {
        let network = Arc::new(InProcessNetwork::new());
        let sender = InProcessNetworkSender::new(network.clone());

        let (tx1, mut rx1) = mpsc::channel(10);
        network.register_node(1, tx1).await;

        // Send via MessageSender trait
        let msg = GenericMessage {
            cluster_id: 99,
            message: None,
        };

        sender.send("node1", msg.clone()).await.unwrap();

        // Verify received
        let received = rx1.recv().await.unwrap();
        assert_eq!(received.cluster_id, 99);
    }

    #[tokio::test]
    async fn test_network_bidirectional_communication() {
        let network = Arc::new(InProcessNetwork::new());

        let (tx1, mut rx1) = mpsc::channel(10);
        let (tx2, mut rx2) = mpsc::channel(10);

        network.register_node(1, tx1).await;
        network.register_node(2, tx2).await;

        // Node 1 sends to Node 2
        let msg1to2 = GenericMessage {
            cluster_id: 12,
            message: None,
        };
        network.send_to_node(2, msg1to2).await.unwrap();

        // Node 2 sends to Node 1
        let msg2to1 = GenericMessage {
            cluster_id: 21,
            message: None,
        };
        network.send_to_node(1, msg2to1).await.unwrap();

        // Verify both received
        let received_at_2 = rx2.recv().await.unwrap();
        assert_eq!(received_at_2.cluster_id, 12);

        let received_at_1 = rx1.recv().await.unwrap();
        assert_eq!(received_at_1.cluster_id, 21);
    }
}
