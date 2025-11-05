//! Workflow Execution Runtime (Layer 7)
//!
//! Provides a high-level application interface for distributed workflow execution
//! built on the generic2 Raft infrastructure.

use crate::raft::generic2::{EventBus, ProposalRouter, RaftNode, RaftNodeConfig, Transport};
use crate::workflow2::{
    WorkflowCommand, WorkflowContext, WorkflowError, WorkflowEvent, WorkflowRegistry,
    WorkflowRun, WorkflowStateMachine, TypedWorkflowRun,
};
use crate::workflow2::error::WorkflowStatus;
use slog::{info, Logger};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};

/// High-level runtime for distributed workflow execution
///
/// This runtime provides a simple API for:
/// - Registering workflow functions
/// - Starting workflow executions
/// - Querying workflow status and results
/// - Managing node lifecycle
#[derive(Clone)]
pub struct WorkflowRuntime {
    /// Proposal router for submitting operations
    proposal_router: Arc<ProposalRouter<WorkflowStateMachine>>,

    /// Event bus for workflow event notifications
    event_bus: Arc<EventBus<WorkflowEvent>>,

    /// Workflow registry (shared across runtime)
    registry: Arc<Mutex<WorkflowRegistry>>,

    /// Node ID
    node_id: u64,

    /// Cluster ID
    cluster_id: u32,

    /// Logger
    logger: Logger,
}

impl WorkflowRuntime {
    /// Create a new Workflow runtime
    ///
    /// # Arguments
    /// * `config` - Raft node configuration
    /// * `transport` - Transport layer for network communication
    /// * `mailbox_rx` - Mailbox receiver for Raft messages
    /// * `logger` - Logger instance
    ///
    /// # Returns
    /// A tuple of (WorkflowRuntime, RaftNode handle for running event loop)
    pub fn new(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc::server::raft_proto::GenericMessage>,
        logger: Logger,
    ) -> Result<
        (Self, Arc<Mutex<RaftNode<WorkflowStateMachine>>>),
        Box<dyn std::error::Error>,
    > {
        let state_machine = WorkflowStateMachine::new();
        let event_bus = Arc::new(EventBus::new(100));

        info!(logger, "Creating Workflow runtime";
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
            transport,
            config.cluster_id,
            config.node_id,
            logger.clone(),
        ));

        // Start leader tracker in background
        let router_clone = proposal_router.clone();
        tokio::spawn(async move {
            router_clone.run_leader_tracker().await;
        });

        let runtime = Self {
            proposal_router,
            event_bus,
            registry: Arc::new(Mutex::new(WorkflowRegistry::new())),
            node_id: config.node_id,
            cluster_id: config.cluster_id,
            logger,
        };

