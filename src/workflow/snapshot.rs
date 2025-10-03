use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use crate::workflow::execution::WorkflowStatus;

/// Entry in checkpoint history with metadata
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckpointEntry {
    /// Serialized checkpoint value
    pub value: Vec<u8>,
    /// Raft log index where this checkpoint was committed
    pub log_index: u64,
    /// Timestamp for debugging/observability
    pub timestamp: u64,
}

/// Complete checkpoint history for snapshot creation
/// Never popped, only appended during workflow execution
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct CheckpointHistory {
    /// Map of (workflow_id, checkpoint_key) -> ordered list of values
    /// Stores ALL checkpoint updates for active workflows
    checkpoints: HashMap<(String, String), Vec<CheckpointEntry>>,
}

impl CheckpointHistory {
    /// Add a checkpoint to the history
    pub fn add_checkpoint(&mut self, workflow_id: String, key: String, entry: CheckpointEntry) {
        self.checkpoints
            .entry((workflow_id, key))
            .or_insert_with(Vec::new)
            .push(entry);
    }

    /// Get all checkpoint entries for a specific workflow and key
    pub fn get_checkpoints(&self, workflow_id: &str, key: &str) -> Option<&Vec<CheckpointEntry>> {
        self.checkpoints.get(&(workflow_id.to_string(), key.to_string()))
    }

    /// Remove all checkpoints for a completed workflow
    pub fn remove_workflow(&mut self, workflow_id: &str) {
        self.checkpoints.retain(|(wf_id, _), _| wf_id != workflow_id);
    }

    /// Get all checkpoints (for snapshot creation)
    pub fn all_checkpoints(&self) -> &HashMap<(String, String), Vec<CheckpointEntry>> {
        &self.checkpoints
    }
}

/// Complete cluster state at a point in time
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowSnapshot {
    /// Raft log index this snapshot represents
    pub snapshot_index: u64,
    /// Snapshot creation timestamp
    pub timestamp: u64,
    /// Active workflow states
    pub active_workflows: HashMap<String, WorkflowStatus>,
    /// Complete checkpoint history for active workflows only
    pub checkpoint_history: CheckpointHistory,
    /// Metadata
    pub metadata: SnapshotMetadata,
}

/// Metadata about the snapshot
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotMetadata {
    /// Node ID that created snapshot
    pub creator_node_id: u64,
    /// Cluster configuration at snapshot time (simplified serializable form)
    pub voters: Vec<u64>,
    pub learners: Vec<u64>,
}

/// State extracted from executor for snapshot creation
pub struct SnapshotState {
    pub active_workflows: HashMap<String, WorkflowStatus>,
    pub checkpoint_history: CheckpointHistory,
}

/// Get current timestamp in milliseconds since epoch
pub fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
