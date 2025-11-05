//! Workflow events emitted by the workflow state machine

use serde::{Deserialize, Serialize};

/// Events emitted by the WorkflowStateMachine
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum WorkflowEvent {
    /// A workflow has started execution
    WorkflowStarted {
        workflow_id: String,
        workflow_type: String,
        version: u32,
        owner_node_id: u64,
    },

    /// A workflow has completed successfully
    WorkflowCompleted {
        workflow_id: String,
        result: Vec<u8>,
    },

    /// A workflow has failed with an error
    WorkflowFailed {
        workflow_id: String,
        error: Vec<u8>,
    },

    /// A checkpoint was set for a workflow
    CheckpointSet {
        workflow_id: String,
        key: String,
        value: Vec<u8>,
    },

    /// Workflow ownership changed (e.g., due to node failure)
    OwnershipChanged {
        workflow_id: String,
        old_owner_node_id: u64,
        new_owner_node_id: u64,
    },
}
