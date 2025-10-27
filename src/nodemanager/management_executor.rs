///! Executor for management commands - applies commands to management cluster state

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use serde::{Deserialize, Serialize};

use crate::raft::generic::message::{CommandExecutor, Message};
use crate::raft::generic::transport::TransportInteraction;
use crate::raft::RaftCluster;
use crate::workflow::{WorkflowCommand, WorkflowCommandExecutor};
use super::management_command::ManagementCommand;

/// Serializable snapshot of management state
/// Used for Raft snapshots
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ManagementStateSnapshot {
    /// All execution clusters
    pub execution_clusters: HashMap<u32, ExecutionClusterInfo>,

    /// Node membership: node_id → set of execution cluster IDs
    pub node_memberships: HashMap<u64, HashSet<u32>>,
}

/// State maintained by the management cluster
#[derive(Clone, Debug)]
pub struct ManagementState {
    /// All execution clusters
    pub execution_clusters: HashMap<u32, ExecutionClusterInfo>,

    /// Node membership: node_id → set of execution cluster IDs
    pub node_memberships: HashMap<u64, HashSet<u32>>,
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
    pub cluster_id: u32,
    pub node_ids: Vec<u64>,
    pub created_at: u64,
}

/// Executor for management cluster commands
pub struct ManagementCommandExecutor {
    state: Arc<Mutex<ManagementState>>,
    management_cluster: Mutex<Option<Arc<RaftCluster<ManagementCommandExecutor>>>>,
    logger: slog::Logger,

    // Shared state for creating execution clusters dynamically
    // This Arc is shared with NodeManager so both can access the same HashMap
    execution_clusters: Mutex<Option<Arc<Mutex<HashMap<u32, Arc<RaftCluster<WorkflowCommandExecutor>>>>>>>,
    transport: Mutex<Option<Arc<crate::raft::generic::grpc_transport::GrpcClusterTransport>>>,
    cluster_router: Mutex<Option<Arc<crate::grpc::ClusterRouter>>>,
    node_id: Mutex<Option<u64>>,
}

impl ManagementCommandExecutor {
    pub fn new() -> Self {
        // Create a default logger with discard drain
        let logger = slog::Logger::root(slog::Discard, slog::o!());
        Self {
            state: Arc::new(Mutex::new(ManagementState::default())),
            management_cluster: Mutex::new(None),
            logger,
            execution_clusters: Mutex::new(None),
            transport: Mutex::new(None),
            cluster_router: Mutex::new(None),
            node_id: Mutex::new(None),
        }
    }

    /// Create a new executor with a specific logger
    pub fn with_logger(logger: slog::Logger) -> Self {
        Self {
            state: Arc::new(Mutex::new(ManagementState::default())),
            management_cluster: Mutex::new(None),
            logger,
            execution_clusters: Mutex::new(None),
            transport: Mutex::new(None),
            cluster_router: Mutex::new(None),
            node_id: Mutex::new(None),
        }
    }

    /// Set the management cluster reference (called after construction)
    pub fn set_management_cluster(&self, cluster: Arc<RaftCluster<ManagementCommandExecutor>>) {
        *self.management_cluster.lock().unwrap() = Some(cluster);
    }

    /// Set shared dependencies for creating execution clusters
    /// The execution_clusters HashMap passed here is shared with NodeManager
    pub fn set_dependencies(
        &self,
        execution_clusters: Arc<Mutex<HashMap<u32, Arc<RaftCluster<WorkflowCommandExecutor>>>>>,
        transport: Arc<crate::raft::generic::grpc_transport::GrpcClusterTransport>,
        cluster_router: Arc<crate::grpc::ClusterRouter>,
        node_id: u64,
    ) {
        *self.execution_clusters.lock().unwrap() = Some(execution_clusters);
        *self.transport.lock().unwrap() = Some(transport);
        *self.cluster_router.lock().unwrap() = Some(cluster_router);
        *self.node_id.lock().unwrap() = Some(node_id);
    }

    pub fn state(&self) -> Arc<Mutex<ManagementState>> {
        self.state.clone()
    }

