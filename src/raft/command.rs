use serde::{Deserialize, Serialize};
use crate::raft::generic::RaftCommandType;

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

// Implement the trait for our specific command type
impl RaftCommandType for RaftCommand {
    fn apply(&self, logger: &slog::Logger) -> Result<(), Box<dyn std::error::Error>> {
        match self {
            RaftCommand::SetCheckpoint { key, value, workflow_id } => {
                slog::info!(logger, "Applied checkpoint";
                           "workflow_id" => workflow_id, "key" => key, "size" => value.len());
            },
            RaftCommand::PlaceholderCmd(cmd) => {
                slog::info!(logger, "Applied placeholder command";
                           "id" => cmd.id, "data" => &cmd.data);
            },
            RaftCommand::WorkflowStart { workflow_id, payload } => {
                slog::info!(logger, "Started workflow";
                           "workflow_id" => workflow_id, "payload_size" => payload.len());
            },
            RaftCommand::WorkflowEnd { workflow_id } => {
                slog::info!(logger, "Ended workflow"; "workflow_id" => workflow_id);
            },
        }
        Ok(())
    }
}