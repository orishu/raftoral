///! NodeManager - owns both management and workflow execution clusters

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::collections::HashMap;
use crate::raft::RaftCluster;
use crate::raft::generic::grpc_transport::GrpcClusterTransport;
use crate::raft::generic::message::Message;
use crate::workflow::WorkflowCommandExecutor;
use super::{ManagementCommand, ManagementCommandExecutor};

/// Initial execution cluster ID (cluster_id = 1)
const INITIAL_EXECUTION_CLUSTER_ID: u32 = 1;

/// NodeManager owns both the management cluster and workflow execution cluster(s)
pub struct NodeManager {
    /// Management cluster - tracks execution cluster membership and workflow lifecycle
    management_cluster: Arc<RaftCluster<ManagementCommandExecutor>>,

    /// Workflow execution clusters (cluster_id -> cluster)
    /// Shared with ManagementCommandExecutor for dynamic cluster creation
    execution_clusters: Arc<Mutex<HashMap<u32, Arc<RaftCluster<WorkflowCommandExecutor>>>>>,

    /// Round-robin index for cluster selection
    round_robin_index: AtomicUsize,

    /// ClusterRouter for routing incoming gRPC messages to appropriate clusters
    cluster_router: Arc<crate::grpc::ClusterRouter>,

    /// Transport for accessing node addresses
    transport: Arc<GrpcClusterTransport>,

    /// Node ID for this node
    node_id: u64,
}

impl NodeManager {
    /// Create a new NodeManager by creating the management cluster
    ///
    /// Execution clusters will be created dynamically through management commands.
    /// Call initialize_default_cluster() after construction to create the default cluster.
    ///
    /// # Arguments
    /// * `transport` - The gRPC transport for communication
    /// * `node_id` - This node's ID
    /// * `bootstrap` - If true, bootstrap a new cluster (first node). If false, join existing cluster.
    pub async fn new(
        transport: Arc<GrpcClusterTransport>,
        node_id: u64,
        bootstrap: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Create shared execution clusters HashMap (empty initially)
        let execution_clusters = Arc::new(Mutex::new(HashMap::new()));

        // Create management cluster (cluster_id = 0) using the transport
        // For bootstrap mode, pass None to create single-node cluster
        // For join mode, pass empty vec to indicate joining (will be added via ConfChange)
        let management_executor = ManagementCommandExecutor::default();
        let management_transport_ref: Arc<dyn crate::raft::generic::transport::TransportInteraction<Message<ManagementCommand>>> = transport.clone();
        let bootstrap_peers = if bootstrap { None } else { Some(vec![]) };
        let management_cluster = Arc::new(RaftCluster::new(node_id, 0, management_transport_ref, management_executor, bootstrap_peers).await?);

        // Create ClusterRouter and register management cluster
        // Execution clusters will be registered dynamically as they're created
        use crate::raft::generic::ClusterRouter;
        let mut router = ClusterRouter::new();
        router.register_management_cluster(management_cluster.local_sender.clone());
        let cluster_router = Arc::new(router);

        // Set dependencies in management executor so it can create execution clusters
        management_cluster.executor.set_dependencies(
            execution_clusters.clone(),
            transport.clone(),
            cluster_router.clone(),
            node_id,
        );

        // Set management cluster reference for proposing commands
        management_cluster.executor.set_management_cluster(management_cluster.clone());

        Ok(Self {
            management_cluster,
            execution_clusters,
            round_robin_index: AtomicUsize::new(0),
            cluster_router,
            transport,
            node_id,
        })
    }

    /// Initialize the default execution cluster (cluster_id = 1)
    /// This should be called after NodeManager::new() when bootstrapping a cluster
    pub async fn initialize_default_cluster(&self) -> Result<(), String> {
        let cluster_id = INITIAL_EXECUTION_CLUSTER_ID;

        // Propose CreateExecutionCluster command
        let create_command = ManagementCommand::CreateExecutionCluster(
            super::management_command::CreateExecutionClusterData {
                cluster_id,
                initial_node_ids: vec![self.node_id],
            }
        );

        self.management_cluster.propose_and_sync(create_command).await
            .map_err(|e| format!("Failed to create default execution cluster: {}", e))?;

        // Propose AssociateNode command to add this node
        let associate_command = ManagementCommand::AssociateNode(
            super::management_command::AssociateNodeData {
                cluster_id,
                node_id: self.node_id,
            }
        );

        self.management_cluster.propose_and_sync(associate_command).await
            .map_err(|e| format!("Failed to associate node with default cluster: {}", e))?;

        Ok(())
    }