    /// Get information about a specific execution cluster
    pub fn get_cluster_info(&self, cluster_id: &u32) -> Option<ExecutionClusterInfo> {
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
    pub fn get_node_clusters(&self, node_id: u64) -> Vec<u32> {
        self.state.lock().unwrap()
            .node_memberships
            .get(&node_id)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Find an execution cluster that includes the given node
    pub fn find_cluster_with_node(&self, node_id: u64) -> Option<u32> {
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

                // First, update management state
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

                // Get dependencies for async cluster creation
                let execution_clusters_arc = match self.execution_clusters.lock().unwrap().clone() {
                    Some(arc) => arc,
                    None => {
                        slog::warn!(self.logger, "Execution clusters not initialized, skipping actual cluster creation");
                        return Ok(());
                    }
                };
                let transport = match self.transport.lock().unwrap().clone() {
                    Some(t) => t,
                    None => {
                        slog::warn!(self.logger, "Transport not initialized, skipping actual cluster creation");
                        return Ok(());
                    }
                };
                let cluster_router = match self.cluster_router.lock().unwrap().clone() {
                    Some(r) => r,
                    None => {
                        slog::warn!(self.logger, "ClusterRouter not initialized, skipping actual cluster creation");
                        return Ok(());
                    }
                };
                let node_id = match *self.node_id.lock().unwrap() {
                    Some(id) => id,
                    None => {
                        slog::warn!(self.logger, "Node ID not initialized, skipping actual cluster creation");
                        return Ok(());
                    }
                };

                let cluster_id = data.cluster_id;
                let logger = self.logger.clone();

                // Spawn async task to create the actual RaftCluster
                // This avoids blocking the apply() method
                tokio::spawn(async move {
                    let executor = WorkflowCommandExecutor::default();

                    // Create transport reference
                    let transport_ref: Arc<dyn crate::raft::generic::transport::TransportInteraction<crate::raft::generic::message::Message<crate::workflow::WorkflowCommand>>> = transport.clone();

                    match RaftCluster::new(node_id, cluster_id, transport_ref, executor).await {
                        Ok(cluster) => {
                            let cluster = Arc::new(cluster);

                            // Create WorkflowRuntime for this cluster
                            let workflow_runtime = crate::workflow::runtime::WorkflowRuntime::new(cluster.clone());
                            cluster.executor.set_runtime(workflow_runtime);

                            // Register cluster in ClusterRouter
                            if let Err(e) = cluster_router.register_execution_cluster(cluster_id, cluster.local_sender.clone()) {
                                slog::error!(logger, "Failed to register execution cluster in router"; "error" => %e);
                                return;
                            }

                            // Add to shared execution_clusters HashMap
                            execution_clusters_arc.lock().unwrap().insert(cluster_id, cluster);

                            slog::info!(logger, "Execution cluster created successfully"; "cluster_id" => %cluster_id);
                        }
                        Err(e) => {
                            slog::error!(logger, "Failed to create execution cluster"; "cluster_id" => %cluster_id, "error" => %e);
                        }
                    }
                });
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

                // First, update management state
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

                // Get dependencies for async cluster operation
                let execution_clusters_arc = match self.execution_clusters.lock().unwrap().clone() {
                    Some(arc) => arc,
                    None => {
                        slog::warn!(self.logger, "Execution clusters not initialized, skipping actual node addition");
                        return Ok(());
                    }
                };
                let transport = match self.transport.lock().unwrap().clone() {
                    Some(t) => t,
                    None => {
                        slog::warn!(self.logger, "Transport not initialized, skipping actual node addition");
                        return Ok(());
                    }
                };

                let cluster_id = data.cluster_id;
                let node_id = data.node_id;
                let logger = self.logger.clone();

                // Spawn async task to add node to execution cluster
                tokio::spawn(async move {
                    // Get the node's address from transport
                    let addresses = transport.get_node_addresses_sync(&[node_id]);
                    let address = match addresses.first() {
                        Some(addr) => addr.clone(),
                        None => {
                            slog::error!(logger, "Node address not found in transport"; "node_id" => node_id);
                            return;
                        }
                    };

                    // Get the execution cluster
                    let cluster = {
                        let clusters = execution_clusters_arc.lock().unwrap();
                        match clusters.get(&cluster_id).cloned() {
                            Some(c) => c,
                            None => {
                                slog::error!(logger, "Execution cluster not found"; "cluster_id" => %cluster_id);
                                return;
                            }
                        }
                    };

                    // Add node to execution cluster
                    match cluster.add_node(node_id, address).await {
                        Ok(_) => {
                            slog::info!(logger, "Node added to execution cluster successfully";
                                       "node_id" => node_id, "cluster_id" => %cluster_id);
                        }
                        Err(e) => {
                            slog::error!(logger, "Failed to add node to execution cluster";
                                        "node_id" => node_id, "cluster_id" => %cluster_id, "error" => %e);
                        }
                    }
                });
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

        // Only the leader should propose commands to add nodes to execution clusters
        if !is_leader {
            slog::debug!(self.logger, "Not leader, skipping execution cluster association");
            return;
        }

        // Get management cluster reference for proposing commands
        let mgmt_cluster = match self.management_cluster.lock().unwrap().clone() {
            Some(cluster) => cluster,
            None => {
                slog::warn!(self.logger, "Management cluster not set, cannot propose execution cluster commands");
                return;
            }
        };

        // Default execution cluster ID
        let default_cluster_id = 1u32;

        // Check if default execution cluster exists
        let cluster_exists = {
            let state = self.state.lock().unwrap();
            state.execution_clusters.contains_key(&default_cluster_id)
        };

        // Clone logger for async block
        let logger = self.logger.clone();

        // Spawn async task to propose commands
        tokio::spawn(async move {
            // If cluster doesn't exist, create it first
            if !cluster_exists {
                slog::info!(logger, "Creating default execution cluster"; "cluster_id" => %default_cluster_id);
                let create_command = ManagementCommand::CreateExecutionCluster(
                    super::management_command::CreateExecutionClusterData {
                        cluster_id: default_cluster_id,
                        initial_node_ids: vec![added_node_id],
                    }
                );

                if let Err(e) = mgmt_cluster.propose_and_sync(create_command).await {
                    slog::error!(logger, "Failed to create default execution cluster"; "error" => %e);
                    return;
                }
            }

            // Associate the node with the default execution cluster
            slog::info!(logger, "Associating node with default execution cluster";
                       "node_id" => added_node_id, "cluster_id" => %default_cluster_id);
            let associate_command = ManagementCommand::AssociateNode(
                super::management_command::AssociateNodeData {
                    cluster_id: default_cluster_id,
                    node_id: added_node_id,
                }
            );

            if let Err(e) = mgmt_cluster.propose_and_sync(associate_command).await {
                slog::error!(logger, "Failed to associate node with default cluster"; "error" => %e);
            }
        });
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

    // TODO: These tests need to be rewritten as integration tests since they now
    // require actual RaftCluster creation and transport initialization
    // For now, they're commented out

    // #[test]
    // fn test_create_execution_cluster() { ... }

    // #[test]
    // fn test_associate_and_disassociate_node() { ... }





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
