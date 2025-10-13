///! NodeManager - owns both management and workflow execution clusters

use std::sync::Arc;
use crate::raft::RaftCluster;
use crate::raft::generic::grpc_transport::GrpcClusterTransport;
use crate::raft::generic::transport::ClusterTransport;
use crate::raft::generic::message::Message;
use crate::workflow::{WorkflowCommand, WorkflowCommandExecutor, WorkflowRuntime};
use super::management_executor::ManagementCommandExecutor;
use super::management_command::ManagementCommand;

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
    /// Create a new NodeManager by creating both clusters from a shared transport
    ///
    /// The transport is shared between management and workflow clusters since
    /// execution clusters are "virtual" - they use the same underlying transport.
    ///
    /// TODO: In future milestones, implement proper message routing to allow
    /// multiple clusters to share the same transport without conflicts.
    pub async fn new(
        transport: Arc<GrpcClusterTransport<Message<WorkflowCommand>>>,
        node_id: u64,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Create workflow executor
        let workflow_executor = WorkflowCommandExecutor::default();

        // Create workflow cluster
        let workflow_cluster = transport.create_cluster(node_id, workflow_executor).await?;

        // Create workflow runtime
        let workflow_runtime = WorkflowRuntime::new(workflow_cluster.clone());

        // TODO: Create management cluster once we implement unified transport with routing
        // For now, create a placeholder that won't be used
        let management_nodes = vec![crate::raft::generic::grpc_transport::NodeConfig {
            node_id,
            address: "127.0.0.1:0".to_string(),
        }];
        let management_transport = Arc::new(GrpcClusterTransport::<Message<ManagementCommand>>::new(management_nodes));
        management_transport.start().await?;

        let management_executor = ManagementCommandExecutor::default();
        let management_cluster = management_transport.create_cluster(node_id, management_executor).await?;

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
