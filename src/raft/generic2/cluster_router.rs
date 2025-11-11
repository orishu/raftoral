//! Cluster Router (Layer 2)
//!
//! Routes incoming Raft messages to the correct cluster based on cluster_id.

use crate::grpc2::proto::GenericMessage;
use crate::raft::generic2::errors::RoutingError;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Routes incoming Raft messages to the correct cluster
///
/// Each cluster registers its mailbox sender with the router.
/// When a message arrives, the router looks up the cluster_id and
/// forwards the message to the appropriate cluster's mailbox.
pub struct ClusterRouter {
    /// Map: cluster_id → mpsc::Sender for that cluster's RaftNode
    routes: Arc<Mutex<HashMap<u32, mpsc::Sender<GenericMessage>>>>,
}

impl ClusterRouter {
    /// Create a new ClusterRouter
    pub fn new() -> Self {
        Self {
            routes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a cluster with the router
    ///
    /// # Arguments
    /// * `cluster_id` - The cluster ID (0 = management, 1+ = execution clusters)
    /// * `sender` - The mpsc::Sender for sending messages to this cluster's RaftNode
    pub async fn register_cluster(&self, cluster_id: u32, sender: mpsc::Sender<GenericMessage>) {
        let mut routes = self.routes.lock().await;
        routes.insert(cluster_id, sender);
    }

    /// Unregister a cluster from the router
    ///
    /// # Arguments
    /// * `cluster_id` - The cluster ID to remove
    pub async fn unregister_cluster(&self, cluster_id: u32) {
        let mut routes = self.routes.lock().await;
        routes.remove(&cluster_id);
    }

    /// Route a message to the appropriate cluster
    ///
    /// This looks up the cluster_id in the message and forwards it
    /// to the registered cluster's mailbox.
    ///
    /// # Arguments
    /// * `message` - The GenericMessage containing cluster_id and payload
    ///
    /// # Returns
    /// * `Ok(())` if message was successfully routed
    /// * `Err(RoutingError)` if cluster not found or mailbox full
    pub async fn route_message(&self, message: GenericMessage) -> Result<(), RoutingError> {
        let cluster_id = message.cluster_id;

        // Look up the sender for this cluster
        let sender = {
            let routes = self.routes.lock().await;
            routes.get(&cluster_id).cloned()
        };

        match sender {
            Some(tx) => {
                // Send to cluster's mailbox
                tx.send(message).await.map_err(|_| RoutingError::MailboxFull {
                    cluster_id,
                })?;
                Ok(())
            }
            None => Err(RoutingError::ClusterNotFound { cluster_id }),
        }
    }

    /// Get the number of registered clusters
    pub async fn cluster_count(&self) -> usize {
        self.routes.lock().await.len()
    }

    /// Check if a cluster is registered
    pub async fn is_cluster_registered(&self, cluster_id: u32) -> bool {
        self.routes.lock().await.contains_key(&cluster_id)
    }
}

impl Default for ClusterRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_cluster_router_new() {
        let router = ClusterRouter::new();
        assert_eq!(router.cluster_count().await, 0);
    }

    #[tokio::test]
    async fn test_cluster_router_register() {
        let router = ClusterRouter::new();

        let (tx1, _rx1) = mpsc::channel(10);
        let (tx2, _rx2) = mpsc::channel(10);

        router.register_cluster(0, tx1).await;
        router.register_cluster(1, tx2).await;

        assert_eq!(router.cluster_count().await, 2);
        assert!(router.is_cluster_registered(0).await);
        assert!(router.is_cluster_registered(1).await);
    }

    #[tokio::test]
    async fn test_cluster_router_unregister() {
        let router = ClusterRouter::new();

        let (tx1, _rx1) = mpsc::channel(10);
        let (tx2, _rx2) = mpsc::channel(10);

        router.register_cluster(0, tx1).await;
        router.register_cluster(1, tx2).await;

        assert_eq!(router.cluster_count().await, 2);

        router.unregister_cluster(0).await;

        assert_eq!(router.cluster_count().await, 1);
        assert!(!router.is_cluster_registered(0).await);
        assert!(router.is_cluster_registered(1).await);
    }

    #[tokio::test]
    async fn test_cluster_router_route_message() {
        let router = ClusterRouter::new();

        let (tx, mut rx) = mpsc::channel(10);
        router.register_cluster(42, tx).await;

        // Create a message for cluster 42
        let msg = GenericMessage {
            cluster_id: 42,
            message: None,
        };

        // Route the message
        router.route_message(msg.clone()).await.unwrap();

        // Verify it was received
        let received = rx.recv().await.unwrap();
        assert_eq!(received.cluster_id, 42);
    }

    #[tokio::test]
    async fn test_cluster_router_route_to_unknown_cluster() {
        let router = ClusterRouter::new();

        // Create a message for non-existent cluster
        let msg = GenericMessage {
            cluster_id: 99,
            message: None,
        };

        // Try to route
        let result = router.route_message(msg).await;

        assert!(result.is_err());
        match result {
            Err(RoutingError::ClusterNotFound { cluster_id }) => {
                assert_eq!(cluster_id, 99);
            }
            _ => panic!("Expected ClusterNotFound error"),
        }
    }

    #[tokio::test]
    async fn test_cluster_router_multiple_messages() {
        let router = ClusterRouter::new();

        let (tx1, mut rx1) = mpsc::channel(10);
        let (tx2, mut rx2) = mpsc::channel(10);

        router.register_cluster(0, tx1).await;
        router.register_cluster(1, tx2).await;

        // Send messages to both clusters
        for _ in 0..5 {
            let msg = GenericMessage {
                cluster_id: 0,
                message: None,
            };
            router.route_message(msg).await.unwrap();

            let msg = GenericMessage {
                cluster_id: 1,
                message: None,
            };
            router.route_message(msg).await.unwrap();
        }

        // Verify cluster 0 received 5 messages
        let mut count = 0;
        while rx1.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(count, 5);

        // Verify cluster 1 received 5 messages
        let mut count = 0;
        while rx2.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(count, 5);
    }

    #[tokio::test]
    async fn test_cluster_router_mailbox_closed() {
        let router = ClusterRouter::new();

        // Create a channel and immediately drop the receiver to close it
        let (tx, _rx) = mpsc::channel(2);
        drop(_rx); // Explicitly close the channel

        router.register_cluster(0, tx).await;

        // Try to send a message to the closed channel
        let msg = GenericMessage {
            cluster_id: 0,
            message: None,
        };

        let result = router.route_message(msg).await;

        assert!(result.is_err());
        match result {
            Err(RoutingError::MailboxFull { cluster_id }) => {
                assert_eq!(cluster_id, 0);
            }
            _ => panic!("Expected MailboxFull error"),
        }
    }
}
