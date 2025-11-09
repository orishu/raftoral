//! Management Runtime (Layer 7)
//!
//! Provides a high-level application interface for managing sub-cluster metadata
//! built on the generic2 Raft infrastructure.

use crate::management::{
    ClusterAction, ClusterManager, ClusterManagerConfig, ManagementEvent, ManagementStateMachine,
    SubClusterRuntime,
};
use crate::raft::generic2::{ClusterRouter, EventBus, ProposalRouter, RaftNode, RaftNodeConfig, Transport};
use slog::{info, Logger};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};

/// High-level runtime for managing sub-cluster metadata
///
/// This runtime provides a simple API for:
/// - Creating and deleting sub-clusters
/// - Managing node membership in sub-clusters
/// - Setting and querying sub-cluster metadata
/// - Subscribing to management events
///
/// The type parameter R represents the sub-cluster runtime type (e.g., WorkflowRuntime)
/// that implements the SubClusterRuntime trait. When management events are observed,
/// ManagementRuntime will dynamically create and manage instances of R.
pub struct ManagementRuntime<R>
where
    R: SubClusterRuntime,
{
    /// Proposal router for submitting operations
    proposal_router: Arc<ProposalRouter<ManagementStateMachine>>,

    /// Event bus for management event notifications
    event_bus: Arc<EventBus<ManagementEvent>>,

    /// Cluster router for registering sub-clusters
    cluster_router: Arc<ClusterRouter>,

    /// Transport layer (shared across all clusters)
    transport: Arc<dyn Transport>,

    /// Active sub-cluster runtimes managed by this node
    sub_clusters: Arc<Mutex<HashMap<u32, Arc<R>>>>,

    /// Shared configuration for sub-cluster runtimes
    shared_config: Arc<Mutex<R::SharedConfig>>,

    /// Cluster manager for automatic topology decisions
    cluster_manager: ClusterManager,

    /// Node ID
    node_id: u64,

    /// Cluster ID
    cluster_id: u32,

    /// Logger
    logger: Logger,

    /// Phantom data for sub-cluster runtime type
    _phantom: PhantomData<R>,
}

