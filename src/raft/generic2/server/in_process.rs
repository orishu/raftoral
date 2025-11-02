//! In-process server for testing (Layer 0)
//!
//! Provides in-memory message routing without actual network I/O.
//! This enables fast multi-node unit tests.

use crate::grpc::server::raft_proto::GenericMessage;
use crate::raft::generic2::errors::TransportError;
use crate::raft::generic2::transport::MessageSender;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// In-process server that routes messages between nodes in memory
///
/// This is used for testing multi-node scenarios without network overhead.
/// Messages sent to a node are directly delivered to that node's transport
/// via the receive callback.
pub struct InProcessServer {
    /// Registry of node_id → receive callback
    /// When a message is sent to a node, we call this callback
    nodes: Arc<Mutex<HashMap<u64, Arc<dyn Fn(GenericMessage) -> Result<(), TransportError> + Send + Sync>>>>,
}

impl InProcessServer {
    /// Create a new in-process server
    pub fn new() -> Self {
        Self {
            nodes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a node's receive callback
    ///
    /// This is called by the TransportLayer to register itself for receiving messages
    pub async fn register_node<F>(&self, node_id: u64, callback: F)
    where
        F: Fn(GenericMessage) -> Result<(), TransportError> + Send + Sync + 'static,
    {
        let mut nodes = self.nodes.lock().await;
        nodes.insert(node_id, Arc::new(callback));
    }

    /// Unregister a node
    pub async fn unregister_node(&self, node_id: u64) {
        let mut nodes = self.nodes.lock().await;
        nodes.remove(&node_id);
    }

    /// Send a message to a node (called by InProcessMessageSender)
    ///
    /// This looks up the target node's receive callback and invokes it directly
    pub async fn send_to_node(
        &self,
        target_node_id: u64,
        message: GenericMessage,
    ) -> Result<(), TransportError> {
        let nodes = self.nodes.lock().await;

        match nodes.get(&target_node_id) {
            Some(callback) => callback(message),
            None => Err(TransportError::PeerNotFound {
                node_id: target_node_id,
            }),
        }
    }

    /// Get the number of registered nodes
    pub async fn node_count(&self) -> usize {
        self.nodes.lock().await.len()
    }
}

impl Default for InProcessServer {
    fn default() -> Self {
        Self::new()
    }
}

/// MessageSender implementation for InProcessServer
///
/// This extracts the node_id from the address string and routes the message
/// through the InProcessServer.
pub struct InProcessMessageSender {
    server: Arc<InProcessServer>,
}

impl InProcessMessageSender {
    /// Create a new InProcessMessageSender
    pub fn new(server: Arc<InProcessServer>) -> Self {
        Self { server }
    }

    /// Parse node_id from address string
    ///
    /// For in-process testing, we use addresses like "node:1", "node:2", etc.
    /// This extracts the numeric node_id from the address.
    fn parse_node_id(address: &str) -> Result<u64, TransportError> {
        if let Some(node_id_str) = address.strip_prefix("node:") {
            node_id_str.parse::<u64>().map_err(|e| {
                TransportError::SerializationError {
                    reason: format!("Invalid node address '{}': {}", address, e),
                }
            })
        } else {
            Err(TransportError::SerializationError {
                reason: format!(
                    "Invalid in-process address '{}', expected format 'node:<id>'",
                    address
                ),
            })
        }
    }
}

#[tonic::async_trait]
impl MessageSender for InProcessMessageSender {
    async fn send(&self, address: &str, message: GenericMessage) -> Result<(), TransportError> {
        // Parse node_id from address
        let node_id = Self::parse_node_id(address)?;

        // Route message through server
        self.server.send_to_node(node_id, message).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    #[tokio::test]
    async fn test_in_process_server_register() {
        let server = InProcessServer::new();

        assert_eq!(server.node_count().await, 0);

        // Register node 1
        server
            .register_node(1, |_msg| Ok(()))
            .await;

        assert_eq!(server.node_count().await, 1);

        // Register node 2
        server
            .register_node(2, |_msg| Ok(()))
            .await;

        assert_eq!(server.node_count().await, 2);
    }

    #[tokio::test]
    async fn test_in_process_server_unregister() {
        let server = InProcessServer::new();

        server.register_node(1, |_msg| Ok(())).await;
        server.register_node(2, |_msg| Ok(())).await;

        assert_eq!(server.node_count().await, 2);

        server.unregister_node(1).await;

        assert_eq!(server.node_count().await, 1);
    }

    #[tokio::test]
    async fn test_in_process_server_send_to_node() {
        let server = Arc::new(InProcessServer::new());

        // Track received messages
        let received = Arc::new(TokioMutex::new(Vec::new()));
        let received_clone = received.clone();

        // Register node 1 with callback
        server
            .register_node(1, move |msg| {
                let received = received_clone.clone();
                tokio::spawn(async move {
                    received.lock().await.push(msg);
                });
                Ok(())
            })
            .await;

        // Send message to node 1
        let msg = GenericMessage {
            cluster_id: 42,
            message: None,
        };

        server.send_to_node(1, msg.clone()).await.unwrap();

        // Give callback time to execute
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Verify message was received
        let received_msgs = received.lock().await;
        assert_eq!(received_msgs.len(), 1);
        assert_eq!(received_msgs[0].cluster_id, 42);
    }

    #[tokio::test]
    async fn test_in_process_server_send_to_unknown_node() {
        let server = Arc::new(InProcessServer::new());

        let msg = GenericMessage {
            cluster_id: 1,
            message: None,
        };

        let result = server.send_to_node(99, msg).await;
        assert!(result.is_err());

        match result {
            Err(TransportError::PeerNotFound { node_id }) => {
                assert_eq!(node_id, 99);
            }
            _ => panic!("Expected PeerNotFound error"),
        }
    }

    #[tokio::test]
    async fn test_in_process_message_sender_parse_address() {
        assert_eq!(
            InProcessMessageSender::parse_node_id("node:1").unwrap(),
            1
        );
        assert_eq!(
            InProcessMessageSender::parse_node_id("node:42").unwrap(),
            42
        );

        assert!(InProcessMessageSender::parse_node_id("invalid").is_err());
        assert!(InProcessMessageSender::parse_node_id("node:abc").is_err());
    }

    #[tokio::test]
    async fn test_in_process_message_sender_send() {
        let server = Arc::new(InProcessServer::new());
        let sender = InProcessMessageSender::new(server.clone());

        // Track received messages
        let received = Arc::new(TokioMutex::new(Vec::new()));
        let received_clone = received.clone();

        // Register node 2
        server
            .register_node(2, move |msg| {
                let received = received_clone.clone();
                tokio::spawn(async move {
                    received.lock().await.push(msg);
                });
                Ok(())
            })
            .await;

        // Send message via MessageSender
        let msg = GenericMessage {
            cluster_id: 10,
            message: None,
        };

        sender.send("node:2", msg.clone()).await.unwrap();

        // Give callback time to execute
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Verify message was received
        let received_msgs = received.lock().await;
        assert_eq!(received_msgs.len(), 1);
        assert_eq!(received_msgs[0].cluster_id, 10);
    }
}
