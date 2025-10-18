///! Management commands for controlling execution cluster membership
///! and tracking workflow lifecycle across the deployment.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Commands for the management cluster
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ManagementCommand {
    /// Create a new virtual execution cluster
    CreateExecutionCluster(CreateExecutionClusterData),

    /// Destroy an execution cluster (must be empty of workflows)
    DestroyExecutionCluster(ExecutionClusterId),

    /// Associate a node with an execution cluster
    AssociateNode(AssociateNodeData),

    /// Disassociate a node from an execution cluster
    DisassociateNode(DisassociateNodeData),

    /// Schedule a workflow to start on an execution cluster
    /// Management nodes that are part of the execution cluster will propose WorkflowStart
    ScheduleWorkflowStart(ScheduleWorkflowData),

    /// Report workflow started on an execution cluster (legacy, for backward compatibility)
    ReportWorkflowStarted(WorkflowLifecycleData),

    /// Report workflow ended on an execution cluster
    ReportWorkflowEnded(WorkflowLifecycleData),

    /// Change a node's role (voter/learner) in the management cluster
    ChangeNodeRole(ChangeNodeRoleData),
}

pub type ExecutionClusterId = Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateExecutionClusterData {
    pub cluster_id: Uuid,
    pub initial_node_ids: Vec<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssociateNodeData {
    pub cluster_id: Uuid,
    pub node_id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DisassociateNodeData {
    pub cluster_id: Uuid,
    pub node_id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScheduleWorkflowData {
    pub workflow_id: Uuid,
    pub cluster_id: Uuid,
    pub workflow_type: String,
    pub version: u32,
    pub input_json: String,  // JSON-serialized input for the workflow
    pub timestamp: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowLifecycleData {
    pub workflow_id: Uuid,
    pub cluster_id: Uuid,
    pub workflow_type: String,
    pub version: u32,
    pub timestamp: u64,
    /// JSON-serialized result (for ReportWorkflowEnded)
    /// This is a Result<T, E> serialized as JSON
    pub result_json: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangeNodeRoleData {
    pub node_id: u64,
    pub is_voter: bool,
}
