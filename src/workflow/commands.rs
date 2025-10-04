use serde::{Deserialize, Serialize};

/// Data for starting a workflow execution
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowStartData {
    pub workflow_id: String,
    pub workflow_type: String,
    pub version: u32,
    pub input: Vec<u8>,
    pub owner_node_id: u64,  // Node responsible for executing this workflow
}

/// Data for ending a workflow execution
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowEndData {
    pub workflow_id: String,
    pub result: Vec<u8>,
}

/// Data for setting a checkpoint value
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckpointData {
    pub workflow_id: String,
    pub key: String,
    pub value: Vec<u8>,
}

/// Reason for ownership change
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum OwnerChangeReason {
    /// Node was removed from cluster configuration
    NodeFailure,
    /// Future: leader-initiated load balancing
    #[allow(dead_code)]
    LoadBalancing,
}

/// Data for changing workflow ownership
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OwnerChangeData {
    pub workflow_id: String,
    pub old_owner_node_id: u64,
    pub new_owner_node_id: u64,
    pub reason: OwnerChangeReason,
}

/// Commands that can be proposed through the Raft cluster for workflow operations
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WorkflowCommand {
    /// Start a new workflow execution
    WorkflowStart(WorkflowStartData),
    /// End a workflow execution with a result
    WorkflowEnd(WorkflowEndData),
    /// Set a checkpoint value for a workflow
    SetCheckpoint(CheckpointData),
    /// Change ownership of a workflow to a different node
    OwnerChange(OwnerChangeData),
}
