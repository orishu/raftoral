///! NodeManager - owns both management and workflow execution clusters

use std::sync::Arc;
use crate::raft::RaftCluster;
use crate::raft::generic::grpc_transport::GrpcClusterTransport;
use crate::raft::generic::message::Message;
use crate::workflow::{WorkflowCommand, WorkflowCommandExecutor, WorkflowRuntime};
use super::{ManagementCommand, ManagementCommandExecutor};

/// NodeManager owns both the management cluster and workflow execution cluster(s)
/// In future milestones, the workflow cluster will become a HashMap for multiple execution clusters
pub struct NodeManager {
    /// Management cluster - tracks execution cluster membership and workflow lifecycle
    management_cluster: Arc<RaftCluster<ManagementCommandExecutor>>,

    /// Workflow execution cluster (will become a HashMap in future milestones)
    pub workflow_cluster: Arc<RaftCluster<WorkflowCommandExecutor>>,

    /// Workflow runtime (public API for starting workflows)
    workflow_runtime: Arc<WorkflowRuntime>,

    /// ClusterRouter for routing incoming gRPC messages to appropriate clusters
    cluster_router: Arc<crate::grpc::ClusterRouter>,
}

impl NodeManager {
    /// Create a new NodeManager by creating both clusters from a shared transport
    ///
    /// Phase 3: Transport is now type-parameter-free!
    /// The transport can now truly be shared between different cluster types.
    pub async fn new(
        transport: Arc<GrpcClusterTransport>,
        node_id: u64,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Create workflow cluster (cluster_id = 1)
        // RaftCluster::new creates its own mailbox internally
        let executor = WorkflowCommandExecutor::default();
        let transport_ref: Arc<dyn crate::raft::generic::transport::TransportInteraction<Message<WorkflowCommand>>> = transport.clone();
        let workflow_cluster = Arc::new(RaftCluster::new(node_id, 1, transport_ref, executor).await?);

        // Create workflow runtime
        let workflow_runtime = WorkflowRuntime::new(workflow_cluster.clone());

        // Set the runtime reference in the executor so it can spawn workflows
        workflow_cluster.executor.set_runtime(workflow_runtime.clone());

        // Create management cluster (cluster_id = 0) using the same transport
        // Phase 3: Transport is now type-parameter-free and can be shared!
        let management_executor = ManagementCommandExecutor::default();
        let management_transport_ref: Arc<dyn crate::raft::generic::transport::TransportInteraction<Message<ManagementCommand>>> = transport.clone();
        let management_cluster = Arc::new(RaftCluster::new(node_id, 0, management_transport_ref, management_executor).await?);

        // Set cluster references in management executor for dynamic cluster construction
        management_cluster.executor.set_workflow_cluster(workflow_cluster.clone());
        management_cluster.executor.set_management_cluster(management_cluster.clone());

        // Create ClusterRouter and register both clusters
        // This enables multi-cluster routing where:
        // - cluster_id = 0 routes to management cluster
        // - cluster_id = 1 (default) routes to workflow execution cluster
        use crate::raft::generic::ClusterRouter;
        let mut router = ClusterRouter::new();
        router.register_management_cluster(management_cluster.local_sender.clone());
        router.register_execution_cluster(1, workflow_cluster.local_sender.clone())?;
        let cluster_router = Arc::new(router);

        Ok(Self {
            management_cluster,
            workflow_cluster,
            workflow_runtime,
            cluster_router,
        })
    }

    /// Get the cluster router for message routing
    pub fn cluster_router(&self) -> Arc<crate::raft::generic::ClusterRouter> {
        self.cluster_router.clone()
    }

    /// Get the workflow runtime for public API access
    pub fn workflow_runtime(&self) -> Arc<WorkflowRuntime> {
        self.workflow_runtime.clone()
    }

    /// Add a node to the management cluster
    /// The workflow cluster will be updated automatically via the management executor's
    /// on_node_added callback when the ConfChange is applied
    pub async fn add_node(&self, node_id: u64, address: String) -> Result<(), String> {
        self.management_cluster.add_node(node_id, address).await
            .map_err(|e| format!("Failed to add node to management cluster: {}", e))
    }

    /// Remove a node from the management cluster
    /// The workflow cluster will be updated automatically via the management executor's
    /// on_node_removed callback when the ConfChange is applied
    pub async fn remove_node(&self, node_id: u64) -> Result<(), String> {
        self.management_cluster.remove_node(node_id).await
            .map_err(|e| format!("Failed to remove node from management cluster: {}", e))
    }

    /// Get cluster size (workflow cluster for now)
    pub fn cluster_size(&self) -> usize {
        self.workflow_cluster.node_count()
    }
}
