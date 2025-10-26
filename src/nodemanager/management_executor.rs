///! Executor for management commands - applies commands to management cluster state

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::raft::generic::message::CommandExecutor;
use crate::raft::RaftCluster;
use super::management_command::ManagementCommand;

/// Serializable snapshot of management state
/// Used for Raft snapshots
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ManagementStateSnapshot {
    /// All execution clusters
    pub execution_clusters: HashMap<Uuid, ExecutionClusterInfo>,

    /// Node membership: node_id → set of execution cluster IDs
    pub node_memberships: HashMap<u64, HashSet<Uuid>>,
}

/// State maintained by the management cluster
#[derive(Clone, Debug)]
pub struct ManagementState {
    /// All execution clusters
    pub execution_clusters: HashMap<Uuid, ExecutionClusterInfo>,

    /// Node membership: node_id → set of execution cluster IDs
    pub node_memberships: HashMap<u64, HashSet<Uuid>>,
}

impl ManagementState {
    /// Create a snapshot of the state
    pub fn create_snapshot(&self) -> ManagementStateSnapshot {
        ManagementStateSnapshot {
            execution_clusters: self.execution_clusters.clone(),
            node_memberships: self.node_memberships.clone(),
        }
    }

    /// Restore state from a snapshot
    pub fn restore_from_snapshot(&mut self, snapshot: ManagementStateSnapshot) {
        self.execution_clusters = snapshot.execution_clusters;
        self.node_memberships = snapshot.node_memberships;
    }
}

