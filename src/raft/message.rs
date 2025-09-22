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

pub enum Message {
    Propose {
        id: u8,
        callback: Option<tokio::sync::oneshot::Sender<bool>>,
        command: RaftCommand,
    },
    Raft(raft::prelude::Message),
    ConfChangeV2 {
        id: u8,
        callback: Option<tokio::sync::oneshot::Sender<bool>>,
        change: raft::prelude::ConfChangeV2,
    },
}