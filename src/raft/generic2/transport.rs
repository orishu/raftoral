//! Protocol-agnostic transport layer (Layer 1)
//!
//! This module provides an abstract interface for sending and receiving Raft messages
//! without tying to a specific protocol (gRPC, HTTP, or InProcess).

use crate::grpc::server::raft_proto::GenericMessage;
use crate::raft::generic2::errors::TransportError;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Protocol-specific message sender trait
///
/// This trait is implemented by each server layer (gRPC, HTTP, InProcess)
/// to provide actual message sending functionality.
#[tonic::async_trait]
pub trait MessageSender: Send + Sync {
    /// Send a message to a peer at the given address
    ///
    /// # Arguments
    /// * `address` - Protocol-specific address string (e.g., "192.168.1.10:5001" for gRPC)
    /// * `message` - The GenericMessage to send
    async fn send(&self, address: &str, message: GenericMessage) -> Result<(), TransportError>;
}

/// Transport layer trait (Layer 1)
///
/// Provides protocol-agnostic message sending/receiving interface.
/// The actual protocol (gRPC/HTTP/InProcess) is determined by the MessageSender implementation.
#[tonic::async_trait]
pub trait Transport: Send + Sync {
    /// Send a message to a peer node
    async fn send_message(
        &self,
        target_node_id: u64,
        message: GenericMessage,
    ) -> Result<(), TransportError>;

    /// Receive a message from a peer (called by Server layer)
    ///
    /// This is typically called by the gRPC/HTTP server when it receives
    /// a message, which then forwards it to the ClusterRouter.
    async fn receive_message(&self, message: GenericMessage) -> Result<(), TransportError>;

    /// Add a peer to the registry
    async fn add_peer(&self, node_id: u64, address: String);

    /// Remove a peer from the registry
    async fn remove_peer(&self, node_id: u64);

    /// List all peer node IDs
    async fn list_peers(&self) -> Vec<u64>;

    /// Get peer address by node ID
    async fn get_peer_address(&self, node_id: u64) -> Option<String>;
}

/// Concrete implementation of Transport layer
///
/// This maintains the peer registry and delegates actual message sending
/// to the protocol-specific MessageSender.
pub struct TransportLayer {
    /// Peer registry: node_id → address (protocol-agnostic string)
    peers: Arc<Mutex<HashMap<u64, String>>>,

    /// Protocol-specific message sender (injected by server layer)
    message_sender: Arc<dyn MessageSender>,

    /// Optional callback for receiving messages
    /// This is set by the ClusterRouter to receive incoming messages
    receive_callback: Arc<Mutex<Option<Arc<dyn Fn(GenericMessage) -> Result<(), TransportError> + Send + Sync>>>>,
}

impl TransportLayer {
    /// Create a new TransportLayer with the given MessageSender
    pub fn new(message_sender: Arc<dyn MessageSender>) -> Self {
        Self {
            peers: Arc::new(Mutex::new(HashMap::new())),
            message_sender,
            receive_callback: Arc::new(Mutex::new(None)),
        }
    }

    /// Set the receive callback (typically called by ClusterRouter)
    pub async fn set_receive_callback<F>(&self, callback: F)
    where
        F: Fn(GenericMessage) -> Result<(), TransportError> + Send + Sync + 'static,
    {
        let mut cb = self.receive_callback.lock().await;
        *cb = Some(Arc::new(callback));
    }
}

#[tonic::async_trait]
impl Transport for TransportLayer {
    async fn send_message(
        &self,
        target_node_id: u64,
        message: GenericMessage,
    ) -> Result<(), TransportError> {
        // Look up peer address
        let address = {
            let peers = self.peers.lock().await;
            peers.get(&target_node_id).cloned()
        };

        match address {
            Some(addr) => {
                // Send via protocol-specific sender
                self.message_sender.send(&addr, message).await
            }
            None => Err(TransportError::PeerNotFound {
                node_id: target_node_id,
            }),
        }
    }

    async fn receive_message(&self, message: GenericMessage) -> Result<(), TransportError> {
        // Forward to receive callback (ClusterRouter)
        let callback = self.receive_callback.lock().await;
        match callback.as_ref() {
            Some(cb) => cb(message),
            None => {
                // No callback registered yet, this is okay during initialization
                Ok(())
            }
        }
    }

    async fn add_peer(&self, node_id: u64, address: String) {
        let mut peers = self.peers.lock().await;
        peers.insert(node_id, address);
    }

    async fn remove_peer(&self, node_id: u64) {
        let mut peers = self.peers.lock().await;
        peers.remove(&node_id);
    }

    async fn list_peers(&self) -> Vec<u64> {
        let peers = self.peers.lock().await;
        peers.keys().copied().collect()
    }

