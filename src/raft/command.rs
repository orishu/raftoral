use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlaceholderCommand {
    pub id: u64,
    pub data: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RaftCommand {
    WorkflowStart { workflow_id: u64, payload: Vec<u8> },
    WorkflowEnd { workflow_id: u64 },
    SetCheckpoint { workflow_id: u64, key: String, value: Vec<u8> },
    PlaceholderCmd(PlaceholderCommand),
}

// Note: RaftCommand is legacy - use WorkflowCommand and WorkflowCommandExecutor instead