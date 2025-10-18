///! Executor for management commands - applies commands to management cluster state

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::raft::generic::message::CommandExecutor;
use crate::raft::RaftCluster;
use crate::workflow::WorkflowCommandExecutor;
use super::management_command::{ManagementCommand, AssociateNodeData, DisassociateNodeData};

// The default execution cluster ID (for the single execution cluster that mirrors management cluster)
const DEFAULT_EXECUTION_CLUSTER_ID: u128 = 1;

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
    workflow_cluster: Mutex<Option<Arc<RaftCluster<WorkflowCommandExecutor>>>>,
    management_cluster: Mutex<Option<Arc<RaftCluster<ManagementCommandExecutor>>>>,
}

impl ManagementCommandExecutor {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ManagementState::default())),
            workflow_cluster: Mutex::new(None),
            management_cluster: Mutex::new(None),
        }
    }

    /// Set the workflow cluster reference (called after construction)
    pub fn set_workflow_cluster(&self, cluster: Arc<RaftCluster<WorkflowCommandExecutor>>) {
        *self.workflow_cluster.lock().unwrap() = Some(cluster);
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

    /// Find which cluster a workflow is running on
    pub fn get_workflow_location(&self, workflow_id: &Uuid) -> Option<Uuid> {
        self.state.lock().unwrap().workflow_locations.get(workflow_id).copied()
    }

    /// Get all execution clusters that a node is a member of
    pub fn get_node_clusters(&self, node_id: u64) -> Vec<Uuid> {
        self.state.lock().unwrap()
            .node_memberships
            .get(&node_id)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Find the least loaded execution cluster (fewest active workflows)
    pub fn find_least_loaded_cluster(&self) -> Option<Uuid> {
        self.state.lock().unwrap()
            .execution_clusters
            .values()
            .min_by_key(|cluster| cluster.active_workflows.len())
            .map(|cluster| cluster.cluster_id)
    }

    /// Find an execution cluster that includes the given node
    pub fn find_cluster_with_node(&self, node_id: u64) -> Option<Uuid> {
        let state = self.state.lock().unwrap();
        state.node_memberships
            .get(&node_id)
            .and_then(|clusters| clusters.iter().next().copied())
    }

    /// Get count of active workflows across all clusters
    pub fn get_total_active_workflows(&self) -> usize {
        self.state.lock().unwrap()
            .execution_clusters
            .values()
            .map(|cluster| cluster.active_workflows.len())
            .sum()
    }
}

impl CommandExecutor for ManagementCommandExecutor {
    type Command = ManagementCommand;

    fn apply(&self, command: &Self::Command, logger: &slog::Logger) -> Result<(), Box<dyn std::error::Error>> {
        let mut state = self.state.lock().unwrap();

        match command {
            ManagementCommand::CreateExecutionCluster(data) => {
                slog::info!(logger, "Creating execution cluster";
                    "cluster_id" => %data.cluster_id,
                    "initial_nodes" => ?data.initial_node_ids
                );

                // Create new execution cluster
                let cluster_info = ExecutionClusterInfo {
                    cluster_id: data.cluster_id,
                    node_ids: data.initial_node_ids.clone(),
                    active_workflows: HashSet::new(),
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
                slog::info!(logger, "Destroying execution cluster"; "cluster_id" => %cluster_id);

                // Clone node_ids before borrowing state mutably
                let node_ids = if let Some(cluster_info) = state.execution_clusters.get(cluster_id) {
                    // Check that cluster has no active workflows
                    if !cluster_info.active_workflows.is_empty() {
                        return Err(format!(
                            "Cannot destroy cluster {} with {} active workflows",
                            cluster_id,
                            cluster_info.active_workflows.len()
                        ).into());
                    }
                    cluster_info.node_ids.clone()
                } else {
                    slog::warn!(logger, "Attempt to destroy non-existent cluster"; "cluster_id" => %cluster_id);
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
                slog::info!(logger, "Associating node with cluster";
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
                slog::info!(logger, "Disassociating node from cluster";
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

            ManagementCommand::ReportWorkflowStarted(data) => {
                slog::info!(logger, "Workflow started";
                    "workflow_id" => %data.workflow_id,
                    "cluster_id" => %data.cluster_id,
                    "workflow_type" => &data.workflow_type,
                    "version" => data.version
                );

                // Add workflow to cluster's active set
                if let Some(cluster_info) = state.execution_clusters.get_mut(&data.cluster_id) {
                    cluster_info.active_workflows.insert(data.workflow_id);
                } else {
                    slog::warn!(logger, "Workflow started on unknown cluster";
                        "cluster_id" => %data.cluster_id,
                        "workflow_id" => %data.workflow_id
                    );
                }

                // Track workflow location
                state.workflow_locations.insert(data.workflow_id, data.cluster_id);
            }

            ManagementCommand::ReportWorkflowEnded(data) => {
                slog::info!(logger, "Workflow ended";
                    "workflow_id" => %data.workflow_id,
                    "cluster_id" => %data.cluster_id
                );

                // Remove workflow from cluster's active set
                if let Some(cluster_info) = state.execution_clusters.get_mut(&data.cluster_id) {
                    cluster_info.active_workflows.remove(&data.workflow_id);
                }

                // Remove workflow location tracking
                state.workflow_locations.remove(&data.workflow_id);
            }

            ManagementCommand::ChangeNodeRole(data) => {
                slog::info!(logger, "Changing node role";
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

    fn apply_with_index(&self, command: &Self::Command, logger: &slog::Logger, _log_index: u64) -> Result<(), Box<dyn std::error::Error>> {
        // For now, just delegate to apply (ignoring the index)
        self.apply(command, logger)
    }

    fn on_node_added(&self, added_node_id: u64, address: &str, is_leader: bool, logger: &slog::Logger) {
        slog::info!(logger, "Management cluster detected node addition";
                   "node_id" => added_node_id, "address" => address, "is_leader" => is_leader);

        // Only the leader should modify the execution cluster and propose state changes
        if !is_leader {
            slog::debug!(logger, "Not leader, skipping execution cluster modification");
            return;
        }

        // 1. Propose AssociateNode command to management cluster (for state tracking)
        let management_cluster = self.management_cluster.lock().unwrap().clone();
        if let Some(mgmt_cluster) = management_cluster {
            let cluster_id = Uuid::from_u128(DEFAULT_EXECUTION_CLUSTER_ID);
            let command = ManagementCommand::AssociateNode(AssociateNodeData {
                cluster_id,
                node_id: added_node_id,
            });

            let logger_clone = logger.clone();
            let mgmt_cluster_clone = mgmt_cluster.clone();
            tokio::spawn(async move {
                match mgmt_cluster_clone.propose_and_sync(command).await {
                    Ok(_) => {
                        slog::info!(logger_clone, "Proposed AssociateNode to management cluster";
                                   "node_id" => added_node_id, "cluster_id" => %cluster_id);
                    }
                    Err(e) => {
                        slog::error!(logger_clone, "Failed to propose AssociateNode";
                                    "node_id" => added_node_id, "error" => %e);
                    }
                }
            });
        }

        // 2. Add node to workflow (execution) cluster
        let workflow_cluster = self.workflow_cluster.lock().unwrap().clone();
        if let Some(cluster) = workflow_cluster {
            let address_clone = address.to_string();
            let logger_clone = logger.clone();
            tokio::spawn(async move {
                match cluster.add_node(added_node_id, address_clone.clone()).await {
                    Ok(_) => {
                        slog::info!(logger_clone, "Successfully added node to workflow cluster";
                                   "node_id" => added_node_id, "address" => &address_clone);
                    }
                    Err(e) => {
                        slog::error!(logger_clone, "Failed to add node to workflow cluster";
                                    "node_id" => added_node_id, "error" => %e);
                    }
                }
            });
        } else {
            slog::warn!(logger, "Workflow cluster reference not set");
        }
    }

    fn on_node_removed(&self, removed_node_id: u64, is_leader: bool, logger: &slog::Logger) {
        slog::info!(logger, "Management cluster detected node removal";
                   "node_id" => removed_node_id, "is_leader" => is_leader);

        // Only the leader should modify the execution cluster and propose state changes
        if !is_leader {
            slog::debug!(logger, "Not leader, skipping execution cluster modification");
            return;
        }

        // 1. Propose DisassociateNode command to management cluster (for state tracking)
        let management_cluster = self.management_cluster.lock().unwrap().clone();
        if let Some(mgmt_cluster) = management_cluster {
            let cluster_id = Uuid::from_u128(DEFAULT_EXECUTION_CLUSTER_ID);
            let command = ManagementCommand::DisassociateNode(DisassociateNodeData {
                cluster_id,
                node_id: removed_node_id,
            });

            let logger_clone = logger.clone();
            let mgmt_cluster_clone = mgmt_cluster.clone();
            tokio::spawn(async move {
                match mgmt_cluster_clone.propose_and_sync(command).await {
                    Ok(_) => {
                        slog::info!(logger_clone, "Proposed DisassociateNode to management cluster";
                                   "node_id" => removed_node_id, "cluster_id" => %cluster_id);
                    }
                    Err(e) => {
                        slog::error!(logger_clone, "Failed to propose DisassociateNode";
                                    "node_id" => removed_node_id, "error" => %e);
                    }
                }
            });
        }

        // 2. Remove node from workflow (execution) cluster
        let workflow_cluster = self.workflow_cluster.lock().unwrap().clone();
        if let Some(cluster) = workflow_cluster {
            let logger_clone = logger.clone();
            tokio::spawn(async move {
                match cluster.remove_node(removed_node_id).await {
                    Ok(_) => {
                        slog::info!(logger_clone, "Successfully removed node from workflow cluster";
                                   "node_id" => removed_node_id);
                    }
                    Err(e) => {
                        slog::error!(logger_clone, "Failed to remove node from workflow cluster";
                                    "node_id" => removed_node_id, "error" => %e);
                    }
                }
            });
        } else {
            slog::warn!(logger, "Workflow cluster reference not set");
        }
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
        WorkflowLifecycleData,
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
        let logger = create_test_logger();

        let cluster_id = Uuid::new_v4();
        let command = ManagementCommand::CreateExecutionCluster(CreateExecutionClusterData {
            cluster_id,
            initial_node_ids: vec![1, 2, 3],
        });

        executor.apply(&command, &logger).unwrap();

        // Verify cluster was created
        let cluster_info = executor.get_cluster_info(&cluster_id).unwrap();
        assert_eq!(cluster_info.cluster_id, cluster_id);
        assert_eq!(cluster_info.node_ids, vec![1, 2, 3]);
        assert_eq!(cluster_info.active_workflows.len(), 0);

        // Verify node memberships
        assert_eq!(executor.get_node_clusters(1), vec![cluster_id]);
        assert_eq!(executor.get_node_clusters(2), vec![cluster_id]);
        assert_eq!(executor.get_node_clusters(3), vec![cluster_id]);
    }

    #[test]
    fn test_associate_and_disassociate_node() {
        let executor = ManagementCommandExecutor::new();
        let logger = create_test_logger();

        let cluster_id = Uuid::new_v4();

        // Create cluster
        executor.apply(&ManagementCommand::CreateExecutionCluster(
            CreateExecutionClusterData {
                cluster_id,
                initial_node_ids: vec![1, 2],
            }
        ), &logger).unwrap();

        // Associate node 3
        executor.apply(&ManagementCommand::AssociateNode(
            AssociateNodeData {
                cluster_id,
                node_id: 3,
            }
        ), &logger).unwrap();

        let cluster_info = executor.get_cluster_info(&cluster_id).unwrap();
        assert!(cluster_info.node_ids.contains(&3));
        assert_eq!(executor.get_node_clusters(3), vec![cluster_id]);

        // Disassociate node 3
        executor.apply(&ManagementCommand::DisassociateNode(
            DisassociateNodeData {
                cluster_id,
                node_id: 3,
            }
        ), &logger).unwrap();

        let cluster_info = executor.get_cluster_info(&cluster_id).unwrap();
        assert!(!cluster_info.node_ids.contains(&3));
        assert_eq!(executor.get_node_clusters(3).len(), 0);
    }

    #[test]
    fn test_workflow_lifecycle() {
        let executor = ManagementCommandExecutor::new();
        let logger = create_test_logger();

        let cluster_id = Uuid::new_v4();
        let workflow_id = Uuid::new_v4();

        // Create cluster
        executor.apply(&ManagementCommand::CreateExecutionCluster(
            CreateExecutionClusterData {
                cluster_id,
                initial_node_ids: vec![1],
            }
        ), &logger).unwrap();

        // Start workflow
        executor.apply(&ManagementCommand::ReportWorkflowStarted(
            WorkflowLifecycleData {
                workflow_id,
                cluster_id,
                workflow_type: "test_workflow".to_string(),
                version: 1,
                timestamp: 12345,
            }
        ), &logger).unwrap();

        // Verify workflow tracked
        assert_eq!(executor.get_workflow_location(&workflow_id), Some(cluster_id));
        let cluster_info = executor.get_cluster_info(&cluster_id).unwrap();
        assert!(cluster_info.active_workflows.contains(&workflow_id));
        assert_eq!(executor.get_total_active_workflows(), 1);

        // End workflow
        executor.apply(&ManagementCommand::ReportWorkflowEnded(
            WorkflowLifecycleData {
                workflow_id,
                cluster_id,
                workflow_type: "test_workflow".to_string(),
                version: 1,
                timestamp: 12346,
            }
        ), &logger).unwrap();

        // Verify workflow removed
        assert_eq!(executor.get_workflow_location(&workflow_id), None);
        let cluster_info = executor.get_cluster_info(&cluster_id).unwrap();
        assert!(!cluster_info.active_workflows.contains(&workflow_id));
        assert_eq!(executor.get_total_active_workflows(), 0);
    }

    #[test]
    fn test_destroy_cluster_with_workflows_fails() {
        let executor = ManagementCommandExecutor::new();
        let logger = create_test_logger();

        let cluster_id = Uuid::new_v4();
        let workflow_id = Uuid::new_v4();

        // Create cluster and start workflow
        executor.apply(&ManagementCommand::CreateExecutionCluster(
            CreateExecutionClusterData {
                cluster_id,
                initial_node_ids: vec![1],
            }
        ), &logger).unwrap();

        executor.apply(&ManagementCommand::ReportWorkflowStarted(
            WorkflowLifecycleData {
                workflow_id,
                cluster_id,
                workflow_type: "test".to_string(),
                version: 1,
                timestamp: 0,
            }
        ), &logger).unwrap();

        // Try to destroy cluster - should fail
        let result = executor.apply(&ManagementCommand::DestroyExecutionCluster(cluster_id), &logger);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("active workflows"));

        // End workflow
        executor.apply(&ManagementCommand::ReportWorkflowEnded(
            WorkflowLifecycleData {
                workflow_id,
                cluster_id,
                workflow_type: "test".to_string(),
                version: 1,
                timestamp: 1,
            }
        ), &logger).unwrap();

        // Now destroy should succeed
        executor.apply(&ManagementCommand::DestroyExecutionCluster(cluster_id), &logger).unwrap();
        assert!(executor.get_cluster_info(&cluster_id).is_none());
    }

    #[test]
    fn test_find_least_loaded_cluster() {
        let executor = ManagementCommandExecutor::new();
        let logger = create_test_logger();

        let cluster1 = Uuid::new_v4();
        let cluster2 = Uuid::new_v4();

        // Create two clusters
        executor.apply(&ManagementCommand::CreateExecutionCluster(
            CreateExecutionClusterData {
                cluster_id: cluster1,
                initial_node_ids: vec![1],
            }
        ), &logger).unwrap();

        executor.apply(&ManagementCommand::CreateExecutionCluster(
            CreateExecutionClusterData {
                cluster_id: cluster2,
                initial_node_ids: vec![2],
            }
        ), &logger).unwrap();

        // Both empty - either is fine
        let least_loaded = executor.find_least_loaded_cluster();
        assert!(least_loaded.is_some());

        // Add workflow to cluster1
        executor.apply(&ManagementCommand::ReportWorkflowStarted(
            WorkflowLifecycleData {
                workflow_id: Uuid::new_v4(),
                cluster_id: cluster1,
                workflow_type: "test".to_string(),
                version: 1,
                timestamp: 0,
            }
        ), &logger).unwrap();

        // cluster2 should be least loaded
        assert_eq!(executor.find_least_loaded_cluster(), Some(cluster2));
    }
}
