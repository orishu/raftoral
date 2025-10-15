use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use tonic::Status;
use crate::raft::generic::message::Message;
use crate::grpc::server::raft_proto::GenericMessage;
use crate::workflow::WorkflowCommand;
use crate::nodemanager::ManagementCommand;

/// ClusterRouter manages routing of GenericMessages to appropriate clusters
/// based on cluster_id field:
/// - cluster_id = 0: Management cluster
/// - cluster_id != 0: Execution cluster (by ID)
pub struct ClusterRouter {
    /// Management cluster sender (cluster_id = 0)
    management_sender: Option<mpsc::UnboundedSender<Message<ManagementCommand>>>,

    /// Execution cluster senders (cluster_id != 0)
    execution_senders: Arc<RwLock<HashMap<u64, mpsc::UnboundedSender<Message<WorkflowCommand>>>>>,
}

impl ClusterRouter {
    /// Create a new ClusterRouter with optional management cluster
    pub fn new() -> Self {
        Self {
            management_sender: None,
            execution_senders: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register the management cluster sender (cluster_id = 0)
    pub fn register_management_cluster(
        &mut self,
        sender: mpsc::UnboundedSender<Message<ManagementCommand>>,
    ) {
        self.management_sender = Some(sender);
    }

    /// Register an execution cluster sender with a specific cluster_id
    pub fn register_execution_cluster(
        &self,
        cluster_id: u64,
        sender: mpsc::UnboundedSender<Message<WorkflowCommand>>,
    ) -> Result<(), String> {
        if cluster_id == 0 {
            return Err("cluster_id=0 is reserved for management cluster".to_string());
        }

        let mut senders = self.execution_senders.write().unwrap();
        if senders.contains_key(&cluster_id) {
            return Err(format!("Cluster {} already registered", cluster_id));
        }

        senders.insert(cluster_id, sender);
        Ok(())
    }

    /// Unregister an execution cluster
    pub fn unregister_execution_cluster(&self, cluster_id: u64) -> Result<(), String> {
        if cluster_id == 0 {
            return Err("Cannot unregister management cluster through this method".to_string());
        }

        let mut senders = self.execution_senders.write().unwrap();
        if senders.remove(&cluster_id).is_none() {
            return Err(format!("Cluster {} not found", cluster_id));
        }

        Ok(())
    }

    /// Route a GenericMessage to the appropriate cluster based on cluster_id
    pub async fn route_message(&self, generic_msg: GenericMessage) -> Result<(), Status> {
        let cluster_id = generic_msg.cluster_id;

        match cluster_id {
            0 => {
                // Management cluster
                if let Some(sender) = &self.management_sender {
                    let message = Message::<ManagementCommand>::from_protobuf(generic_msg)
                        .map_err(|e| Status::invalid_argument(format!("Failed to deserialize management message: {}", e)))?;

                    sender.send(message)
                        .map_err(|e| Status::internal(format!("Failed to send to management cluster: {}", e)))?;

                    Ok(())
                } else {
                    Err(Status::unavailable("Management cluster not available"))
                }
            }
            cluster_id => {
                // Execution cluster
                let senders = self.execution_senders.read().unwrap();
                if let Some(sender) = senders.get(&cluster_id) {
                    let message = Message::<WorkflowCommand>::from_protobuf(generic_msg)
                        .map_err(|e| Status::invalid_argument(format!("Failed to deserialize workflow message: {}", e)))?;

                    sender.send(message)
                        .map_err(|e| Status::internal(format!("Failed to send to execution cluster {}: {}", cluster_id, e)))?;

                    Ok(())
                } else {
                    Err(Status::not_found(format!("Execution cluster {} not found", cluster_id)))
                }
            }
        }
    }

    /// Get the number of registered execution clusters
    pub fn execution_cluster_count(&self) -> usize {
        self.execution_senders.read().unwrap().len()
    }

    /// Check if management cluster is registered
    pub fn has_management_cluster(&self) -> bool {
        self.management_sender.is_some()
    }

    /// Get list of registered execution cluster IDs
    pub fn execution_cluster_ids(&self) -> Vec<u64> {
        self.execution_senders.read().unwrap().keys().copied().collect()
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

    #[test]
    fn test_cluster_router_creation() {
        let router = ClusterRouter::new();
        assert_eq!(router.execution_cluster_count(), 0);
        assert!(!router.has_management_cluster());
    }

    #[test]
    fn test_register_execution_cluster() {
        let router = ClusterRouter::new();
        let (sender, _receiver) = mpsc::unbounded_channel();

        // Should succeed
        router.register_execution_cluster(1, sender).unwrap();
        assert_eq!(router.execution_cluster_count(), 1);
        assert_eq!(router.execution_cluster_ids(), vec![1]);
    }

    #[test]
    fn test_register_duplicate_cluster() {
        let router = ClusterRouter::new();
        let (sender1, _receiver1) = mpsc::unbounded_channel();
        let (sender2, _receiver2) = mpsc::unbounded_channel();

        router.register_execution_cluster(1, sender1).unwrap();
        let result = router.register_execution_cluster(1, sender2);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already registered"));
    }

    #[test]
    fn test_register_cluster_id_zero() {
        let router = ClusterRouter::new();
        let (sender, _receiver) = mpsc::unbounded_channel();

        let result = router.register_execution_cluster(0, sender);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("reserved for management"));
    }

    #[test]
    fn test_unregister_execution_cluster() {
        let router = ClusterRouter::new();
        let (sender, _receiver) = mpsc::unbounded_channel();

        router.register_execution_cluster(1, sender).unwrap();
        assert_eq!(router.execution_cluster_count(), 1);

        router.unregister_execution_cluster(1).unwrap();
        assert_eq!(router.execution_cluster_count(), 0);
    }

    #[test]
    fn test_unregister_nonexistent_cluster() {
        let router = ClusterRouter::new();
        let result = router.unregister_execution_cluster(99);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_management_cluster_registration() {
        let mut router = ClusterRouter::new();
        let (sender, _receiver) = mpsc::unbounded_channel();

        assert!(!router.has_management_cluster());
        router.register_management_cluster(sender);
        assert!(router.has_management_cluster());
    }

    #[tokio::test]
    async fn test_route_to_execution_cluster() {
        use crate::grpc::server::raft_proto;

        let router = ClusterRouter::new();
        let (sender, mut receiver) = mpsc::unbounded_channel();

        // Register execution cluster with id=1
        router.register_execution_cluster(1, sender).unwrap();

        // Create a GenericMessage for cluster_id=1
        let proto_msg = raft_proto::GenericMessage {
            cluster_id: 1,
            message: Some(raft_proto::generic_message::Message::Campaign(
                raft_proto::CampaignMessage {}
            )),
        };

        // Route the message
        router.route_message(proto_msg).await.unwrap();

        // Verify the message was received
        let received = receiver.try_recv();
        assert!(received.is_ok(), "Message should be routed to execution cluster");
    }

    #[tokio::test]
    async fn test_route_to_nonexistent_cluster() {
        use crate::grpc::server::raft_proto;

        let router = ClusterRouter::new();

        // Create a GenericMessage for non-existent cluster_id=999
        let proto_msg = raft_proto::GenericMessage {
            cluster_id: 999,
            message: Some(raft_proto::generic_message::Message::Campaign(
                raft_proto::CampaignMessage {}
            )),
        };

        // Route the message - should fail
        let result = router.route_message(proto_msg).await;
        assert!(result.is_err(), "Routing to non-existent cluster should fail");

        let err = result.unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
        assert!(err.message().contains("not found"));
    }

    #[tokio::test]
    async fn test_multiple_execution_clusters() {
        use crate::grpc::server::raft_proto;

        let router = ClusterRouter::new();
        let (sender1, mut receiver1) = mpsc::unbounded_channel();
        let (sender2, mut receiver2) = mpsc::unbounded_channel();

        // Register two execution clusters
        router.register_execution_cluster(1, sender1).unwrap();
        router.register_execution_cluster(2, sender2).unwrap();

        assert_eq!(router.execution_cluster_count(), 2);

        // Route to cluster 1
        let proto_msg1 = raft_proto::GenericMessage {
            cluster_id: 1,
            message: Some(raft_proto::generic_message::Message::Campaign(
                raft_proto::CampaignMessage {}
            )),
        };
        router.route_message(proto_msg1).await.unwrap();

        // Route to cluster 2
        let proto_msg2 = raft_proto::GenericMessage {
            cluster_id: 2,
            message: Some(raft_proto::generic_message::Message::Campaign(
                raft_proto::CampaignMessage {}
            )),
        };
        router.route_message(proto_msg2).await.unwrap();

        // Verify messages went to correct clusters
        assert!(receiver1.try_recv().is_ok(), "Cluster 1 should receive message");
        assert!(receiver2.try_recv().is_ok(), "Cluster 2 should receive message");
    }
}