impl Default for ManagementState {
    fn default() -> Self {
        Self {
            execution_clusters: HashMap::new(),
            node_memberships: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionClusterInfo {
    pub cluster_id: Uuid,
    pub node_ids: Vec<u64>,
    pub created_at: u64,
}

/// Executor for management cluster commands
pub struct ManagementCommandExecutor {
    state: Arc<Mutex<ManagementState>>,
    management_cluster: Mutex<Option<Arc<RaftCluster<ManagementCommandExecutor>>>>,
    logger: slog::Logger,
}

impl ManagementCommandExecutor {
    pub fn new() -> Self {
        // Create a default logger with discard drain
        let logger = slog::Logger::root(slog::Discard, slog::o!());
        Self {
            state: Arc::new(Mutex::new(ManagementState::default())),
            management_cluster: Mutex::new(None),
            logger,
        }
    }

    /// Create a new executor with a specific logger
    pub fn with_logger(logger: slog::Logger) -> Self {
        Self {
            state: Arc::new(Mutex::new(ManagementState::default())),
            management_cluster: Mutex::new(None),
            logger,
        }
    }

    /// Set the management cluster reference (called after construction)
    pub fn set_management_cluster(&self, cluster: Arc<RaftCluster<ManagementCommandExecutor>>) {
        *self.management_cluster.lock().unwrap() = Some(cluster);
    }

    pub fn state(&self) -> Arc<Mutex<ManagementState>> {
        self.state.clone()
    }

    /// Get information about a specific execution cluster
    pub fn get_cluster_info(&self, cluster_id: &Uuid) -> Option<ExecutionClusterInfo> {
        self.state.lock().unwrap().execution_clusters.get(cluster_id).cloned()
    }

    /// Get all execution clusters
    pub fn get_all_clusters(&self) -> Vec<ExecutionClusterInfo> {
        self.state.lock().unwrap()
            .execution_clusters
            .values()
            .cloned()
            .collect()
    }

    /// Get all execution clusters that a node is a member of
    pub fn get_node_clusters(&self, node_id: u64) -> Vec<Uuid> {
        self.state.lock().unwrap()
            .node_memberships
            .get(&node_id)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Find an execution cluster that includes the given node
    pub fn find_cluster_with_node(&self, node_id: u64) -> Option<Uuid> {
        let state = self.state.lock().unwrap();
        state.node_memberships
            .get(&node_id)
            .and_then(|clusters| clusters.iter().next().copied())
    }
}

impl CommandExecutor for ManagementCommandExecutor {
    type Command = ManagementCommand;

    fn apply(&self, command: &Self::Command) -> Result<(), Box<dyn std::error::Error>> {
        let mut state = self.state.lock().unwrap();

        match command {
            ManagementCommand::CreateExecutionCluster(data) => {
                slog::info!(self.logger, "Creating execution cluster";
                    "cluster_id" => %data.cluster_id,
                    "initial_nodes" => ?data.initial_node_ids
                );

                // Create new execution cluster
                let cluster_info = ExecutionClusterInfo {
                    cluster_id: data.cluster_id,
                    node_ids: data.initial_node_ids.clone(),
                    created_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                };

                state.execution_clusters.insert(data.cluster_id, cluster_info);

                // Update node memberships
                for node_id in &data.initial_node_ids {
                    state.node_memberships
                        .entry(*node_id)
                        .or_insert_with(HashSet::new)
                        .insert(data.cluster_id);
                }
            }

            ManagementCommand::DestroyExecutionCluster(cluster_id) => {
                slog::info!(self.logger, "Destroying execution cluster"; "cluster_id" => %cluster_id);

                // Clone node_ids before borrowing state mutably
                let node_ids = if let Some(cluster_info) = state.execution_clusters.get(cluster_id) {
                    cluster_info.node_ids.clone()
                } else {
                    slog::warn!(self.logger, "Attempt to destroy non-existent cluster"; "cluster_id" => %cluster_id);
                    return Ok(());
                };

                // Remove from node memberships
                for node_id in &node_ids {
                    let should_remove_node = if let Some(memberships) = state.node_memberships.get_mut(node_id) {
                        memberships.remove(cluster_id);
                        memberships.is_empty()
                    } else {
                        false
                    };

                    if should_remove_node {
                        state.node_memberships.remove(node_id);
                    }
                }

                // Remove cluster
                state.execution_clusters.remove(cluster_id);
            }

            ManagementCommand::AssociateNode(data) => {
                slog::info!(self.logger, "Associating node with cluster";
                    "node_id" => data.node_id,
                    "cluster_id" => %data.cluster_id
                );

                // Update cluster's node list
                if let Some(cluster_info) = state.execution_clusters.get_mut(&data.cluster_id) {
                    if !cluster_info.node_ids.contains(&data.node_id) {
                        cluster_info.node_ids.push(data.node_id);
                    }
                } else {
                    return Err(format!("Cluster {} not found", data.cluster_id).into());
                }

                // Update node's membership set
                state.node_memberships
                    .entry(data.node_id)
                    .or_insert_with(HashSet::new)
                    .insert(data.cluster_id);
            }

            ManagementCommand::DisassociateNode(data) => {
                slog::info!(self.logger, "Disassociating node from cluster";
                    "node_id" => data.node_id,
                    "cluster_id" => %data.cluster_id
                );

                // Remove from cluster's node list
                if let Some(cluster_info) = state.execution_clusters.get_mut(&data.cluster_id) {
                    cluster_info.node_ids.retain(|id| *id != data.node_id);
                }

                // Remove from node's membership set
                let should_remove_node = if let Some(memberships) = state.node_memberships.get_mut(&data.node_id) {
                    memberships.remove(&data.cluster_id);
                    memberships.is_empty()
                } else {
                    false
                };

                if should_remove_node {
                    state.node_memberships.remove(&data.node_id);
                }
            }

            ManagementCommand::ChangeNodeRole(data) => {
                slog::info!(self.logger, "Changing node role";
                    "node_id" => data.node_id,
                    "is_voter" => data.is_voter
                );
                // Note: Role changes are handled by Raft ConfChange mechanism
                // This command is primarily for logging/auditing
                // The actual Raft configuration is managed by RaftCluster
            }
        }

        Ok(())
    }

    fn apply_with_index(&self, command: &Self::Command, _log_index: u64) -> Result<(), Box<dyn std::error::Error>> {
        // For now, just delegate to apply (ignoring the index)
        self.apply(command)
    }

    fn on_node_added(&self, added_node_id: u64, address: &str, is_leader: bool) {
        slog::info!(self.logger, "Management cluster detected node addition";
                   "node_id" => added_node_id, "address" => address, "is_leader" => is_leader);

        // Note: With multiple execution clusters, we no longer automatically sync
        // execution cluster membership with management cluster membership.
        // Execution clusters are managed explicitly through CreateExecutionCluster
        // and AssociateNode/DisassociateNode commands.
    }

    fn on_node_removed(&self, removed_node_id: u64, is_leader: bool) {
        slog::info!(self.logger, "Management cluster detected node removal";
                   "node_id" => removed_node_id, "is_leader" => is_leader);

        // Note: With multiple execution clusters, we no longer automatically sync
        // execution cluster membership with management cluster membership.
        // Execution clusters are managed explicitly through CreateExecutionCluster
        // and AssociateNode/DisassociateNode commands.
    }

    fn create_snapshot(&self, snapshot_index: u64) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        slog::info!(self.logger, "Creating management state snapshot"; "index" => snapshot_index);

        let state = self.state.lock().unwrap();
        let snapshot = state.create_snapshot();

        // Serialize the snapshot
        let snapshot_bytes = serde_json::to_vec(&snapshot)?;

        slog::info!(self.logger, "Created management state snapshot";
                   "index" => snapshot_index,
                   "size_bytes" => snapshot_bytes.len(),
                   "execution_clusters" => snapshot.execution_clusters.len(),
                   "node_memberships" => snapshot.node_memberships.len()
        );

        Ok(snapshot_bytes)
    }

    fn restore_from_snapshot(&self, snapshot_data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        slog::info!(self.logger, "Restoring management state from snapshot"; "size_bytes" => snapshot_data.len());

        // Deserialize the snapshot
        let snapshot: ManagementStateSnapshot = serde_json::from_slice(snapshot_data)?;

        // Restore state
        let mut state = self.state.lock().unwrap();
        state.restore_from_snapshot(snapshot);

        slog::info!(self.logger, "Restored management state from snapshot";
                   "execution_clusters" => state.execution_clusters.len(),
                   "node_memberships" => state.node_memberships.len()
        );

        Ok(())
    }

    fn should_create_snapshot(&self, log_size: u64, snapshot_interval: u64) -> bool {
        // Create snapshot every snapshot_interval entries (default 1000)
        log_size > 0 && log_size % snapshot_interval == 0
    }
}

impl Default for ManagementCommandExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::management_command::{
        CreateExecutionClusterData, AssociateNodeData, DisassociateNodeData,
    };
    use slog::Drain;

    fn create_test_logger() -> slog::Logger {
        let decorator = slog_term::PlainDecorator::new(slog_term::TestStdoutWriter);
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        slog::Logger::root(drain, slog::o!())
    }

    #[test]
    fn test_create_execution_cluster() {
        let executor = ManagementCommandExecutor::new();
        let _logger = create_test_logger();

        let cluster_id = Uuid::new_v4();
        let command = ManagementCommand::CreateExecutionCluster(CreateExecutionClusterData {
            cluster_id,
            initial_node_ids: vec![1, 2, 3],
        });

        executor.apply(&command).unwrap();

        // Verify cluster was created
        let cluster_info = executor.get_cluster_info(&cluster_id).unwrap();
        assert_eq!(cluster_info.cluster_id, cluster_id);
        assert_eq!(cluster_info.node_ids, vec![1, 2, 3]);

        // Verify node memberships
        assert_eq!(executor.get_node_clusters(1), vec![cluster_id]);
        assert_eq!(executor.get_node_clusters(2), vec![cluster_id]);
        assert_eq!(executor.get_node_clusters(3), vec![cluster_id]);
    }

    #[test]
    fn test_associate_and_disassociate_node() {
        let executor = ManagementCommandExecutor::new();
        let _logger = create_test_logger();

        let cluster_id = Uuid::new_v4();

        // Create cluster
        executor.apply(&ManagementCommand::CreateExecutionCluster(
            CreateExecutionClusterData {
                cluster_id,
                initial_node_ids: vec![1, 2],
            }
        )).unwrap();

        // Associate node 3
        executor.apply(&ManagementCommand::AssociateNode(
            AssociateNodeData {
                cluster_id,
                node_id: 3,
            }
        )).unwrap();

        let cluster_info = executor.get_cluster_info(&cluster_id).unwrap();
        assert!(cluster_info.node_ids.contains(&3));
        assert_eq!(executor.get_node_clusters(3), vec![cluster_id]);

        // Disassociate node 3
        executor.apply(&ManagementCommand::DisassociateNode(
            DisassociateNodeData {
                cluster_id,
                node_id: 3,
            }
        )).unwrap();

        let cluster_info = executor.get_cluster_info(&cluster_id).unwrap();
        assert!(!cluster_info.node_ids.contains(&3));
        assert_eq!(executor.get_node_clusters(3).len(), 0);
    }





    #[test]
    fn test_snapshot_interval() {
        let executor = ManagementCommandExecutor::new();

        // Should create snapshot every 1000 entries
        assert!(!executor.should_create_snapshot(0, 1000));
        assert!(!executor.should_create_snapshot(999, 1000));
        assert!(executor.should_create_snapshot(1000, 1000));
        assert!(!executor.should_create_snapshot(1001, 1000));
        assert!(executor.should_create_snapshot(2000, 1000));
        assert!(executor.should_create_snapshot(3000, 1000));

        // With different interval
        assert!(executor.should_create_snapshot(500, 500));
        assert!(executor.should_create_snapshot(1500, 500));
        assert!(!executor.should_create_snapshot(501, 500));
    }

}