    /// Get the cluster router for message routing
    pub fn cluster_router(&self) -> Arc<crate::raft::generic::ClusterRouter> {
        self.cluster_router.clone()
    }

    /// Get the management cluster executor for querying cluster state
    pub fn management_executor(&self) -> &ManagementCommandExecutor {
        &self.management_cluster.executor
    }

    /// Get the management cluster for accessing leader info
    pub fn management_cluster(&self) -> &Arc<RaftCluster<ManagementCommandExecutor>> {
        &self.management_cluster
    }

    /// Get the address for a specific node ID from the transport
    /// Returns None if node is not found
    pub fn get_node_address(&self, node_id: u64) -> Option<String> {
        let addresses = self.transport.get_node_addresses_sync(&[node_id]);
        addresses.first().cloned()
    }

    /// Get a specific execution cluster by ID
    pub fn get_execution_cluster(&self, cluster_id: &u32) -> Option<Arc<RaftCluster<WorkflowCommandExecutor>>> {
        self.execution_clusters.lock().unwrap().get(cluster_id).cloned()
    }

    /// Select an execution cluster using round-robin strategy
    /// Returns (cluster_id, cluster) for the selected cluster
    pub fn select_execution_cluster_round_robin(&self) -> (u32, Arc<RaftCluster<WorkflowCommandExecutor>>) {
        let clusters = self.execution_clusters.lock().unwrap();

        // Get list of cluster IDs (sorted for deterministic round-robin)
        let mut cluster_ids: Vec<_> = clusters.keys().copied().collect();
        cluster_ids.sort(); // Ensure consistent ordering

        if cluster_ids.is_empty() {
            panic!("No execution clusters available");
        }

        // Round-robin selection
        let index = self.round_robin_index.fetch_add(1, Ordering::Relaxed);
        let selected_id = cluster_ids[index % cluster_ids.len()];
        let selected_cluster = clusters
            .get(&selected_id)
            .expect("Cluster should exist")
            .clone();

        (selected_id, selected_cluster)
    }

    /// Get an execution cluster executor by ID (returns owned reference)
    pub fn get_execution_cluster_executor(&self, cluster_id: &u32) -> Option<Arc<RaftCluster<WorkflowCommandExecutor>>> {
        self.execution_clusters.lock().unwrap().get(cluster_id).cloned()
    }

    /// Add a node to the management cluster
    /// The workflow cluster will be updated automatically via the management executor's
    /// on_node_added callback when the ConfChange is applied
    pub async fn add_node(&self, node_id: u64, address: String) -> Result<(), String> {
        self.management_cluster.add_node(node_id, address).await
            .map_err(|e| format!("Failed to add node to management cluster: {}", e))
    }

    /// Remove a node from the management cluster
    /// The workflow cluster will be updated automatically via the management executor's
    /// on_node_removed callback when the ConfChange is applied
    pub async fn remove_node(&self, node_id: u64) -> Result<(), String> {
        self.management_cluster.remove_node(node_id).await
            .map_err(|e| format!("Failed to remove node from management cluster: {}", e))
    }

    /// Get the total number of nodes across all execution clusters
    /// Note: A node may belong to multiple execution clusters
    pub fn total_execution_cluster_nodes(&self) -> usize {
        // Return size of first execution cluster, or 0 if none exist
        // (For initial cluster, this gives us the cluster size)
        let clusters = self.execution_clusters.lock().unwrap();
        clusters
            .values()
            .next()
            .map(|cluster| cluster.node_count())
            .unwrap_or(0)
    }

    /// Get all execution cluster IDs
    pub fn get_execution_cluster_ids(&self) -> Vec<u32> {
        self.execution_clusters.lock().unwrap().keys().copied().collect()
    }

    /// Get node addresses for a list of node IDs
    pub fn get_node_addresses(&self, node_ids: &[u64]) -> Vec<String> {
        self.transport.get_node_addresses_sync(node_ids)
    }

    /// Propose a management command to the management cluster
    /// Used for scheduling workflows, creating clusters, etc.
    pub async fn propose_management_command(&self, command: super::ManagementCommand) -> Result<(), String> {
        self.management_cluster.propose_and_sync(command).await
            .map_err(|e| format!("Failed to propose management command: {}", e))
    }
}