impl<R> ManagementRuntime<R>
where
    R: SubClusterRuntime,
{
    /// Create a new Management runtime
    ///
    /// # Arguments
    /// * `config` - Raft node configuration
    /// * `transport` - Transport layer for network communication
    /// * `mailbox_rx` - Mailbox receiver for Raft messages
    /// * `cluster_router` - Cluster router for registering sub-clusters
    /// * `shared_config` - Shared configuration for sub-cluster runtimes
    /// * `logger` - Logger instance
    ///
    /// # Returns
    /// A tuple of (Arc<ManagementRuntime>, RaftNode handle for running event loop)
    pub fn new(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc::server::raft_proto::GenericMessage>,
        cluster_router: Arc<ClusterRouter>,
        shared_config: Arc<Mutex<R::SharedConfig>>,
        logger: Logger,
    ) -> Result<
        (
            Arc<Self>,
            Arc<Mutex<RaftNode<ManagementStateMachine>>>,
        ),
        Box<dyn std::error::Error>,
    >
    where
        R: SubClusterRuntime,
    {
        let state_machine = ManagementStateMachine::new();
        let event_bus = Arc::new(EventBus::new(100));

        info!(logger, "Creating Management runtime";
            "node_id" => config.node_id,
            "cluster_id" => config.cluster_id
        );

        // Create RaftNode
        let node = RaftNode::new_single_node(
            config.clone(),
            transport.clone(),
            mailbox_rx,
            state_machine,
            event_bus.clone(),
            logger.clone(),
        )?;

        let node_arc = Arc::new(Mutex::new(node));

        // Create ProposalRouter
        let proposal_router = Arc::new(ProposalRouter::new(
            node_arc.clone(),
            transport.clone(),
            config.cluster_id,
            config.node_id,
            logger.clone(),
        ));

        // Start leader tracker in background
        let router_clone = proposal_router.clone();
        tokio::spawn(async move {
            router_clone.run_leader_tracker().await;
        });

        let runtime = Arc::new(Self {
            proposal_router,
            event_bus: event_bus.clone(),
            cluster_router,
            transport,
            sub_clusters: Arc::new(Mutex::new(HashMap::new())),
            shared_config,
            cluster_manager: ClusterManager::new(ClusterManagerConfig::default()),
            node_id: config.node_id,
            cluster_id: config.cluster_id,
            logger: logger.clone(),
            _phantom: PhantomData,
        });

        // Start event observer in background
        let runtime_clone = runtime.clone();
        tokio::spawn(async move {
            runtime_clone.run_event_observer().await;
        });

        Ok((runtime, node_arc))
    }

    /// Create a new Management runtime for a node joining an existing cluster
    ///
    /// # Arguments
    /// * `config` - Raft node configuration
    /// * `transport` - Transport layer for network communication
    /// * `mailbox_rx` - Mailbox receiver for Raft messages
    /// * `initial_voters` - IDs of all nodes in the cluster (including this node)
    /// * `cluster_router` - Cluster router for registering sub-clusters
    /// * `shared_config` - Shared configuration for sub-cluster runtimes
    /// * `logger` - Logger instance
    ///
    /// # Returns
    /// A tuple of (Arc<ManagementRuntime>, RaftNode handle for running event loop)
    pub fn new_joining_node(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc::server::raft_proto::GenericMessage>,
        initial_voters: Vec<u64>,
        cluster_router: Arc<ClusterRouter>,
        shared_config: Arc<Mutex<R::SharedConfig>>,
        logger: Logger,
    ) -> Result<
        (
            Arc<Self>,
            Arc<Mutex<RaftNode<ManagementStateMachine>>>,
        ),
        Box<dyn std::error::Error>,
    >
    where
        R: SubClusterRuntime,
    {
        let state_machine = ManagementStateMachine::new();
        let event_bus = Arc::new(EventBus::new(100));

        info!(logger, "Creating Management runtime (joining existing cluster)";
            "node_id" => config.node_id,
            "cluster_id" => config.cluster_id,
            "initial_voters" => ?initial_voters
        );

        // Create RaftNode for joining a multi-node cluster with known peers
        let node = RaftNode::new_multi_node_with_peers(
            config.clone(),
            transport.clone(),
            mailbox_rx,
            state_machine,
            event_bus.clone(),
            initial_voters,
            logger.clone(),
        )?;

        let node_arc = Arc::new(Mutex::new(node));

        // Create ProposalRouter
        let proposal_router = Arc::new(ProposalRouter::new(
            node_arc.clone(),
            transport.clone(),
            config.cluster_id,
            config.node_id,
            logger.clone(),
        ));

        // Start leader tracker in background
        let router_clone = proposal_router.clone();
        tokio::spawn(async move {
            router_clone.run_leader_tracker().await;
        });

        let runtime = Arc::new(Self {
            proposal_router,
            event_bus: event_bus.clone(),
            cluster_router,
            transport,
            sub_clusters: Arc::new(Mutex::new(HashMap::new())),
            shared_config,
            cluster_manager: ClusterManager::new(ClusterManagerConfig::default()),
            node_id: config.node_id,
            cluster_id: config.cluster_id,
            logger: logger.clone(),
            _phantom: PhantomData,
        });

        // Start event observer in background
        let runtime_clone = runtime.clone();
        tokio::spawn(async move {
            runtime_clone.run_event_observer().await;
        });

        Ok((runtime, node_arc))
    }

    /// Create a new Management runtime for a node joining an existing cluster as a LEARNER
    ///
    /// This constructor is used when joining a large cluster where only a limited number
    /// of nodes should be voters. Learner nodes receive log updates but don't participate
    /// in voting for consensus.
    ///
    /// # Arguments
    /// * `config` - Raft node configuration
    /// * `transport` - Transport layer for network communication
    /// * `mailbox_rx` - Mailbox receiver for Raft messages
    /// * `existing_voters` - IDs of the existing voter nodes (NOT including this node)
    /// * `cluster_router` - Cluster router for registering sub-clusters
    /// * `shared_config` - Shared configuration for sub-cluster runtimes
    /// * `logger` - Logger instance
    ///
    /// # Returns
    /// A tuple of (Arc<ManagementRuntime>, RaftNode handle for running event loop)
    ///
    /// # Example
    /// ```ignore
    /// // Node 6 joins a cluster with 5 existing voters (1-5) as a learner
    /// let (runtime, node) = ManagementRuntime::new_joining_learner(
    ///     config,
    ///     transport,
    ///     mailbox_rx,
    ///     vec![1, 2, 3, 4, 5], // existing voters
    ///     cluster_router,
    ///     shared_config,
    ///     logger,
    /// )?;
    /// // Node 6 will be a learner, receiving updates but not voting
    /// ```
    pub fn new_joining_learner(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc::server::raft_proto::GenericMessage>,
        existing_voters: Vec<u64>,
        cluster_router: Arc<ClusterRouter>,
        shared_config: Arc<Mutex<R::SharedConfig>>,
        logger: Logger,
    ) -> Result<
        (
            Arc<Self>,
            Arc<Mutex<RaftNode<ManagementStateMachine>>>,
        ),
        Box<dyn std::error::Error>,
    >
    where
        R: SubClusterRuntime,
    {
        let state_machine = ManagementStateMachine::new();
        let event_bus = Arc::new(EventBus::new(100));

        info!(logger, "Creating Management runtime (joining as learner)";
            "node_id" => config.node_id,
            "cluster_id" => config.cluster_id,
            "existing_voters" => ?existing_voters
        );

        // Create ConfState with existing voters and this node as a learner
        use raft::prelude::ConfState;
        let conf_state = ConfState {
            voters: existing_voters,
            learners: vec![config.node_id],
            ..Default::default()
        };

        // Create RaftNode with explicit ConfState (learner mode)
        let node = RaftNode::new_with_conf_state(
            config.clone(),
            transport.clone(),
            mailbox_rx,
            state_machine,
            event_bus.clone(),
            conf_state,
            logger.clone(),
        )?;

        let node_arc = Arc::new(Mutex::new(node));

        // Create ProposalRouter
        let proposal_router = Arc::new(ProposalRouter::new(
            node_arc.clone(),
            transport.clone(),
            config.cluster_id,
            config.node_id,
            logger.clone(),
        ));

        // Start leader tracker in background
        let router_clone = proposal_router.clone();
        tokio::spawn(async move {
            router_clone.run_leader_tracker().await;
        });

        let runtime = Arc::new(Self {
            proposal_router,
            event_bus: event_bus.clone(),
            cluster_router,
            transport,
            sub_clusters: Arc::new(Mutex::new(HashMap::new())),
            shared_config,
            cluster_manager: ClusterManager::new(ClusterManagerConfig::default()),
            node_id: config.node_id,
            cluster_id: config.cluster_id,
            logger: logger.clone(),
            _phantom: PhantomData,
        });

        // Start event observer in background
        let runtime_clone = runtime.clone();
        tokio::spawn(async move {
            runtime_clone.run_event_observer().await;
        });

        Ok((runtime, node_arc))
    }

    /// Create a new sub-cluster
    ///
    /// # Arguments
    /// * `node_ids` - Initial node IDs to include in the sub-cluster
    ///
    /// # Returns
    /// * `Ok(cluster_id)` - Operation completed successfully, returns the assigned cluster ID
    /// * `Err(String)` - Operation failed
    pub async fn create_sub_cluster(
        &self,
        node_ids: Vec<u64>,
    ) -> Result<u32, String> {
        info!(self.logger, "Creating sub-cluster";
            "node_ids" => ?node_ids
        );

        // Subscribe to events to capture the assigned cluster_id
        let mut event_rx = self.event_bus.subscribe();

        let command = crate::management::state_machine::ManagementCommand::CreateSubCluster {
            node_ids,
        };

        // Propose the command
        self.proposal_router
            .propose_and_wait(command)
            .await
            .map_err(|e| format!("{}", e))?;

        // Wait for the SubClusterCreated event to get the assigned cluster_id
        // We need to do this because the cluster_id is assigned by the state machine
        loop {
            match event_rx.recv().await {
                Ok(ManagementEvent::SubClusterCreated { cluster_id, .. }) => {
                    info!(self.logger, "Sub-cluster created"; "cluster_id" => cluster_id);
                    return Ok(cluster_id);
                }
                Ok(_) => continue, // Ignore other events
                Err(_) => return Err("Event channel closed".to_string()),
            }
        }
    }

    /// Delete a sub-cluster
    ///
    /// # Arguments
    /// * `cluster_id` - Unique identifier of the sub-cluster to delete
    ///
    /// # Returns
    /// * `Ok(())` - Operation completed successfully
    /// * `Err(String)` - Operation failed
    pub async fn delete_sub_cluster(&self, cluster_id: u32) -> Result<(), String> {
        info!(self.logger, "Deleting sub-cluster"; "cluster_id" => cluster_id);

        let command = crate::management::state_machine::ManagementCommand::DeleteSubCluster {
            cluster_id,
        };

        self.proposal_router
            .propose_and_wait(command)
            .await
            .map_err(|e| format!("{}", e))
    }

    /// Add a node to a sub-cluster
    ///
    /// # Arguments
    /// * `cluster_id` - Sub-cluster identifier
    /// * `node_id` - Node ID to add
    ///
    /// # Returns
    /// * `Ok(())` - Operation completed successfully
    /// * `Err(String)` - Operation failed
    pub async fn add_node_to_sub_cluster(
        &self,
        cluster_id: u32,
        node_id: u64,
    ) -> Result<(), String> {
        info!(self.logger, "Adding node to sub-cluster";
            "cluster_id" => cluster_id,
            "node_id" => node_id
        );

        let command =
            crate::management::state_machine::ManagementCommand::AddNodeToSubCluster {
                cluster_id,
                node_id,
            };

        let result = self
            .proposal_router
            .propose_and_wait(command)
            .await
            .map_err(|e| format!("{}", e));

        // After successful addition, check if any clusters need splitting
        if result.is_ok() {
            info!(self.logger, "Node added to sub-cluster, invoking ClusterManager to check for splits";
                "cluster_id" => cluster_id,
                "node_id" => node_id
            );

            let state = self.get_state_snapshot().await;
            let actions = self.cluster_manager.decide_splits(&state);

            // Execute any split actions
            self.execute_actions(actions).await;
        }

        result
    }

    /// Remove a node from a sub-cluster
    ///
    /// # Arguments
    /// * `cluster_id` - Sub-cluster identifier
    /// * `node_id` - Node ID to remove
    ///
    /// # Returns
    /// * `Ok(())` - Operation completed successfully
    /// * `Err(String)` - Operation failed
    pub async fn remove_node_from_sub_cluster(
        &self,
        cluster_id: u32,
        node_id: u64,
    ) -> Result<(), String> {
        info!(self.logger, "Removing node from sub-cluster";
            "cluster_id" => cluster_id,
            "node_id" => node_id
        );

        let command =
            crate::management::state_machine::ManagementCommand::RemoveNodeFromSubCluster {
                cluster_id,
                node_id,
            };

        let result = self
            .proposal_router
            .propose_and_wait(command)
            .await
            .map_err(|e| format!("{}", e));

        // After successful removal, check if consolidation is needed
        if result.is_ok() {
            info!(self.logger, "Node removed from sub-cluster, invoking ClusterManager to check for consolidation";
                "cluster_id" => cluster_id,
                "node_id" => node_id
            );

            let state = self.get_state_snapshot().await;
            let actions = self.cluster_manager.decide_consolidation(&state);

            // Execute any consolidation actions
            self.execute_actions(actions).await;
        }

        result
    }

    /// Set metadata for a sub-cluster
    ///
    /// # Arguments
    /// * `cluster_id` - Sub-cluster identifier
    /// * `key` - Metadata key
    /// * `value` - Metadata value
    ///
    /// # Returns
    /// * `Ok(())` - Operation completed successfully
    /// * `Err(String)` - Operation failed
    pub async fn set_metadata(
        &self,
        cluster_id: u32,
        key: String,
        value: String,
    ) -> Result<(), String> {
        info!(self.logger, "Setting sub-cluster metadata";
            "cluster_id" => cluster_id,
            "key" => &key,
            "value" => &value
        );

        let command = crate::management::state_machine::ManagementCommand::SetMetadata {
            cluster_id,
            key,
            value,
        };

        self.proposal_router
            .propose_and_wait(command)
            .await
            .map_err(|e| format!("{}", e))
    }

    /// Delete metadata for a sub-cluster
    ///
    /// # Arguments
    /// * `cluster_id` - Sub-cluster identifier
    /// * `key` - Metadata key to delete
    ///
    /// # Returns
    /// * `Ok(())` - Operation completed successfully
    /// * `Err(String)` - Operation failed
    pub async fn delete_metadata(&self, cluster_id: u32, key: String) -> Result<(), String> {
        info!(self.logger, "Deleting sub-cluster metadata";
            "cluster_id" => cluster_id,
            "key" => &key
        );

        let command = crate::management::state_machine::ManagementCommand::DeleteMetadata {
            cluster_id,
            key,
        };

        self.proposal_router
            .propose_and_wait(command)
            .await
            .map_err(|e| format!("{}", e))
    }

    /// Get sub-cluster metadata (reads from local state machine)
    ///
    /// # Arguments
    /// * `cluster_id` - Sub-cluster identifier
    ///
    /// # Returns
    /// * `Some(metadata)` - Sub-cluster exists with this metadata
    /// * `None` - Sub-cluster does not exist
    pub async fn get_sub_cluster(
        &self,
        cluster_id: &u32,
    ) -> Option<crate::management::state_machine::SubClusterMetadata> {
        let node_arc = self.proposal_router.node();
        let node = node_arc.lock().await;
        let sm = node.state_machine().clone();
        drop(node);

        sm.lock().await.get_sub_cluster(cluster_id).cloned()
    }

    /// List all sub-cluster IDs (reads from local state machine)
    ///
    /// # Returns
    /// Vector of all sub-cluster IDs
    pub async fn list_sub_clusters(&self) -> Vec<u32> {
        let node_arc = self.proposal_router.node();
        let node = node_arc.lock().await;
        let sm = node.state_machine().clone();
        drop(node);

        sm.lock().await.list_sub_clusters()
    }

    /// Get all sub-cluster metadata (reads from local state machine)
    ///
    /// # Returns
    /// HashMap of all sub-cluster metadata
    pub async fn get_all_sub_clusters(
        &self,
    ) -> std::collections::HashMap<u32, crate::management::state_machine::SubClusterMetadata>
    {
        let node_arc = self.proposal_router.node();
        let node = node_arc.lock().await;
        let sm = node.state_machine().clone();
        drop(node);

        sm.lock().await.get_all_sub_clusters().clone()
    }

    /// Get a reference to a sub-cluster runtime instance
    ///
    /// # Arguments
    /// * `cluster_id` - ID of the sub-cluster
    ///
    /// # Returns
    /// * `Some(Arc<R>)` - Sub-cluster runtime if it exists on this node
    /// * `None` - Sub-cluster does not exist or hasn't been created yet
    pub async fn get_sub_cluster_runtime(&self, cluster_id: &u32) -> Option<Arc<R>> {
        self.sub_clusters.lock().await.get(cluster_id).cloned()
    }

    /// Check if a node is a voter in the management cluster
    ///
    /// # Arguments
    /// * `node_id` - ID of the node to check
    ///
    /// # Returns
    /// * `true` if the node is a voter
    /// * `false` if the node is a learner or not in the cluster
    pub async fn is_management_voter(&self, node_id: u64) -> bool {
        let node = self.proposal_router.node();
        let node_guard = node.lock().await;
        let conf_state = node_guard.conf_state().await;
        conf_state.voters.contains(&node_id)
    }

    /// Get all voters in the management cluster
    ///
    /// # Returns
    /// Vector of node IDs that are voters in the management cluster
    pub async fn get_management_voters(&self) -> Vec<u64> {
        let node = self.proposal_router.node();
        let node_guard = node.lock().await;
        let conf_state = node_guard.conf_state().await;
        conf_state.voters.clone()
    }

    /// Get all learners in the management cluster
    ///
    /// # Returns
    /// Vector of node IDs that are learners in the management cluster
    pub async fn get_management_learners(&self) -> Vec<u64> {
        let node = self.proposal_router.node();
        let node_guard = node.lock().await;
        let conf_state = node_guard.conf_state().await;
        conf_state.learners.clone()
    }

    /// Subscribe to management events
    ///
    /// # Returns
    /// Receiver for ManagementEvent notifications
    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<ManagementEvent> {
        self.event_bus.subscribe()
    }

    /// Check if this node is the leader
    pub async fn is_leader(&self) -> bool {
        self.proposal_router.is_leader().await
    }

    /// Get the current leader ID (if known)
    pub async fn leader_id(&self) -> Option<u64> {
        self.proposal_router.leader_id().await
    }

    /// Get this node's ID
    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    /// Get the cluster ID
    pub fn cluster_id(&self) -> u32 {
        self.cluster_id
    }

    /// Get a snapshot of the current management state
    ///
    /// This is used by ClusterManager to make decisions based on current state.
    async fn get_state_snapshot(&self) -> ManagementStateMachine {
        let node = self.proposal_router.node();
        let node_guard = node.lock().await;
        let state = node_guard.state_machine().clone();
        drop(node_guard);
        let snapshot = state.lock().await.clone();
        snapshot
    }

    /// Execute cluster actions by proposing corresponding commands
    ///
    /// This helper translates ClusterActions from ClusterManager into
    /// ManagementCommands that are proposed via Raft.
    async fn execute_actions(&self, actions: Vec<ClusterAction>) {
        Self::execute_actions_static(
            self.proposal_router.clone(),
            self.logger.clone(),
            actions,
        )
        .await
    }

    /// Static helper to execute cluster actions
    ///
    /// This is a standalone function that can be called from spawned tasks
    /// without requiring a reference to ManagementRuntime.
    async fn execute_actions_static(
        proposal_router: Arc<ProposalRouter<ManagementStateMachine>>,
        logger: Logger,
        actions: Vec<ClusterAction>,
    ) {
        use slog::info;

        for action in actions {
            match action {
                ClusterAction::AddNodeToCluster { cluster_id, node_id } => {
                    info!(logger, "ClusterManager action: AddNodeToCluster";
                        "cluster_id" => cluster_id,
                        "node_id" => node_id
                    );
                    let command = crate::management::state_machine::ManagementCommand::AddNodeToSubCluster {
                        cluster_id,
                        node_id,
                    };
                    let _ = proposal_router.propose(command).await;
                }

                ClusterAction::CreateCluster { node_ids } => {
                    info!(logger, "ClusterManager action: CreateCluster";
                        "node_ids" => ?node_ids
                    );
                    let command = crate::management::state_machine::ManagementCommand::CreateSubCluster {
                        node_ids,
                    };
                    let _ = proposal_router.propose(command).await;
                }

                ClusterAction::RemoveNodesFromCluster { cluster_id, node_ids } => {
                    info!(logger, "ClusterManager action: RemoveNodesFromCluster";
                        "cluster_id" => cluster_id,
                        "node_ids" => ?node_ids
                    );
                    // Propose multiple RemoveNodeFromSubCluster commands
                    for node_id in node_ids {
                        let cmd = crate::management::state_machine::ManagementCommand::RemoveNodeFromSubCluster {
                            cluster_id,
                            node_id,
                        };
                        let _ = proposal_router.propose(cmd).await;
                    }
                }

                ClusterAction::DrainCluster { cluster_id } => {
                    info!(logger, "ClusterManager action: DrainCluster";
                        "cluster_id" => cluster_id
                    );
                    let command = crate::management::state_machine::ManagementCommand::SetMetadata {
                        cluster_id,
                        key: "draining".to_string(),
                        value: "true".to_string(),
                    };
                    let _ = proposal_router.propose(command).await;
                }

                ClusterAction::DestroyCluster { cluster_id } => {
                    info!(logger, "ClusterManager action: DestroyCluster";
                        "cluster_id" => cluster_id
                    );
                    let command = crate::management::state_machine::ManagementCommand::DeleteSubCluster {
                        cluster_id,
                    };
                    let _ = proposal_router.propose(command).await;
                }
            }
        }
    }

    /// Add a node to the management cluster
    ///
    /// # Arguments
    /// * `node_id` - ID of the node to add
    /// * `address` - Network address of the node
    ///
    /// # Returns
    /// Receiver that will be notified when the configuration change completes
    pub async fn add_node(
        &self,
        node_id: u64,
        address: String,
    ) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        info!(self.logger, "Adding node to management cluster"; "node_id" => node_id, "address" => &address);

        // Register the node's address for later use when adding to sub-clusters
        let register_cmd = crate::management::state_machine::ManagementCommand::RegisterNodeAddress {
            node_id,
            address: address.clone(),
        };

        // Propose the address registration (fire and forget, no need to wait)
        let _ = self.proposal_router.propose(register_cmd).await;

        // Add the node to the management cluster's Raft configuration
        let result = self.proposal_router.add_node(node_id, address).await;

        // Spawn background task to invoke ClusterManager after Raft operation completes
        if result.is_ok() {
            let proposal_router = self.proposal_router.clone();
            let cluster_manager = self.cluster_manager.clone();
            let logger = self.logger.clone();

            tokio::spawn(async move {
                // Wait briefly for the Raft operation to be applied
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                info!(logger, "Node added to management cluster, invoking ClusterManager for placement decision"; "node_id" => node_id);

                // Get state snapshot and decide placement
                let state = {
                    let node = proposal_router.node();
                    let node_guard = node.lock().await;
                    let state = node_guard.state_machine().clone();
                    drop(node_guard);
                    let snapshot = state.lock().await.clone();
                    snapshot
                };

                let actions = cluster_manager.decide_node_placement(&state, node_id);

                // Execute the placement actions
                ManagementRuntime::<R>::execute_actions_static(proposal_router, logger, actions).await;
            });
        }

        result
    }

    /// Remove a node from the management cluster
    ///
    /// # Arguments
    /// * `node_id` - ID of the node to remove
    ///
    /// # Returns
    /// Receiver that will be notified when the configuration change completes
    pub async fn remove_node(
        &self,
        node_id: u64,
    ) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        info!(self.logger, "Removing node from management cluster"; "node_id" => node_id);
        let result = self.proposal_router.remove_node(node_id).await;

        // Spawn background task to invoke ClusterManager after Raft operation completes
        if result.is_ok() {
            let proposal_router = self.proposal_router.clone();
            let cluster_manager = self.cluster_manager.clone();
            let logger = self.logger.clone();

            tokio::spawn(async move {
                // Wait briefly for the Raft operation to be applied
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                info!(logger, "Node removed from management cluster, invoking ClusterManager for rebalancing decision"; "node_id" => node_id);

                // Get state snapshot and decide rebalancing
                let state = {
                    let node = proposal_router.node();
                    let node_guard = node.lock().await;
                    let state = node_guard.state_machine().clone();
                    drop(node_guard);
                    let snapshot = state.lock().await.clone();
                    snapshot
                };

                let actions = cluster_manager.decide_rebalancing(&state, node_id);

                // Execute the rebalancing actions
                ManagementRuntime::<R>::execute_actions_static(proposal_router, logger, actions).await;
            });
        }

        result
    }

    /// Run event observer for dynamic sub-cluster management (private background task)
    ///
    /// This method observes management events and dynamically creates/manages sub-cluster runtimes:
    /// - SubClusterCreated: Creates a new sub-cluster runtime if this node is in the node list
    /// - NodeAddedToSubCluster: Adds node to existing sub-cluster or joins as new member
    /// - NodeRemovedFromSubCluster: Removes node from sub-cluster or shuts down if removed
    /// - SubClusterDeleted: Shuts down and removes sub-cluster runtime
    async fn run_event_observer(self: Arc<Self>)
    where
        R: SubClusterRuntime,
    {
        use slog::{error, info};

        let mut event_rx = self.event_bus.subscribe();

        info!(self.logger, "Management event observer started");

        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    match event {
                        ManagementEvent::SubClusterCreated {
                            cluster_id,
                            node_ids,
                        } => {
                            // Check if this node is in the cluster
                            if !node_ids.contains(&self.node_id) {
                                continue;
                            }

                            info!(self.logger, "Sub-cluster created event received";
                                "cluster_id" => cluster_id,
                                "node_ids" => ?node_ids,
                                "this_node" => self.node_id
                            );

                            // Check if we're the first node (leader/initiator)
                            let is_first_node = node_ids.first() == Some(&self.node_id);

                            // Create mailbox for this cluster
                            let (mailbox_tx, mailbox_rx) = mpsc::channel(1000);

                            // Validate cluster_id (0 is reserved for management cluster)
                            if cluster_id == 0 {
                                error!(self.logger, "Invalid cluster_id (cannot be 0)");
                                continue;
                            }

                            // Register cluster in cluster router
                            self.cluster_router.register_cluster(cluster_id, mailbox_tx).await;

                            // Create config for sub-cluster
                            let config = RaftNodeConfig {
                                node_id: self.node_id,
                                cluster_id,
                                snapshot_interval: 100,
                                ..Default::default()
                            };

                            // Create runtime based on whether we're first node or joining
                            let (runtime, node) = if is_first_node {
                                info!(self.logger, "Creating single-node sub-cluster";
                                    "cluster_id" => cluster_id
                                );
                                match R::new_single_node(
                                    config,
                                    self.transport.clone(),
                                    mailbox_rx,
                                    self.shared_config.clone(),
                                    self.logger.clone(),
                                ) {
                                    Ok(pair) => pair,
                                    Err(e) => {
                                        let error_msg = e.to_string();
                                        error!(self.logger, "Failed to create sub-cluster runtime";
                                            "cluster_id" => cluster_id,
                                            "error" => error_msg
                                        );
                                        continue;
                                    }
                                }
                            } else {
                                info!(self.logger, "Joining existing sub-cluster";
                                    "cluster_id" => cluster_id,
                                    "initial_voters" => ?node_ids
                                );
                                match R::new_joining_node(
                                    config,
                                    self.transport.clone(),
                                    mailbox_rx,
                                    node_ids.clone(),
                                    self.shared_config.clone(),
                                    self.logger.clone(),
                                ) {
                                    Ok(pair) => pair,
                                    Err(e) => {
                                        let error_msg = e.to_string();
                                        error!(self.logger, "Failed to create sub-cluster runtime";
                                            "cluster_id" => cluster_id,
                                            "error" => error_msg
                                        );
                                        continue;
                                    }
                                }
                            };

                            // Store runtime
                            self.sub_clusters.lock().await.insert(cluster_id, Arc::new(runtime));

                            // Run node in background
                            tokio::spawn(async move {
                                let _ = RaftNode::run_from_arc(node).await;
                            });

                            info!(self.logger, "Sub-cluster runtime created successfully";
                                "cluster_id" => cluster_id
                            );
                        }

                        ManagementEvent::NodeAddedToSubCluster {
                            cluster_id,
                            node_id,
                        } => {
                            info!(self.logger, "Node added to sub-cluster event";
                                "cluster_id" => cluster_id,
                                "node_id" => node_id
                            );

                            // Case 1: If we own this sub-cluster, add the node to the Raft configuration
                            if let Some(sub_cluster) = self.sub_clusters.lock().await.get(&cluster_id).cloned() {
                                // Check if we're the owner of this sub-cluster
                                if sub_cluster.node_id() == self.node_id {
                                    // Owner responsibilities: add the node to the sub-cluster's Raft configuration
                                    info!(self.logger, "This node owns sub-cluster, checking if leader";
                                        "cluster_id" => cluster_id,
                                        "owner_node_id" => sub_cluster.node_id()
                                    );

                                    // Get the node's address from the management state machine
                                    let node_arc = self.proposal_router.node();
                                    let node_guard = node_arc.lock().await;
                                    let sm = node_guard.state_machine().clone();
                                    drop(node_guard);

                                    if let Some(address) = sm.lock().await.get_node_address(&node_id).cloned() {
                                        info!(self.logger, "Adding node to sub-cluster Raft configuration";
                                            "cluster_id" => cluster_id,
                                            "node_id" => node_id,
                                            "address" => &address
                                        );

                                        // Add the node to the sub-cluster (spawned to avoid blocking)
                                        let sub_cluster_clone = sub_cluster.clone();
                                        let logger_clone = self.logger.clone();
                                        tokio::spawn(async move {
                                            match sub_cluster_clone.add_node(node_id, address).await {
                                                Ok(_rx) => {
                                                    info!(logger_clone, "Successfully added node to sub-cluster";
                                                        "cluster_id" => cluster_id,
                                                        "node_id" => node_id
                                                    );
                                                }
                                                Err(e) => {
                                                    slog::error!(logger_clone, "Failed to add node to sub-cluster";
                                                        "cluster_id" => cluster_id,
                                                        "node_id" => node_id,
                                                        "error" => e
                                                    );
                                                }
                                            }
                                        });
                                    } else {
                                        slog::error!(self.logger, "Node address not found in management state";
                                            "node_id" => node_id
                                        );
                                    }
                                }
                            }
                            // Case 2: If THIS node is the one being added, join the sub-cluster
                            else if node_id == self.node_id {
                                info!(self.logger, "This node is being added to sub-cluster, joining as new member";
                                    "cluster_id" => cluster_id,
                                    "node_id" => node_id
                                );

                                // Get the sub-cluster metadata to find all member nodes
                                let node_arc = self.proposal_router.node();
                                let node_guard = node_arc.lock().await;
                                let sm = node_guard.state_machine().clone();
                                drop(node_guard);

                                if let Some(metadata) = sm.lock().await.get_sub_cluster(&cluster_id) {
                                    info!(self.logger, "Joining multi-node sub-cluster";
                                        "cluster_id" => cluster_id,
                                        "node_ids" => ?metadata.node_ids
                                    );

                                    // Create a new mailbox for this sub-cluster
                                    let (mailbox_tx, mailbox_rx) = mpsc::channel(100);

                                    // Validate cluster_id (0 is reserved for management)
                                    if cluster_id == 0 {
                                        error!(self.logger, "Invalid cluster_id (cannot be 0)");
                                        continue;
                                    }

                                    // Register cluster in cluster router
                                    self.cluster_router.register_cluster(cluster_id, mailbox_tx).await;

                                    // Create config for sub-cluster
                                    let config = RaftNodeConfig {
                                        node_id: self.node_id,
                                        cluster_id,
                                        snapshot_interval: 100,
                                        ..Default::default()
                                    };

                                    // Create runtime joining existing cluster
                                    let (runtime, node) = match R::new_joining_node(
                                        config,
                                        self.transport.clone(),
                                        mailbox_rx,
                                        metadata.node_ids.clone(),
                                        self.shared_config.clone(),
                                        self.logger.clone(),
                                    ) {
                                        Ok(pair) => pair,
                                        Err(e) => {
                                            let error_msg = e.to_string();
                                            error!(self.logger, "Failed to create joining sub-cluster runtime";
                                                "cluster_id" => cluster_id,
                                                "error" => error_msg
                                            );
                                            continue;
                                        }
                                    };

                                    // Store runtime
                                    self.sub_clusters.lock().await.insert(cluster_id, Arc::new(runtime));

                                    // Run node in background
                                    tokio::spawn(async move {
                                        let _ = RaftNode::run_from_arc(node).await;
                                    });

                                    info!(self.logger, "Sub-cluster runtime created successfully (joined existing cluster)";
                                        "cluster_id" => cluster_id
                                    );
                                } else {
                                    slog::error!(self.logger, "Sub-cluster metadata not found";
                                        "cluster_id" => cluster_id
                                    );
                                }
                            }
                        }

                        ManagementEvent::NodeRemovedFromSubCluster {
                            cluster_id,
                            node_id,
                        } => {
                            info!(self.logger, "Node removed from sub-cluster event";
                                "cluster_id" => cluster_id,
                                "node_id" => node_id
                            );

                            // If this is this node being removed, shut down the runtime
                            if node_id == self.node_id {
                                info!(self.logger, "This node removed from sub-cluster, shutting down";
                                    "cluster_id" => cluster_id
                                );
                                self.sub_clusters.lock().await.remove(&cluster_id);
                            }

                            // TODO: If we own the cluster, remove via proposal router
                        }

                        ManagementEvent::SubClusterDeleted { cluster_id } => {
                            info!(self.logger, "Sub-cluster deleted event";
                                "cluster_id" => cluster_id
                            );

                            // Shut down and remove the runtime
                            self.sub_clusters.lock().await.remove(&cluster_id);
                        }

                        _ => {
                            // Ignore other events (metadata changes)
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Continue on lag
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    info!(self.logger, "Management event observer stopped (event channel closed)");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::generic2::{InProcessMessageSender, InProcessServer, TransportLayer};
    use crate::raft::generic2::errors::TransportError;
    use crate::raft::generic2::StateMachine;

    // Dummy shared config for testing
    pub struct DummyConfig;

    // Dummy implementation for testing
    impl SubClusterRuntime for () {
        type StateMachine = DummyStateMachine;
        type SharedConfig = DummyConfig;

        fn new_single_node(
            _config: RaftNodeConfig,
            _transport: Arc<dyn crate::raft::generic2::Transport>,
            _mailbox_rx: mpsc::Receiver<crate::grpc::server::raft_proto::GenericMessage>,
            _shared_config: Arc<Mutex<Self::SharedConfig>>,
            _logger: Logger,
        ) -> Result<(Self, Arc<Mutex<RaftNode<Self::StateMachine>>>), Box<dyn std::error::Error>> {
            unimplemented!("Dummy implementation for testing")
        }

        fn new_joining_node(
            _config: RaftNodeConfig,
            _transport: Arc<dyn crate::raft::generic2::Transport>,
            _mailbox_rx: mpsc::Receiver<crate::grpc::server::raft_proto::GenericMessage>,
            _initial_voters: Vec<u64>,
            _shared_config: Arc<Mutex<Self::SharedConfig>>,
            _logger: Logger,
        ) -> Result<(Self, Arc<Mutex<RaftNode<Self::StateMachine>>>), Box<dyn std::error::Error>> {
            unimplemented!("Dummy implementation for testing")
        }

        async fn add_node(&self, _node_id: u64, _address: String)
            -> Result<oneshot::Receiver<Result<(), String>>, String> {
            unimplemented!("Dummy implementation for testing")
        }

        async fn remove_node(&self, _node_id: u64)
            -> Result<oneshot::Receiver<Result<(), String>>, String> {
            unimplemented!("Dummy implementation for testing")
        }

        fn node_id(&self) -> u64 {
            0
        }

        fn cluster_id(&self) -> u32 {
            0
        }
    }

    // Dummy state machine for testing
    #[derive(Debug, Clone)]
    pub struct DummyStateMachine;

    impl StateMachine for DummyStateMachine {
        type Command = ();
        type Event = ();

        fn apply(&mut self, _command: &Self::Command) -> Result<Vec<Self::Event>, Box<dyn std::error::Error>> {
            Ok(vec![])
        }

        fn snapshot(&self) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
            Ok(vec![])
        }

        fn restore(&mut self, _snapshot: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
            Ok(())
        }
    }

    fn create_logger() -> Logger {
        use slog::Drain;
        let decorator = slog_term::PlainDecorator::new(std::io::stdout());
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        Logger::root(drain, slog::o!())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_management_runtime_basic_operations() {
        let logger = create_logger();
        let server = Arc::new(InProcessServer::new());
        let transport = Arc::new(TransportLayer::new(Arc::new(InProcessMessageSender::new(
            server.clone(),
        ))));

        let (tx, rx) = mpsc::channel(100);
        let cluster_router = Arc::new(ClusterRouter::new());

        let config = RaftNodeConfig {
            node_id: 1,
            cluster_id: 0,
            snapshot_interval: 0, // Disable snapshots for this test
            ..Default::default()
        };

        // Register node with server
        let tx_clone = tx.clone();
        server
            .register_node(1, move |msg| {
                tx_clone.try_send(msg).map_err(|e| TransportError::SendFailed {
                    node_id: 1,
                    reason: format!("Failed to send: {}", e),
                })
            })
            .await;

        // Create shared dummy config
        let shared_config = Arc::new(Mutex::new(DummyConfig));

        // Use () as placeholder for sub-cluster type since we're not instantiating any
        let (runtime, node) =
            ManagementRuntime::<()>::new(config, transport, rx, cluster_router, shared_config, logger).unwrap();

        // Campaign to become leader
        node.lock()
            .await
            .campaign()
            .await
            .expect("Campaign should succeed");

        // Run node in background
        let node_clone = node.clone();
        let _node_handle = tokio::spawn(async move {
            use crate::raft::generic2::RaftNode;
            let _ = RaftNode::run_from_arc(node_clone).await;
        });

        // Wait for node to become leader
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        assert!(runtime.is_leader().await, "Node should be leader");

        // Test create sub-cluster (cluster_id assigned automatically)
        let cluster_id = runtime
            .create_sub_cluster(vec![1, 2, 3])
            .await
            .expect("Create sub-cluster should succeed");
        assert_eq!(cluster_id, 1); // First cluster should get ID 1

        // Test get sub-cluster
        let metadata = runtime.get_sub_cluster(&cluster_id).await;
        assert!(metadata.is_some());
        assert_eq!(metadata.as_ref().unwrap().node_ids, vec![1, 2, 3]);

        // Test list sub-clusters
        let clusters = runtime.list_sub_clusters().await;
        assert_eq!(clusters.len(), 1);
        assert!(clusters.contains(&cluster_id));

        // Test add node to sub-cluster
        runtime
            .add_node_to_sub_cluster(cluster_id, 4)
            .await
            .expect("Add node should succeed");

        let metadata = runtime.get_sub_cluster(&cluster_id).await.unwrap();
        assert_eq!(metadata.node_ids, vec![1, 2, 3, 4]);

        // Test set metadata
        runtime
            .set_metadata(cluster_id, "type".to_string(), "kv".to_string())
            .await
            .expect("Set metadata should succeed");

        let metadata = runtime.get_sub_cluster(&cluster_id).await.unwrap();
        assert_eq!(
            metadata.metadata.get("type"),
            Some(&"kv".to_string())
        );

        // Test delete metadata
        runtime
            .delete_metadata(cluster_id, "type".to_string())
            .await
            .expect("Delete metadata should succeed");

        let metadata = runtime.get_sub_cluster(&cluster_id).await.unwrap();
        assert!(metadata.metadata.get("type").is_none());

        // Test remove node from sub-cluster
        runtime
            .remove_node_from_sub_cluster(cluster_id, 4)
            .await
            .expect("Remove node should succeed");

        let metadata = runtime.get_sub_cluster(&cluster_id).await.unwrap();
        assert_eq!(metadata.node_ids, vec![1, 2, 3]);

        // Test delete sub-cluster
        runtime
            .delete_sub_cluster(cluster_id)
            .await
            .expect("Delete sub-cluster should succeed");

        let metadata = runtime.get_sub_cluster(&cluster_id).await;
        assert!(metadata.is_none());

        let clusters = runtime.list_sub_clusters().await;
        assert_eq!(clusters.len(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_management_runtime_with_kv_runtime_type() {
        // This test demonstrates ManagementRuntime parameterized with KvRuntime
        // to track metadata about KV sub-clusters (not actually creating them yet)
        use crate::kv::KvRuntime;

        let logger = create_logger();
        let server = Arc::new(InProcessServer::new());
        let transport = Arc::new(TransportLayer::new(Arc::new(InProcessMessageSender::new(
            server.clone(),
        ))));

        let (tx, rx) = mpsc::channel(100);
        let cluster_router = Arc::new(ClusterRouter::new());

        let config = RaftNodeConfig {
            node_id: 1,
            cluster_id: 0,  // Management cluster
            snapshot_interval: 0,
            ..Default::default()
        };

        // Register node with server
        let tx_clone = tx.clone();
        server
            .register_node(1, move |msg| {
                tx_clone.try_send(msg).map_err(|e| TransportError::SendFailed {
                    node_id: 1,
                    reason: format!("Failed to send: {}", e),
                })
            })
            .await;

        // Create shared KV config
        let shared_config = Arc::new(Mutex::new(crate::kv::runtime::KvConfig));

        // Create ManagementRuntime typed to manage KvRuntime sub-clusters
        let (runtime, node) =
            ManagementRuntime::<KvRuntime>::new(config, transport, rx, cluster_router, shared_config, logger).unwrap();

        // Campaign to become leader
        node.lock()
            .await
            .campaign()
            .await
            .expect("Campaign should succeed");

        // Run node in background
        let node_clone = node.clone();
        let _node_handle = tokio::spawn(async move {
            use crate::raft::generic2::RaftNode;
            let _ = RaftNode::run_from_arc(node_clone).await;
        });

        // Wait for node to become leader
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        assert!(runtime.is_leader().await, "Node should be leader");

        // Create first KV cluster with nodes 1,2,3
        let kv_cluster_1 = runtime
            .create_sub_cluster(vec![1, 2, 3])
            .await
            .expect("Create first KV cluster should succeed");
        assert_eq!(kv_cluster_1, 1); // First cluster should get ID 1

        runtime
            .set_metadata(kv_cluster_1, "type".to_string(), "kv".to_string())
            .await
            .expect("Set metadata should succeed");

        runtime
            .set_metadata(kv_cluster_1, "region".to_string(), "us-west".to_string())
            .await
            .expect("Set metadata should succeed");

        // Create second KV cluster with nodes 4,5,6
        let kv_cluster_2 = runtime
            .create_sub_cluster(vec![4, 5, 6])
            .await
            .expect("Create second KV cluster should succeed");
        assert_eq!(kv_cluster_2, 2); // Second cluster should get ID 2

        runtime
            .set_metadata(kv_cluster_2, "type".to_string(), "kv".to_string())
            .await
            .expect("Set metadata should succeed");

        runtime
            .set_metadata(kv_cluster_2, "region".to_string(), "us-east".to_string())
            .await
            .expect("Set metadata should succeed");

        // Create third KV cluster with nodes 7,8,9
        let kv_cluster_3 = runtime
            .create_sub_cluster(vec![7, 8, 9])
            .await
            .expect("Create third KV cluster should succeed");
        assert_eq!(kv_cluster_3, 3); // Third cluster should get ID 3

        runtime
            .set_metadata(kv_cluster_3, "type".to_string(), "kv".to_string())
            .await
            .expect("Set metadata should succeed");

        runtime
            .set_metadata(kv_cluster_3, "region".to_string(), "eu-west".to_string())
            .await
            .expect("Set metadata should succeed");

        // Verify all clusters are tracked
        let all_clusters = runtime.list_sub_clusters().await;
        assert_eq!(all_clusters.len(), 3);
        assert!(all_clusters.contains(&kv_cluster_1));
        assert!(all_clusters.contains(&kv_cluster_2));
        assert!(all_clusters.contains(&kv_cluster_3));

        // Verify metadata for each cluster
        let meta1 = runtime.get_sub_cluster(&kv_cluster_1).await.unwrap();
        assert_eq!(meta1.node_ids, vec![1, 2, 3]);
        assert_eq!(meta1.metadata.get("type"), Some(&"kv".to_string()));
        assert_eq!(meta1.metadata.get("region"), Some(&"us-west".to_string()));

        let meta2 = runtime.get_sub_cluster(&kv_cluster_2).await.unwrap();
        assert_eq!(meta2.node_ids, vec![4, 5, 6]);
        assert_eq!(meta2.metadata.get("region"), Some(&"us-east".to_string()));

        let meta3 = runtime.get_sub_cluster(&kv_cluster_3).await.unwrap();
        assert_eq!(meta3.node_ids, vec![7, 8, 9]);
        assert_eq!(meta3.metadata.get("region"), Some(&"eu-west".to_string()));

        // Test get_all_sub_clusters
        let all_metadata = runtime.get_all_sub_clusters().await;
        assert_eq!(all_metadata.len(), 3);

        // Delete one cluster and verify
        runtime
            .delete_sub_cluster(kv_cluster_2)
            .await
            .expect("Delete should succeed");

        let remaining_clusters = runtime.list_sub_clusters().await;
        assert_eq!(remaining_clusters.len(), 2);
        assert!(!remaining_clusters.contains(&kv_cluster_2));
    }
}
