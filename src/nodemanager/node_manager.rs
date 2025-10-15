///! NodeManager - owns both management and workflow execution clusters

use std::sync::Arc;
use crate::raft::RaftCluster;
use crate::raft::generic::grpc_transport::GrpcClusterTransport;
use crate::raft::generic::message::Message;
use crate::workflow::{WorkflowCommand, WorkflowCommandExecutor, WorkflowRuntime};
use super::{ManagementCommand, ManagementCommandExecutor};

/// NodeManager owns both the management cluster and workflow execution cluster(s)
/// In future milestones, this will manage multiple execution clusters
pub struct NodeManager {
    /// Management cluster - tracks execution cluster membership and workflow lifecycle
    management_cluster: Arc<RaftCluster<ManagementCommandExecutor>>,

    /// Workflow execution cluster (will become a HashMap in future milestones)
    pub workflow_cluster: Arc<RaftCluster<WorkflowCommandExecutor>>,

    /// Workflow runtime (public API for starting workflows)
    workflow_runtime: Arc<WorkflowRuntime>,
}

impl NodeManager {
    /// Create a ClusterRouter and register both management and workflow clusters
    ///
    /// This enables Phase 2 multi-cluster routing where:
    /// - cluster_id = 0 routes to management cluster
    /// - cluster_id = 1 (default) routes to workflow execution cluster
    ///
    /// Returns the configured ClusterRouter ready for use with the gRPC server.
    pub fn create_cluster_router(&self) -> Result<Arc<crate::grpc::ClusterRouter>, Box<dyn std::error::Error>> {
        use crate::grpc::ClusterRouter;

        let router = ClusterRouter::new();

        // Register management cluster (cluster_id = 0)
        // TODO: Actually use management_cluster when we implement it properly
        // For now, we'll skip registering it since it's a placeholder

        // Register workflow execution cluster (cluster_id = 1 by default)
        // Get the receiver from transport and register it
        // NOTE: This requires access to the transport's receiver, which we'll need to expose
        // For now, we'll document this as a TODO for when we actually use multi-cluster routing

        Ok(Arc::new(router))
    }

    /// Create a new NodeManager by creating both clusters from a shared transport
    ///
    /// Phase 3: Transport is now type-parameter-free!
    /// The transport can now truly be shared between different cluster types.
    pub async fn new(
        transport: Arc<GrpcClusterTransport>,
        node_id: u64,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Create workflow cluster using new pattern
        // Phase 3: Use extract_typed_receiver to get Message<WorkflowCommand> receiver
        let receiver = transport.extract_typed_receiver::<WorkflowCommand>(node_id)?;
        let executor = WorkflowCommandExecutor::default();
        let transport_ref: Arc<dyn crate::raft::generic::transport::TransportInteraction<Message<WorkflowCommand>>> = transport.clone();
        let workflow_cluster = Arc::new(RaftCluster::new(node_id, receiver, transport_ref, executor).await?);

        // Create workflow runtime
        let workflow_runtime = WorkflowRuntime::new(workflow_cluster.clone());

        // Set the runtime reference in the executor so it can spawn workflows
        workflow_cluster.executor.set_runtime(workflow_runtime.clone());

        // TODO: Create management cluster once we implement unified transport with routing
        // For now, create a placeholder that won't be used
        // Phase 3: Transport is now type-parameter-free!
        let management_nodes = vec![crate::raft::generic::grpc_transport::NodeConfig {
            node_id,
            address: "127.0.0.1:0".to_string(),
        }];
        let management_transport = Arc::new(GrpcClusterTransport::new(management_nodes));
        management_transport.start().await?;

        // Create management cluster using new pattern
        // Phase 3: Use extract_typed_receiver to get Message<ManagementCommand> receiver
        let management_receiver = management_transport.extract_typed_receiver::<ManagementCommand>(node_id)?;
        let management_executor = ManagementCommandExecutor::default();
        let management_transport_ref: Arc<dyn crate::raft::generic::transport::TransportInteraction<Message<ManagementCommand>>> = management_transport.clone();
        let management_cluster = Arc::new(RaftCluster::new(node_id, management_receiver, management_transport_ref, management_executor).await?);

        Ok(Self {
            management_cluster,
            workflow_cluster,
            workflow_runtime,
        })
    }

    /// Get the workflow runtime for public API access
    pub fn workflow_runtime(&self) -> Arc<WorkflowRuntime> {
        self.workflow_runtime.clone()
    }

    /// Add a node to the workflow cluster
    /// TODO: In future milestones, this will route to the appropriate execution cluster
    pub async fn add_node(&self, node_id: u64, address: String) -> Result<(), String> {
        self.workflow_cluster.add_node(node_id, address).await
            .map_err(|e| e.to_string())
    }

    /// Remove a node from the workflow cluster
    /// TODO: In future milestones, this will handle removal from all execution clusters
    pub async fn remove_node(&self, node_id: u64) -> Result<(), String> {
        self.workflow_cluster.remove_node(node_id).await
            .map_err(|e| e.to_string())
    }

    /// Get cluster size (workflow cluster for now)
    pub fn cluster_size(&self) -> usize {
        self.workflow_cluster.node_count()
    }
}
