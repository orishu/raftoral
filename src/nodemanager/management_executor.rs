///! Executor for management commands - applies commands to management cluster state

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use ttl_cache::TtlCache;

use crate::raft::generic::message::CommandExecutor;
use crate::raft::RaftCluster;
use crate::workflow::WorkflowCommandExecutor;
use super::management_command::{ManagementCommand, AssociateNodeData, DisassociateNodeData};

// The default execution cluster ID (for the single execution cluster that mirrors management cluster)
const DEFAULT_EXECUTION_CLUSTER_ID: u128 = 1;

/// State maintained by the management cluster
/// Note: completed_workflows is not serialized (it's a TTL cache for runtime use only)
#[derive(Clone)]
pub struct ManagementState {
    /// All execution clusters
    pub execution_clusters: HashMap<Uuid, ExecutionClusterInfo>,

    /// Node membership: node_id → set of execution cluster IDs
    pub node_memberships: HashMap<u64, HashSet<Uuid>>,

    /// Workflow registry: workflow_id → cluster_id
    pub workflow_locations: HashMap<Uuid, Uuid>,

    /// Completed workflow results (TTL cache, 10 minute default)
    /// Maps workflow_id → JSON-serialized Result<T, E>
    /// Not serialized for snapshots (runtime cache only)
    pub completed_workflows: Arc<Mutex<TtlCache<Uuid, String>>>,
}

impl std::fmt::Debug for ManagementState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagementState")
            .field("execution_clusters", &self.execution_clusters)
            .field("node_memberships", &self.node_memberships)
            .field("workflow_locations", &self.workflow_locations)
            .field("completed_workflows", &"<TTL Cache>")
            .finish()
    }
}

impl Default for ManagementState {
    fn default() -> Self {
        // Create TTL cache with 10 minute TTL and capacity of 10000 entries
        let ttl_cache = TtlCache::new(10000);
        Self {
            execution_clusters: HashMap::new(),
            node_memberships: HashMap::new(),
            workflow_locations: HashMap::new(),
            completed_workflows: Arc::new(Mutex::new(ttl_cache)),
        }
    }
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
    logger: slog::Logger,
}

impl ManagementCommandExecutor {
    pub fn new() -> Self {
        // Create a default logger with discard drain
        let logger = slog::Logger::root(slog::Discard, slog::o!());
        Self {
            state: Arc::new(Mutex::new(ManagementState::default())),
            workflow_cluster: Mutex::new(None),
            management_cluster: Mutex::new(None),
            logger,
        }
    }