    async fn get_peer_address(&self, node_id: u64) -> Option<String> {
        let peers = self.peers.lock().await;
        peers.get(&node_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock MessageSender for testing
    struct MockMessageSender {
        sent_messages: Arc<Mutex<Vec<(String, GenericMessage)>>>,
    }

    impl MockMessageSender {
        fn new() -> Self {
            Self {
                sent_messages: Arc::new(Mutex::new(Vec::new())),
            }
        }

        async fn get_sent_messages(&self) -> Vec<(String, GenericMessage)> {
            self.sent_messages.lock().await.clone()
        }
    }

    #[tonic::async_trait]
    impl MessageSender for MockMessageSender {
        async fn send(&self, address: &str, message: GenericMessage) -> Result<(), TransportError> {
            self.sent_messages
                .lock()
                .await
                .push((address.to_string(), message));
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_transport_add_peer() {
        let sender = Arc::new(MockMessageSender::new());
        let transport = TransportLayer::new(sender);

        transport.add_peer(1, "192.168.1.1:5001".to_string()).await;
        transport.add_peer(2, "192.168.1.2:5001".to_string()).await;

        let peers = transport.list_peers().await;
        assert_eq!(peers.len(), 2);
        assert!(peers.contains(&1));
        assert!(peers.contains(&2));
    }

    #[tokio::test]
    async fn test_transport_remove_peer() {
        let sender = Arc::new(MockMessageSender::new());
        let transport = TransportLayer::new(sender);

        transport.add_peer(1, "192.168.1.1:5001".to_string()).await;
        transport.add_peer(2, "192.168.1.2:5001".to_string()).await;

        transport.remove_peer(1).await;

        let peers = transport.list_peers().await;
        assert_eq!(peers.len(), 1);
        assert!(peers.contains(&2));
        assert!(!peers.contains(&1));
    }

    #[tokio::test]
    async fn test_transport_send_message() {
        let sender = Arc::new(MockMessageSender::new());
        let transport = TransportLayer::new(sender.clone());

        // Add peer
        transport.add_peer(2, "192.168.1.2:5001".to_string()).await;

        // Create test message
        let msg = GenericMessage {
            cluster_id: 1,
            message: None,
        };

        // Send message
        let result = transport.send_message(2, msg.clone()).await;
        assert!(result.is_ok());

        // Verify message was sent to correct address
        let sent = sender.get_sent_messages().await;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "192.168.1.2:5001");
        assert_eq!(sent[0].1.cluster_id, 1);
    }

    #[tokio::test]
    async fn test_transport_send_to_unknown_peer() {
        let sender = Arc::new(MockMessageSender::new());
        let transport = TransportLayer::new(sender);

        // Create test message
        let msg = GenericMessage {
            cluster_id: 1,
            message: None,
        };

        // Try to send to non-existent peer
        let result = transport.send_message(99, msg).await;
        assert!(result.is_err());

        match result {
            Err(TransportError::PeerNotFound { node_id }) => {
                assert_eq!(node_id, 99);
            }
            _ => panic!("Expected PeerNotFound error"),
        }
    }

    #[tokio::test]
    async fn test_transport_get_peer_address() {
        let sender = Arc::new(MockMessageSender::new());
        let transport = TransportLayer::new(sender);

        transport.add_peer(1, "192.168.1.1:5001".to_string()).await;

        let addr = transport.get_peer_address(1).await;
        assert_eq!(addr, Some("192.168.1.1:5001".to_string()));

        let addr = transport.get_peer_address(99).await;
        assert_eq!(addr, None);
    }

    /// Integration test: Multi-node communication via InProcessServer
    #[tokio::test]
    async fn test_multi_node_transport_integration() {
        use crate::raft::generic2::{InProcessServer, InProcessMessageSender};
        use tokio::sync::Mutex as TokioMutex;

        // Create in-process server
        let server = Arc::new(InProcessServer::new());

        // Create transports for two nodes
        let transport1 = Arc::new(TransportLayer::new(Arc::new(InProcessMessageSender::new(
            server.clone(),
        ))));
        let transport2 = Arc::new(TransportLayer::new(Arc::new(InProcessMessageSender::new(
            server.clone(),
        ))));

        // Track messages received by each node
        let node1_received = Arc::new(TokioMutex::new(Vec::new()));
        let node2_received = Arc::new(TokioMutex::new(Vec::new()));

        // Set up receive callbacks
        let node1_received_clone = node1_received.clone();
        transport1
            .set_receive_callback(move |msg| {
                let received = node1_received_clone.clone();
                tokio::spawn(async move {
                    received.lock().await.push(msg);
                });
                Ok(())
            })
            .await;

        let node2_received_clone = node2_received.clone();
        transport2
            .set_receive_callback(move |msg| {
                let received = node2_received_clone.clone();
                tokio::spawn(async move {
                    received.lock().await.push(msg);
                });
                Ok(())
            })
            .await;

        // Register nodes with server
        let transport1_clone = transport1.clone();
        server
            .register_node(1, move |msg| {
                let t = transport1_clone.clone();
                tokio::spawn(async move {
                    t.receive_message(msg).await
                });
                Ok(())
            })
            .await;

        let transport2_clone = transport2.clone();
        server
            .register_node(2, move |msg| {
                let t = transport2_clone.clone();
                tokio::spawn(async move {
                    t.receive_message(msg).await
                });
                Ok(())
            })
            .await;

        // Add peers to each transport
        transport1.add_peer(2, "node:2".to_string()).await;
        transport2.add_peer(1, "node:1".to_string()).await;

        // Node 1 sends message to Node 2
        let msg_to_2 = GenericMessage {
            cluster_id: 100,
            message: None,
        };
        transport1.send_message(2, msg_to_2.clone()).await.unwrap();

        // Node 2 sends message to Node 1
        let msg_to_1 = GenericMessage {
            cluster_id: 200,
            message: None,
        };
        transport2.send_message(1, msg_to_1.clone()).await.unwrap();

        // Wait for messages to be processed
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Verify Node 2 received message from Node 1
        let node2_msgs = node2_received.lock().await;
        assert_eq!(node2_msgs.len(), 1);
        assert_eq!(node2_msgs[0].cluster_id, 100);

        // Verify Node 1 received message from Node 2
        let node1_msgs = node1_received.lock().await;
        assert_eq!(node1_msgs.len(), 1);
        assert_eq!(node1_msgs[0].cluster_id, 200);
    }
}
