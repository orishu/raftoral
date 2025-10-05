use serde::{Serialize, Deserialize};
use serde::de::DeserializeOwned;
use std::fmt::Debug;

/// Trait for executing commands that have been committed through Raft
pub trait CommandExecutor: Send + Sync + 'static {
    /// The command type that this executor handles
    type Command: Clone + Debug + Serialize + DeserializeOwned + Send + Sync + 'static;

    /// Apply a command to the state machine with log index tracking
    fn apply_with_index(&self, command: &Self::Command, logger: &slog::Logger, log_index: u64) -> Result<(), Box<dyn std::error::Error>>;

    /// Apply a command to the state machine (backward compatibility)
    fn apply(&self, command: &Self::Command, logger: &slog::Logger) -> Result<(), Box<dyn std::error::Error>> {
        self.apply_with_index(command, logger, 0)
    }

    /// Set the node ID for ownership checks (default: no-op)
    fn set_node_id(&self, _node_id: u64) {
        // Default implementation does nothing
        // WorkflowCommandExecutor overrides this
    }

    /// Notify when a node is removed from the cluster
    /// This allows the executor to react to node failures (e.g., reassign workflows)
    fn on_node_removed(&self, _removed_node_id: u64, _logger: &slog::Logger) {
        // Default implementation does nothing
        // WorkflowCommandExecutor overrides this for workflow reassignment
    }

    /// Create a snapshot of the current state
    /// Returns serialized snapshot data
    fn create_snapshot(&self, _snapshot_index: u64) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        // Default implementation: no snapshot support
        Ok(Vec::new())
    }

    /// Restore state from a snapshot
    /// Called when receiving a snapshot from leader or during recovery
    fn restore_from_snapshot(&self, _snapshot_data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        // Default implementation: no snapshot support
        Ok(())
    }

    /// Check if snapshot should be created based on log size
    /// Executors can override to implement custom logic
    fn should_create_snapshot(&self, _log_size: u64, _snapshot_interval: u64) -> bool {
        // Default implementation: use simple threshold check
        false
    }
}

/// Wrapper for commands with optional tracking ID
#[derive(Clone, Debug, Serialize)]
pub struct CommandWrapper<C>
where
    C: Clone + Debug + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    pub id: Option<u64>,
    pub command: C,
}

// Manual Deserialize implementation to work with command constraints
impl<'de, C> Deserialize<'de> for CommandWrapper<C>
where
    C: Clone + Debug + Serialize + DeserializeOwned + Send + Sync + 'static,
{
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

pub enum Message<C>
where
    C: Clone + Debug + Serialize + DeserializeOwned + Send + Sync + 'static,
{
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
    AddNode {
        node_id: u64,
        callback: Option<tokio::sync::oneshot::Sender<Result<(), Box<dyn std::error::Error + Send + Sync>>>>,
    },
    RemoveNode {
        node_id: u64,
        callback: Option<tokio::sync::oneshot::Sender<Result<(), Box<dyn std::error::Error + Send + Sync>>>>,
    },
}