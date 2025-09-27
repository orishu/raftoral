use serde::{Serialize};
use serde::de::DeserializeOwned;
use std::fmt::Debug;

/// Trait for commands that can be proposed through Raft
pub trait RaftCommandType: Clone + Debug + Serialize + DeserializeOwned + Send + Sync + 'static {
    /// Apply this command to the state machine
    fn apply(&self, logger: &slog::Logger) -> Result<(), Box<dyn std::error::Error>>;
}

pub enum Message<C: RaftCommandType> {
    Propose {
        id: u8,
        callback: Option<tokio::sync::oneshot::Sender<bool>>,
        command: C,
    },
    Raft(raft::prelude::Message),
    ConfChangeV2 {
        id: u8,
        callback: Option<tokio::sync::oneshot::Sender<bool>>,
        change: raft::prelude::ConfChangeV2,
    },
    Campaign {
        callback: Option<tokio::sync::oneshot::Sender<bool>>,
    },
}