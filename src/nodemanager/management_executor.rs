///! Executor for management commands - applies commands to management cluster state

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::raft::generic::message::CommandExecutor;
use super::management_command::ManagementCommand;

/// State maintained by the management cluster
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ManagementState {
    /// All execution clusters
    pub execution_clusters: HashMap<Uuid, ExecutionClusterInfo>,

    /// Node membership: node_id → set of execution cluster IDs
    pub node_memberships: HashMap<u64, HashSet<Uuid>>,

    /// Workflow registry: workflow_id → cluster_id
    pub workflow_locations: HashMap<Uuid, Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionClusterInfo {
    pub cluster_id: Uuid,
    pub node_ids: Vec<u64>,
    pub active_workflows: HashSet<Uuid>,
    pub created_at: u64,
}

/// Executor for management cluster commands
pub struct ManagementCommandExecutor {
    state: Arc<Mutex<ManagementState>>,
}

impl ManagementCommandExecutor {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ManagementState::default())),
        }
    }

    pub fn state(&self) -> Arc<Mutex<ManagementState>> {
        self.state.clone()
    }
}

impl CommandExecutor for ManagementCommandExecutor {
    type Command = ManagementCommand;

    fn apply(&self, _command: &Self::Command, _logger: &slog::Logger) -> Result<(), Box<dyn std::error::Error>> {
        // TODO: Implement command application in future milestones
        // For now, just return empty success
        Ok(())
    }

    fn apply_with_index(&self, command: &Self::Command, logger: &slog::Logger, _log_index: u64) -> Result<(), Box<dyn std::error::Error>> {
        // For now, just delegate to apply (ignoring the index)
        self.apply(command, logger)
    }
}

impl Default for ManagementCommandExecutor {
    fn default() -> Self {
        Self::new()
    }
}