    /// Create a new executor with a specific logger
    pub fn with_logger(logger: slog::Logger) -> Self {
        Self {
            state: Arc::new(Mutex::new(ManagementState::default())),
            workflow_cluster: Mutex::new(None),
            management_cluster: Mutex::new(None),
            logger,
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

    /// Get completed workflow result from TTL cache
    /// Returns None if workflow not found or still running
    pub fn get_completed_workflow_result(&self, workflow_id: &Uuid) -> Option<String> {
        self.state.lock().unwrap()
            .completed_workflows
            .lock()
            .unwrap()
            .get(workflow_id)
            .cloned()
    }

    /// Check if workflow is still active (not yet completed)
    pub fn is_workflow_active(&self, workflow_id: &Uuid) -> bool {
        self.state.lock().unwrap()
            .workflow_locations
            .contains_key(workflow_id)
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
                slog::info!(self.logger, "Destroying execution cluster"; "cluster_id" => %cluster_id);

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

            ManagementCommand::ScheduleWorkflowStart(data) => {
                slog::info!(self.logger, "Scheduling workflow start";
                    "workflow_id" => %data.workflow_id,
                    "cluster_id" => %data.cluster_id,
                    "workflow_type" => &data.workflow_type,
                    "version" => data.version
                );

                // Add workflow to cluster's active set
                if let Some(cluster_info) = state.execution_clusters.get_mut(&data.cluster_id) {
                    cluster_info.active_workflows.insert(data.workflow_id);
                } else {
                    slog::warn!(self.logger, "Workflow scheduled on unknown cluster";
                        "cluster_id" => %data.cluster_id,
                        "workflow_id" => %data.workflow_id
                    );
                }

                // Track workflow location
                state.workflow_locations.insert(data.workflow_id, data.cluster_id);

                // Drop the state lock before async operations
                drop(state);

                // Option 1: Only nodes that are leaders of the execution cluster propose WorkflowStart
                // LIMITATION: If the leader just went offline, no node will start the workflow
                // TODO: Consider implementing Option 2 (all execution cluster members propose)
                //       with duplicate detection in WorkflowCommandExecutor

                // Check if this node is part of the target execution cluster
                let workflow_cluster = self.workflow_cluster.lock().unwrap().clone();
                if let Some(cluster) = workflow_cluster {
                    // Only proceed if we're the leader of the execution cluster
                    // This uses cluster_id matching - we assume workflow_cluster is cluster_id=1
                    // In the future with multiple execution clusters, we'll need to look up the right cluster
                    if data.cluster_id == Uuid::from_u128(DEFAULT_EXECUTION_CLUSTER_ID) {
                        let logger_clone = self.logger.clone();
                        let workflow_id_clone = data.workflow_id;
                        let workflow_type_clone = data.workflow_type.clone();
                        let version = data.version;
                        let input_json = data.input_json.clone();

                        tokio::spawn(async move {
                            // Check if we're the leader before proposing
                            if !cluster.is_leader().await {
                                slog::debug!(logger_clone, "Not leader of execution cluster, skipping WorkflowStart proposal";
                                    "workflow_id" => %workflow_id_clone);
                                return;
                            }

                            slog::info!(logger_clone, "Proposing WorkflowStart to execution cluster";
                                "workflow_id" => %workflow_id_clone,
                                "workflow_type" => &workflow_type_clone);

                            // Propose WorkflowStart command to the execution cluster
                            use crate::workflow::commands::*;

                            // Deserialize input from JSON
                            let input_bytes: Vec<u8> = input_json.into_bytes();

                            let workflow_command = WorkflowCommand::WorkflowStart(WorkflowStartData {
                                workflow_id: workflow_id_clone.to_string(),
                                workflow_type: workflow_type_clone.clone(),
                                version,
                                input: input_bytes,
                                owner_node_id: cluster.node_id(),  // This node becomes the owner
                            });

                            match cluster.propose_and_sync(workflow_command).await {
                                Ok(_) => {
                                    slog::info!(logger_clone, "Successfully proposed WorkflowStart";
                                        "workflow_id" => %workflow_id_clone);
                                },
                                Err(e) => {
                                    slog::error!(logger_clone, "Failed to propose WorkflowStart";
                                        "workflow_id" => %workflow_id_clone,
                                        "error" => %e);
                                }
                            }
                        });
                    }
                }
            }

            ManagementCommand::ReportWorkflowStarted(data) => {
                // Keep this for backward compatibility with tests
                slog::info!(self.logger, "Workflow started (legacy)";
                    "workflow_id" => %data.workflow_id,
                    "cluster_id" => %data.cluster_id,
                    "workflow_type" => &data.workflow_type,
                    "version" => data.version
                );

                // Add workflow to cluster's active set
                if let Some(cluster_info) = state.execution_clusters.get_mut(&data.cluster_id) {
                    cluster_info.active_workflows.insert(data.workflow_id);
                } else {
                    slog::warn!(self.logger, "Workflow started on unknown cluster";
                        "cluster_id" => %data.cluster_id,
                        "workflow_id" => %data.workflow_id
                    );
                }

                // Track workflow location
                state.workflow_locations.insert(data.workflow_id, data.cluster_id);
            }

            ManagementCommand::ReportWorkflowEnded(data) => {
                slog::info!(self.logger, "Workflow ended";
                    "workflow_id" => %data.workflow_id,
                    "cluster_id" => %data.cluster_id
                );

                // Check if workflow is active (ignore duplicates)
                let is_active = state.workflow_locations.contains_key(&data.workflow_id);

                if !is_active {
                    // Workflow already completed or never existed - ignore duplicate report
                    slog::debug!(self.logger, "Ignoring duplicate workflow completion report";
                        "workflow_id" => %data.workflow_id
                    );
                    return Ok(());
                }

                // Remove workflow from cluster's active set
                if let Some(cluster_info) = state.execution_clusters.get_mut(&data.cluster_id) {
                    cluster_info.active_workflows.remove(&data.workflow_id);
                }

                // Remove workflow location tracking
                state.workflow_locations.remove(&data.workflow_id);

                // Store result in TTL cache (10 minute TTL)
                if let Some(ref result_json) = data.result_json {
                    let mut cache = state.completed_workflows.lock().unwrap();
                    cache.insert(data.workflow_id, result_json.clone(), Duration::from_secs(600));
                    slog::debug!(self.logger, "Stored workflow result in cache";
                        "workflow_id" => %data.workflow_id,
                        "ttl_seconds" => 600
                    );
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

        // Only the leader should modify the execution cluster and propose state changes
        if !is_leader {
            slog::debug!(self.logger, "Not leader, skipping execution cluster modification");
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

            let logger_clone = self.logger.clone();
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
            let logger_clone = self.logger.clone();
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
            slog::warn!(self.logger, "Workflow cluster reference not set");
        }
    }

    fn on_node_removed(&self, removed_node_id: u64, is_leader: bool) {
        slog::info!(self.logger, "Management cluster detected node removal";
                   "node_id" => removed_node_id, "is_leader" => is_leader);

        // Only the leader should modify the execution cluster and propose state changes
        if !is_leader {
            slog::debug!(self.logger, "Not leader, skipping execution cluster modification");
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

            let logger_clone = self.logger.clone();
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
            let logger_clone = self.logger.clone();
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
            slog::warn!(self.logger, "Workflow cluster reference not set");
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
        assert_eq!(cluster_info.active_workflows.len(), 0);

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
    fn test_workflow_lifecycle() {
        let executor = ManagementCommandExecutor::new();
        let _logger = create_test_logger();

        let cluster_id = Uuid::new_v4();
        let workflow_id = Uuid::new_v4();

        // Create cluster
        executor.apply(&ManagementCommand::CreateExecutionCluster(
            CreateExecutionClusterData {
                cluster_id,
                initial_node_ids: vec![1],
            }
        )).unwrap();

        // Start workflow
        executor.apply(&ManagementCommand::ReportWorkflowStarted(
            WorkflowLifecycleData {
                workflow_id,
                cluster_id,
                workflow_type: "test_workflow".to_string(),
                version: 1,
                timestamp: 12345,
                result_json: None,
            }
        )).unwrap();

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
                result_json: Some("{\"status\":\"success\"}".to_string()),
            }
        )).unwrap();

        // Verify workflow removed
        assert_eq!(executor.get_workflow_location(&workflow_id), None);
        let cluster_info = executor.get_cluster_info(&cluster_id).unwrap();
        assert!(!cluster_info.active_workflows.contains(&workflow_id));
        assert_eq!(executor.get_total_active_workflows(), 0);
    }

    #[test]
    fn test_destroy_cluster_with_workflows_fails() {
        let executor = ManagementCommandExecutor::new();
        let _logger = create_test_logger();

        let cluster_id = Uuid::new_v4();
        let workflow_id = Uuid::new_v4();

        // Create cluster and start workflow
        executor.apply(&ManagementCommand::CreateExecutionCluster(
            CreateExecutionClusterData {
                cluster_id,
                initial_node_ids: vec![1],
            }
        )).unwrap();

        executor.apply(&ManagementCommand::ReportWorkflowStarted(
            WorkflowLifecycleData {
                workflow_id,
                cluster_id,
                workflow_type: "test".to_string(),
                version: 1,
                timestamp: 0,
                result_json: None,
            }
        )).unwrap();

        // Try to destroy cluster - should fail
        let result = executor.apply(&ManagementCommand::DestroyExecutionCluster(cluster_id));
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
                result_json: None,
            }
        )).unwrap();

        // Now destroy should succeed
        executor.apply(&ManagementCommand::DestroyExecutionCluster(cluster_id)).unwrap();
        assert!(executor.get_cluster_info(&cluster_id).is_none());
    }

    #[test]
    fn test_find_least_loaded_cluster() {
        let executor = ManagementCommandExecutor::new();
        let _logger = create_test_logger();

        let cluster1 = Uuid::new_v4();
        let cluster2 = Uuid::new_v4();

        // Create two clusters
        executor.apply(&ManagementCommand::CreateExecutionCluster(
            CreateExecutionClusterData {
                cluster_id: cluster1,
                initial_node_ids: vec![1],
            }
        )).unwrap();

        executor.apply(&ManagementCommand::CreateExecutionCluster(
            CreateExecutionClusterData {
                cluster_id: cluster2,
                initial_node_ids: vec![2],
            }
        )).unwrap();

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
                result_json: None,
            }
        )).unwrap();

        // cluster2 should be least loaded
        assert_eq!(executor.find_least_loaded_cluster(), Some(cluster2));
    }
}