        Ok((runtime, node_arc))
    }

    /// Create a new Workflow runtime for a node joining an existing cluster
    pub fn new_joining_node(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc::server::raft_proto::GenericMessage>,
        initial_voters: Vec<u64>,
        logger: Logger,
    ) -> Result<
        (Self, Arc<Mutex<RaftNode<WorkflowStateMachine>>>),
        Box<dyn std::error::Error>,
    > {
        let state_machine = WorkflowStateMachine::new();
        let event_bus = Arc::new(EventBus::new(100));

        info!(logger, "Creating Workflow runtime (joining existing cluster)";
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
            transport,
            config.cluster_id,
            config.node_id,
            logger.clone(),
        ));

        // Start leader tracker in background
        let router_clone = proposal_router.clone();
        tokio::spawn(async move {
            router_clone.run_leader_tracker().await;
        });

        let runtime = Self {
            proposal_router,
            event_bus,
            registry: Arc::new(Mutex::new(WorkflowRegistry::new())),
            node_id: config.node_id,
            cluster_id: config.cluster_id,
            logger,
        };

        Ok((runtime, node_arc))
    }

    /// Register a workflow function using a closure
    ///
    /// # Arguments
    /// * `workflow_type` - Unique identifier for the workflow type
    /// * `version` - Version number for this workflow implementation
    /// * `function` - A closure that takes (input, context) and returns a future
    ///
    /// # Returns
    /// * `Ok(())` if registration was successful
    /// * `Err(WorkflowError)` if a workflow with the same type and version already exists
    pub async fn register_workflow_closure<I, O, F, Fut>(
        &self,
        workflow_type: &str,
        version: u32,
        function: F,
    ) -> Result<(), WorkflowError>
    where
        I: Send + Sync + for<'de> serde::Deserialize<'de> + 'static,
        O: Send + Sync + serde::Serialize + 'static,
        F: Fn(I, WorkflowContext) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<O, WorkflowError>> + Send + Sync + 'static,
    {
        info!(self.logger, "Registering workflow";
            "workflow_type" => workflow_type,
            "version" => version
        );

        let mut registry = self.registry.lock().await;
        registry.register_closure(workflow_type, version, function)
    }

    /// Start a workflow execution
    ///
    /// # Arguments
    /// * `workflow_id` - Unique ID for this workflow instance
    /// * `workflow_type` - The type of workflow to execute
    /// * `version` - The version of the workflow to execute
    /// * `input` - Input data for the workflow
    ///
    /// # Returns
    /// A TypedWorkflowRun handle for waiting on completion
    pub async fn start_workflow<I, O>(
        &self,
        workflow_id: String,
        workflow_type: String,
        version: u32,
        input: I,
    ) -> Result<TypedWorkflowRun<O>, WorkflowError>
    where
        I: serde::Serialize,
        O: serde::de::DeserializeOwned,
    {
        info!(self.logger, "Starting workflow";
            "workflow_id" => &workflow_id,
            "workflow_type" => &workflow_type,
            "version" => version
        );

        // Serialize input
        let input_bytes = serde_json::to_vec(&input)
            .map_err(|e| WorkflowError::SerializationError(e.to_string()))?;

        // Create WorkflowStart command
        let command = WorkflowCommand::WorkflowStart {
            workflow_id: workflow_id.clone(),
            workflow_type,
            version,
            input: input_bytes,
            owner_node_id: self.node_id,
        };

        // Propose through Raft
        self.proposal_router
            .propose_and_wait(command)
            .await
            .map_err(|e| WorkflowError::ClusterError(format!("{}", e)))?;

        // Return typed workflow run handle
        let run = WorkflowRun::new(workflow_id, Arc::new(self.clone()));
        Ok(TypedWorkflowRun::new(run))
    }

    /// Get workflow status (reads from local state machine)
    pub async fn get_workflow_status(&self, workflow_id: &str) -> Option<WorkflowStatus> {
        let node_arc = self.proposal_router.node();
        let node = node_arc.lock().await;
        let sm = node.state_machine().clone();
        drop(node);

        sm.lock()
            .await
            .get_workflow_status(workflow_id)
            .cloned()
    }

    /// Get workflow result (reads from local state machine)
    pub async fn get_result(&self, workflow_id: &str) -> Option<Vec<u8>> {
        let node_arc = self.proposal_router.node();
        let node = node_arc.lock().await;
        let sm = node.state_machine().clone();
        drop(node);

        sm.lock().await.get_result(workflow_id).cloned()
    }

    /// Subscribe to workflow events
    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<WorkflowEvent> {
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

    /// Set a replicated variable (checkpoint)
    ///
    /// This proposes a SetCheckpoint command through Raft.
    /// TODO: Add late follower catch-up queue checking in workflow execution logic
    pub async fn set_checkpoint<T>(
        &self,
        workflow_id: &str,
        key: &str,
        value: T,
    ) -> Result<T, WorkflowError>
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de> + Clone,
    {
        // Serialize value
        let value_bytes = serde_json::to_vec(&value)
            .map_err(|e| WorkflowError::SerializationError(e.to_string()))?;

        // Propose through Raft
        let command = WorkflowCommand::SetCheckpoint {
            workflow_id: workflow_id.to_string(),
            key: key.to_string(),
            value: value_bytes,
        };

        self.proposal_router
            .propose_and_wait(command)
            .await
            .map_err(|e| WorkflowError::ClusterError(format!("{}", e)))?;

        Ok(value)
    }

    /// Add a node to the workflow cluster
    pub async fn add_node(
        &self,
        node_id: u64,
        address: String,
    ) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        info!(self.logger, "Adding node to workflow cluster"; "node_id" => node_id, "address" => &address);
        self.proposal_router.add_node(node_id, address).await
    }

    /// Remove a node from the workflow cluster
    pub async fn remove_node(
        &self,
        node_id: u64,
    ) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        info!(self.logger, "Removing node from workflow cluster"; "node_id" => node_id);
        self.proposal_router.remove_node(node_id).await
    }
}
