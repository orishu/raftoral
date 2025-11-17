//! HTTP Transport Implementation
//!
//! Implements the Transport trait using HTTP/REST protocol.

use crate::grpc::proto::GenericMessage;
use crate::http::HttpMessageSender;
use crate::raft::generic::errors::TransportError;
use crate::raft::generic::{ClusterRouter, MessageSender, Transport};
use slog::{debug, warn, Logger};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// HTTP-based transport implementation
pub struct HttpTransport {
    /// Maps node_id → address (e.g., "http://192.168.1.10:8001")
    peer_addresses: Arc<Mutex<HashMap<u64, String>>>,
    /// Routes messages to appropriate cluster
    cluster_router: Arc<ClusterRouter>,
    /// HTTP client for sending messages
    client: Arc<HttpMessageSender>,
    /// Logger
    logger: Logger,
}

impl HttpTransport {
    pub fn new(cluster_router: Arc<ClusterRouter>, logger: Logger) -> Self {
        Self {
            peer_addresses: Arc::new(Mutex::new(HashMap::new())),
            cluster_router,
            client: Arc::new(HttpMessageSender::new(logger.clone())),
            logger,
        }
    }
}

#[tonic::async_trait]
impl Transport for HttpTransport {
    async fn send_message(
        &self,
        target_node_id: u64,
        message: GenericMessage,
    ) -> Result<(), TransportError> {
        // Look up peer address
        let addresses = self.peer_addresses.lock().await;
        let address = addresses
            .get(&target_node_id)
            .ok_or_else(|| TransportError::PeerNotFound {
                node_id: target_node_id,
            })?
            .clone();
        drop(addresses);

        debug!(self.logger, "Sending HTTP message";
            "to_node" => target_node_id,
            "address" => &address,
            "cluster_id" => message.cluster_id
        );

        // Send via HTTP
        MessageSender::send(self.client.as_ref(), &address, message).await
    }

    async fn receive_message(&self, message: GenericMessage) -> Result<(), TransportError> {
        debug!(self.logger, "Received HTTP message";
            "cluster_id" => message.cluster_id
        );

        // Forward to cluster router
        self.cluster_router
            .route_message(message)
            .await
            .map_err(|e| TransportError::Other(e.to_string()))
    }

    async fn add_peer(&self, node_id: u64, address: String) {
        debug!(self.logger, "Adding HTTP peer";
            "node_id" => node_id,
            "address" => &address
        );
        self.peer_addresses.lock().await.insert(node_id, address);
    }

    async fn remove_peer(&self, node_id: u64) {
        debug!(self.logger, "Removing HTTP peer"; "node_id" => node_id);
        self.peer_addresses.lock().await.remove(&node_id);
    }

    async fn list_peers(&self) -> Vec<u64> {
        self.peer_addresses.lock().await.keys().copied().collect()
    }

    async fn get_peer_address(&self, node_id: u64) -> Option<String> {
        self.peer_addresses.lock().await.get(&node_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slog::{Drain, o};

    fn create_test_logger() -> Logger {
        let decorator = slog_term::PlainDecorator::new(std::io::stdout());
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        slog::Logger::root(drain, o!())
    }

    #[tokio::test]
    async fn test_http_transport_add_peer() {
        let logger = create_test_logger();
        let cluster_router = Arc::new(ClusterRouter::new());
        let transport = HttpTransport::new(cluster_router, logger);

        transport.add_peer(1, "http://localhost:8001".to_string()).await;
        transport.add_peer(2, "http://localhost:8002".to_string()).await;

        let peers = transport.list_peers().await;
        assert_eq!(peers.len(), 2);
        assert!(peers.contains(&1));
        assert!(peers.contains(&2));
    }

    #[tokio::test]
    async fn test_http_transport_remove_peer() {
        let logger = create_test_logger();
        let cluster_router = Arc::new(ClusterRouter::new());
        let transport = HttpTransport::new(cluster_router, logger);

        transport.add_peer(1, "http://localhost:8001".to_string()).await;
        transport.add_peer(2, "http://localhost:8002".to_string()).await;

        transport.remove_peer(1).await;

        let peers = transport.list_peers().await;
        assert_eq!(peers.len(), 1);
        assert!(peers.contains(&2));
        assert!(!peers.contains(&1));
    }

    #[tokio::test]
    async fn test_http_transport_get_peer_address() {
        let logger = create_test_logger();
        let cluster_router = Arc::new(ClusterRouter::new());
        let transport = HttpTransport::new(cluster_router, logger);

        transport.add_peer(1, "http://localhost:8001".to_string()).await;

        let addr = transport.get_peer_address(1).await;
        assert_eq!(addr, Some("http://localhost:8001".to_string()));

        let addr = transport.get_peer_address(99).await;
        assert_eq!(addr, None);
    }

    #[tokio::test]
    async fn test_http_transport_send_to_unknown_peer() {
        let logger = create_test_logger();
        let cluster_router = Arc::new(ClusterRouter::new());
        let transport = HttpTransport::new(cluster_router, logger);

        // Create test message
        let msg = GenericMessage {
            cluster_id: 1,
            message: None,
        };

        // Try to send to non-existent peer (should fail with PeerNotFound)
        let result = transport.send_message(99, msg).await;
        assert!(result.is_err());

        match result {
            Err(TransportError::PeerNotFound { node_id }) => {
                assert_eq!(node_id, 99);
            }
            _ => panic!("Expected PeerNotFound error"),
        }
    }
}
