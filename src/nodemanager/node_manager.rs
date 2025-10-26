///! NodeManager - owns both management and workflow execution clusters

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::collections::HashMap;
use uuid::Uuid;
use crate::raft::RaftCluster;
use crate::raft::generic::grpc_transport::GrpcClusterTransport;
use crate::raft::generic::message::Message;
use crate::workflow::{WorkflowCommand, WorkflowCommandExecutor, WorkflowRuntime};
use super::{ManagementCommand, ManagementCommandExecutor};

/// Initial execution cluster ID (cluster_id = 1)
const INITIAL_EXECUTION_CLUSTER_ID: u128 = 1;

/// NodeManager owns both the management cluster and workflow execution cluster(s)
pub struct NodeManager {
    /// Management cluster - tracks execution cluster membership and workflow lifecycle
    management_cluster: Arc<RaftCluster<ManagementCommandExecutor>>,

    /// Workflow execution clusters (cluster_id -> cluster)
    execution_clusters: HashMap<Uuid, Arc<RaftCluster<WorkflowCommandExecutor>>>,

    /// Round-robin index for cluster selection
    round_robin_index: AtomicUsize,

    /// Workflow runtime (public API for starting workflows)
    workflow_runtime: Arc<WorkflowRuntime>,

    /// ClusterRouter for routing incoming gRPC messages to appropriate clusters
    cluster_router: Arc<crate::grpc::ClusterRouter>,

    /// Transport for accessing node addresses
    transport: Arc<GrpcClusterTransport>,
}

impl NodeManager {
    /// Create a new NodeManager by creating both clusters from a shared transport
    ///
    /// Phase 3: Transport is now type-parameter-free!
    /// The transport can now truly be shared between different cluster types.
    pub async fn new(
        transport: Arc<GrpcClusterTransport>,
        node_id: u64,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Create workflow cluster (cluster_id = 1)
        // RaftCluster::new creates its own mailbox internally
        let executor = WorkflowCommandExecutor::default();
        let transport_ref: Arc<dyn crate::raft::generic::transport::TransportInteraction<Message<WorkflowCommand>>> = transport.clone();
        let workflow_cluster = Arc::new(RaftCluster::new(node_id, 1, transport_ref, executor).await?);

        // Create workflow runtime
        let workflow_runtime = WorkflowRuntime::new(workflow_cluster.clone());

        // Set the runtime reference in the executor so it can spawn workflows
        workflow_cluster.executor.set_runtime(workflow_runtime.clone());

        // Create management cluster (cluster_id = 0) using the same transport
        // Phase 3: Transport is now type-parameter-free and can be shared!
        let management_executor = ManagementCommandExecutor::default();
        let management_transport_ref: Arc<dyn crate::raft::generic::transport::TransportInteraction<Message<ManagementCommand>>> = transport.clone();
        let management_cluster = Arc::new(RaftCluster::new(node_id, 0, management_transport_ref, management_executor).await?);

        // Set cluster references in management executor for dynamic cluster construction
        management_cluster.executor.set_workflow_cluster(workflow_cluster.clone());

        // Create ClusterRouter and register both clusters
        // This enables multi-cluster routing where:
        // - cluster_id = 0 routes to management cluster
        // - cluster_id = 1 routes to initial workflow execution cluster
        use crate::raft::generic::ClusterRouter;
        let mut router = ClusterRouter::new();
        router.register_management_cluster(management_cluster.local_sender.clone());
        router.register_execution_cluster(1, workflow_cluster.local_sender.clone())?;
        let cluster_router = Arc::new(router);

        // Initialize execution clusters HashMap with initial cluster
        let initial_cluster_id = Uuid::from_u128(INITIAL_EXECUTION_CLUSTER_ID);
        let mut execution_clusters = HashMap::new();
        execution_clusters.insert(initial_cluster_id, workflow_cluster);

        Ok(Self {
            management_cluster,
            execution_clusters,
            round_robin_index: AtomicUsize::new(0),
            workflow_runtime,
            cluster_router,
            transport,
        })
    }

    /// Get the cluster router for message routing
    pub fn cluster_router(&self) -> Arc<crate::raft::generic::ClusterRouter> {
        self.cluster_router.clone()
    }

    /// Get the workflow runtime for public API access
    pub fn workflow_runtime(&self) -> Arc<WorkflowRuntime> {
        self.workflow_runtime.clone()
    }

    /// Get the management cluster executor for querying cluster state
    pub fn management_executor(&self) -> &ManagementCommandExecutor {
        &self.management_cluster.executor
    }

    /// Get a specific execution cluster by ID
    pub fn get_execution_cluster(&self, cluster_id: &Uuid) -> Option<Arc<RaftCluster<WorkflowCommandExecutor>>> {
        self.execution_clusters.get(cluster_id).cloned()
    }

    /// Select an execution cluster using round-robin strategy
    /// Returns (cluster_id, cluster) for the selected cluster
    pub fn select_execution_cluster_round_robin(&self) -> (Uuid, Arc<RaftCluster<WorkflowCommandExecutor>>) {
        // Get list of cluster IDs (sorted for deterministic round-robin)
        let mut cluster_ids: Vec<_> = self.execution_clusters.keys().copied().collect();
        cluster_ids.sort(); // Ensure consistent ordering

        if cluster_ids.is_empty() {
            panic!("No execution clusters available");
        }

        // Round-robin selection
        let index = self.round_robin_index.fetch_add(1, Ordering::Relaxed);
        let selected_id = cluster_ids[index % cluster_ids.len()];
        let selected_cluster = self.execution_clusters
            .get(&selected_id)
            .expect("Cluster should exist")
            .clone();

        (selected_id, selected_cluster)
    }

    /// Get an execution cluster executor by ID
    pub fn get_execution_cluster_executor(&self, cluster_id: &Uuid) -> Option<&WorkflowCommandExecutor> {
        self.execution_clusters.get(cluster_id).map(|cluster| &*cluster.executor)
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
        self.execution_clusters
            .values()
            .next()
            .map(|cluster| cluster.node_count())
            .unwrap_or(0)
    }

    /// Get all execution cluster IDs
    pub fn get_execution_cluster_ids(&self) -> Vec<Uuid> {
        self.execution_clusters.keys().copied().collect()
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
