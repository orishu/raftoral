use serde::{Serialize, Deserialize};
use serde::de::DeserializeOwned;
use std::fmt::Debug;

/// Trait for commands that can be proposed through Raft
pub trait RaftCommandType: Clone + Debug + Serialize + DeserializeOwned + Send + Sync + 'static {
    /// Apply this command to the state machine
    fn apply(&self, logger: &slog::Logger) -> Result<(), Box<dyn std::error::Error>>;
}

/// Wrapper for commands with optional tracking ID
#[derive(Clone, Debug, Serialize)]
pub struct CommandWrapper<C: RaftCommandType> {
    pub id: Option<u64>,
    pub command: C,
}

// Manual Deserialize implementation to work with RaftCommandType constraints
impl<'de, C: RaftCommandType> Deserialize<'de> for CommandWrapper<C> {
    fn deserialize<D>(deserializer: D) -> Result<CommandWrapper<C>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct CommandWrapperHelper<T> {
            id: Option<u64>,
            command: T,
        }

        let helper = CommandWrapperHelper::<C>::deserialize(deserializer)?;
        Ok(CommandWrapper {
            id: helper.id,
            command: helper.command,
        })
    }
}

pub enum Message<C: RaftCommandType> {
    Propose {
        id: u8,
        callback: Option<tokio::sync::oneshot::Sender<bool>>,
        sync_callback: Option<tokio::sync::oneshot::Sender<Result<(), Box<dyn std::error::Error + Send + Sync>>>>,
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