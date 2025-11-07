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
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio::time::timeout;

/// Subscription helper for workflow-specific events
pub struct WorkflowSubscription {
    receiver: broadcast::Receiver<WorkflowEvent>,
    target_workflow_id: String,
}

impl WorkflowSubscription {
    pub fn new(receiver: broadcast::Receiver<WorkflowEvent>, target_workflow_id: String) -> Self {
        Self {
            receiver,
            target_workflow_id,
        }
    }

    /// Wait for a specific event for this workflow and extract a typed value from it
    pub async fn wait_for_event<F, T>(&mut self, event_extractor: F, timeout_duration: Option<Duration>) -> Result<T, WorkflowError>
    where
        F: Fn(&WorkflowEvent) -> Option<T> + Send + Sync + Clone,
        T: Send + 'static,
    {
        let wait_future = async {
            loop {
                match self.receiver.recv().await {
                    Ok(event) => {
                        // Check if this event is for our workflow and extract value
                        if self.matches_workflow(&event) {
                            if let Some(value) = event_extractor(&event) {
                                return Ok(value);
                            }
                        }
                        // Continue listening for other events
                    },
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Continue listening after lag
                    },
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(WorkflowError::ClusterError("Event channel closed".to_string()));
                    },
                }
            }
        };

        // Apply timeout if provided, otherwise wait indefinitely
        if let Some(timeout_duration) = timeout_duration {
            timeout(timeout_duration, wait_future)
                .await
                .map_err(|_| WorkflowError::Timeout)?
        } else {
            wait_future.await
        }
    }

    fn matches_workflow(&self, event: &WorkflowEvent) -> bool {
        match event {
            WorkflowEvent::WorkflowStarted { workflow_id, .. } => workflow_id == &self.target_workflow_id,
            WorkflowEvent::WorkflowCompleted { workflow_id, .. } => workflow_id == &self.target_workflow_id,
            WorkflowEvent::WorkflowFailed { workflow_id, .. } => workflow_id == &self.target_workflow_id,
            WorkflowEvent::CheckpointSet { workflow_id, .. } => workflow_id == &self.target_workflow_id,
            WorkflowEvent::OwnershipChanged { workflow_id, .. } => workflow_id == &self.target_workflow_id,
        }
    }
}

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
    /// * `registry` - Shared workflow registry
    /// * `logger` - Logger instance
    ///
    /// # Returns
    /// A tuple of (WorkflowRuntime, RaftNode handle for running event loop)
    pub fn new(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc::server::raft_proto::GenericMessage>,
        registry: Arc<Mutex<WorkflowRegistry>>,
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

        let runtime = Arc::new(Self {
            proposal_router,
            event_bus: event_bus.clone(),
            registry,
            node_id: config.node_id,
            cluster_id: config.cluster_id,
            logger: logger.clone(),
        });

        // Start workflow execution observer
        let runtime_clone = runtime.clone();
        tokio::spawn(async move {
            runtime_clone.run_workflow_observer().await;
        });

        Ok(((*runtime).clone(), node_arc))
    }

    /// Create a new Workflow runtime for a node joining an existing cluster
    pub fn new_joining_node(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc::server::raft_proto::GenericMessage>,
        initial_voters: Vec<u64>,
        registry: Arc<Mutex<WorkflowRegistry>>,
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

        let runtime = Arc::new(Self {
            proposal_router,
            event_bus: event_bus.clone(),
            registry,
            node_id: config.node_id,
            cluster_id: config.cluster_id,
            logger: logger.clone(),
        });

        // Start workflow execution observer
        let runtime_clone = runtime.clone();
        tokio::spawn(async move {
            runtime_clone.run_workflow_observer().await;
        });

        Ok(((*runtime).clone(), node_arc))
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
        Fut: std::future::Future<Output = Result<O, WorkflowError>> + Send + 'static,
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

    /// Get a handle to an existing workflow run by ID
    ///
    /// This creates a TypedWorkflowRun that can be used to wait for completion
    /// of a workflow that was started elsewhere (e.g., on a different node).
    ///
    /// # Arguments
    /// * `workflow_id` - The ID of the workflow to get a handle for
    ///
    /// # Returns
    /// A TypedWorkflowRun handle for waiting on completion
    pub fn get_workflow_run<O>(&self, workflow_id: String) -> TypedWorkflowRun<O>
    where
        O: serde::de::DeserializeOwned,
    {
        let run = WorkflowRun::new(workflow_id, Arc::new(self.clone()));
        TypedWorkflowRun::new(run)
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

    /// Subscribe to workflow events for a specific workflow ID
    pub fn subscribe_to_workflow(&self, workflow_id: &str) -> WorkflowSubscription {
        WorkflowSubscription::new(
            self.event_bus.subscribe(),
            workflow_id.to_string()
        )
    }

    /// Check if this node owns the given workflow
    async fn is_workflow_owner(&self, workflow_id: &str) -> bool {
        // Query state machine for workflow ownership
        let node_arc = self.proposal_router.node();
        let node = node_arc.lock().await;
        let sm = node.state_machine().clone();
        drop(node);

        let owner_node_id = sm.lock().await.get_owner(workflow_id);

        match owner_node_id {
            Some(owner) => owner == self.node_id,
            // If no owner is registered, treat this node as owner
            // This handles edge cases like testing and pre-workflow checkpoints
            None => true,
        }
    }

    /// Wait for a specific event OR an ownership change event
    /// Returns Ok(Some(value)) if the expected event occurred
    /// Returns Ok(None) if an OwnershipChanged event occurred (caller should re-check ownership)
    /// Returns Err if timeout or other error
    async fn wait_for_event_or_ownership_change<E, T>(
        workflow_id: &str,
        mut subscription: WorkflowSubscription,
        event_extractor: E,
        timeout_duration: Option<Duration>
    ) -> Result<Option<T>, WorkflowError>
    where
        E: Fn(&WorkflowEvent) -> Option<T> + Send + Sync + Clone,
        T: Send + 'static,
    {
        let workflow_id = workflow_id.to_string();
        let filtered_extractor = move |event: &WorkflowEvent| {
            // First check if this event is for our workflow_id
            let event_workflow_id = match event {
                WorkflowEvent::WorkflowStarted { workflow_id, .. } => workflow_id,
                WorkflowEvent::WorkflowCompleted { workflow_id, .. } => workflow_id,
                WorkflowEvent::WorkflowFailed { workflow_id, .. } => workflow_id,
                WorkflowEvent::CheckpointSet { workflow_id, .. } => workflow_id,
                WorkflowEvent::OwnershipChanged { workflow_id, .. } => workflow_id,
            };

            if event_workflow_id != &workflow_id {
                return None; // Not for our workflow
            }

            // Check if this is an ownership change event
            if matches!(event, WorkflowEvent::OwnershipChanged { .. }) {
                return Some(None);
            }

            // Apply the user's event extractor and wrap in Some
            event_extractor(event).map(Some)
        };

        match subscription.wait_for_event(filtered_extractor, timeout_duration).await {
            Ok(Some(value)) => Ok(Some(value)), // Got expected event
            Ok(None) => Ok(None), // Got OwnershipChanged
            Err(e) => Err(e), // Timeout or other error
        }
    }

    /// Execute an operation based on workflow ownership with retry logic for ownership changes
    ///
    /// If this node owns the workflow, execute the operation immediately.
    /// If not, wait for the owner to execute and propagate the result via events.
    /// Handles ownership changes (e.g., due to failover) by re-checking ownership on OwnershipChanged events.
    async fn execute_owner_or_wait<F, Fut, E, T>(
        &self,
        workflow_id: &str,
        owner_operation: F,
        wait_for_event_fn: E,
        passive_timeout: Option<Duration>,
    ) -> Result<T, WorkflowError>
    where
        F: Fn() -> Fut + Send,
        Fut: std::future::Future<Output = Result<T, WorkflowError>> + Send,
        E: Fn(&WorkflowEvent) -> Option<T> + Send + Sync + Clone,
        T: Send + 'static,
    {
        // Retry loop to handle ownership changes (e.g., due to failover)
        let max_retries = 10; // Increased to handle multiple ownership changes
        for attempt in 0..max_retries {
            // Subscribe first to avoid race conditions
            let subscription = self.subscribe_to_workflow(workflow_id);

            // Check if we're the owner of this workflow
            // Note: Ownership is independent of Raft leadership
            let is_owner = self.is_workflow_owner(workflow_id).await;

            if is_owner {
                // Active owner: Execute the operation
                // This happens if we own this workflow (either from the start or due to failover promotion)
                return owner_operation().await;
            } else {
                // Passive non-owner: Wait for the owner to execute and propagate results
                // Use a shorter timeout to allow retrying if we become owner
                let timeout_duration = if attempt < max_retries - 1 {
                    Some(Duration::from_secs(5)) // Shorter timeout for retry attempts
                } else {
                    passive_timeout // Use provided timeout for final attempt (could be None for indefinite wait)
                };

                // Wait for either:
                // 1. The expected event (e.g., CheckpointSet from the owner)
                // 2. OwnershipChanged event (which means we should re-check if we're now the owner)
                match WorkflowRuntime::wait_for_event_or_ownership_change(
                    workflow_id,
                    subscription,
                    wait_for_event_fn.clone(),
                    timeout_duration
                ).await {
                    Ok(Some(value)) => return Ok(value), // Got the expected event
                    Ok(None) => continue, // Got OwnershipChanged - retry to check if we're now owner
                    Err(WorkflowError::Timeout) => {
                        // Timeout - check if we became owner and should retry
                        if self.is_workflow_owner(workflow_id).await {
                            continue; // Retry as owner
                        } else if attempt == max_retries - 1 {
                            return Err(WorkflowError::Timeout); // Final attempt failed
                        }
                        // Continue to next attempt
                    },
                    Err(e) => return Err(e), // Other errors are not retryable
                }
            }
        }

        Err(WorkflowError::Timeout) // Should not reach here, but just in case
    }

    /// Get workflow input (reads from local state machine)
    async fn get_input(&self, workflow_id: &str) -> Option<Vec<u8>> {
        let node_arc = self.proposal_router.node();
        let node = node_arc.lock().await;
        let sm = node.state_machine().clone();
        drop(node);

        sm.lock().await.get_input(workflow_id).cloned()
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

    /// Try to dequeue a checkpoint from the transient queue
    ///
    /// This is used by ALL nodes (owner and followers) to check if a checkpoint value
    /// has already been enqueued by the state machine before waiting for events.
    /// Uses simple FIFO queue semantics: pop once from front.
    /// Deterministic execution guarantees the queue order matches execution order.
    async fn try_dequeue_checkpoint<T>(
        &self,
        workflow_id: &str,
        key: &str,
    ) -> Result<Option<T>, WorkflowError>
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de> + Clone + Send + Sync + 'static,
    {
        let node_arc = self.proposal_router.node();
        let node = node_arc.lock().await;
        let sm = node.state_machine().clone();
        drop(node);

        let mut sm_guard = sm.lock().await;
        if let Some(serialized_value) = sm_guard.pop_queued_checkpoint(workflow_id, key) {
            let deserialized = serde_json::from_slice::<T>(&serialized_value)
                .map_err(|e| WorkflowError::SerializationError(format!("Deserialization error: {}", e)))?;
            return Ok(Some(deserialized));
        }

        Ok(None)
    }

    /// Set a replicated variable (checkpoint)
    ///
    /// Always checks the transient queue first (for late followers and faster owners).
    /// If queue is empty, uses the owner-or-wait pattern: owner proposes SetCheckpoint,
    /// followers wait for the CheckpointSet event.
    pub async fn set_checkpoint<T>(
        &self,
        workflow_id: &str,
        key: &str,
        value: T,
    ) -> Result<T, WorkflowError>
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de> + Clone + Send + Sync + 'static,
    {
        // Always check the transient queue first (for late followers)
        // This handles the case where the owner has already enqueued the checkpoint
        // before the follower reaches this point in execution
        if let Some(queued_value) = self.try_dequeue_checkpoint(workflow_id, key).await? {
            return Ok(queued_value);
        }

        // No queued value - proceed with normal consensus-based flow
        // Serialize value once (used by both owner and non-owner paths)
        let value_bytes = serde_json::to_vec(&value)
            .map_err(|e| WorkflowError::SerializationError(e.to_string()))?;

        // Clone values for use in closures
        let value_bytes_clone = value_bytes.clone();
        let proposal_router = self.proposal_router.clone();
        let workflow_id_for_owner = workflow_id.to_string();
        let key_for_owner = key.to_string();
        let workflow_id_for_wait = workflow_id.to_string();
        let key_for_wait = key.to_string();

        // Use owner-or-wait pattern
        self.execute_owner_or_wait(
            workflow_id,
            || {
                let workflow_id_clone = workflow_id_for_owner.clone();
                let key_clone = key_for_owner.clone();
                let value_bytes_clone2 = value_bytes_clone.clone();
                let value_clone = value.clone();
                let proposal_router_clone = proposal_router.clone();

                async move {
                    // Owner operation: Propose SetCheckpoint command
                    let command = WorkflowCommand::SetCheckpoint {
                        workflow_id: workflow_id_clone.clone(),
                        key: key_clone.clone(),
                        value: value_bytes_clone2,
                    };

                    proposal_router_clone
                        .propose_and_wait(command)
                        .await
                        .map_err(|e| WorkflowError::ClusterError(format!("{}", e)))?;

                    // Pop the value we just enqueued (during apply in propose_and_wait)
                    // This prevents owner's own execution from consuming its proposed values
                    let node_arc = proposal_router_clone.node();
                    let node = node_arc.lock().await;
                    let sm = node.state_machine().clone();
                    drop(node);

                    let mut sm_guard = sm.lock().await;
                    sm_guard.pop_queued_checkpoint(&workflow_id_clone, &key_clone);

                    Ok(value_clone)
                }
            },
            move |event| {
                // Wait for CheckpointSet event with matching workflow_id and key
                if let WorkflowEvent::CheckpointSet { workflow_id: event_wf_id, key: event_key, value: event_value } = event {
                    if event_wf_id == &workflow_id_for_wait && event_key == &key_for_wait {
                        // Deserialize the value from the event
                        match serde_json::from_slice::<T>(event_value) {
                            Ok(deserialized_value) => Some(deserialized_value),
                            Err(_) => None, // Deserialization failed, continue waiting
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            },
            None, // No timeout (wait indefinitely)
        ).await
    }

    /// Run workflow execution observer (background task)
    ///
    /// This observes WorkflowStarted events and spawns workflow execution tasks.
    /// ALL nodes execute workflows in parallel for fault tolerance.
    async fn run_workflow_observer(self: Arc<Self>) {
        use slog::info;

        let mut event_rx = self.event_bus.subscribe();

        info!(self.logger, "Workflow observer started");

        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    match event {
                        WorkflowEvent::WorkflowStarted {
                            workflow_id,
                            workflow_type,
                            version,
                            owner_node_id,
                        } => {
                            // ALL nodes execute workflows (consensus-driven parallel execution)
                            // Only the owner will propose WorkflowEnd; others wait for it
                            let is_owner = owner_node_id == self.node_id;
                            info!(self.logger, "Starting workflow execution";
                                "workflow_id" => &workflow_id,
                                "workflow_type" => &workflow_type,
                                "version" => version,
                                "is_owner" => is_owner
                            );

                            let runtime_clone = self.clone();
                            tokio::spawn(async move {
                                runtime_clone
                                    .execute_workflow(workflow_id, workflow_type, version)
                                    .await;
                            });
                        }
                        _ => {
                            // Ignore other events
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Continue on lag
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    info!(self.logger, "Workflow observer stopped (event channel closed)");
                    break;
                }
            }
        }
    }

    /// Execute a workflow (called when WorkflowStarted event is observed)
    async fn execute_workflow(
        self: Arc<Self>,
        workflow_id: String,
        workflow_type: String,
        version: u32,
    ) {
        use slog::{error, info};

        // Look up the workflow function
        let registry = self.registry.lock().await;
        let workflow_fn = match registry.get(&workflow_type, version) {
            Some(func) => func,
            None => {
                error!(self.logger, "Workflow function not found";
                    "workflow_id" => &workflow_id,
                    "workflow_type" => &workflow_type,
                    "version" => version
                );
                return;
            }
        };
        drop(registry);

        // Get workflow input from state machine
        let input_bytes = match self.get_input(&workflow_id).await {
            Some(bytes) => bytes,
            None => {
                error!(self.logger, "Workflow input not found in state machine";
                    "workflow_id" => &workflow_id
                );
                return;
            }
        };
        let input_any: Box<dyn std::any::Any + Send> = Box::new(input_bytes);

        // Create workflow context
        let context = WorkflowContext::new(workflow_id.clone(), self.clone());

        // Execute the workflow
        info!(self.logger, "Executing workflow"; "workflow_id" => &workflow_id);

        let result = workflow_fn.execute(input_any, context).await;

        // Use owner/wait pattern for WorkflowEnd
        // Owner proposes WorkflowEnd, non-owners wait for WorkflowCompleted/WorkflowFailed event
        let proposal_router = self.proposal_router.clone();
        let logger_clone = self.logger.clone();

        let result_bytes = match result {
            Ok(result_bytes) => {
                info!(self.logger, "Workflow completed successfully"; "workflow_id" => &workflow_id);
                result_bytes
            }
            Err(e) => {
                error!(self.logger, "Workflow execution failed";
                    "workflow_id" => &workflow_id,
                    "error" => format!("{:?}", e)
                );
                // Serialize the error
                serde_json::to_vec(&e).unwrap_or_else(|_| {
                    serde_json::to_vec(&WorkflowError::ClusterError(format!("{:?}", e)))
                        .unwrap_or_default()
                })
            }
        };

        let result_bytes_clone = result_bytes.clone();
        let workflow_id_for_owner = workflow_id.clone();
        let workflow_id_for_wait = workflow_id.clone();

        match self.execute_owner_or_wait(
            &workflow_id,
            || {
                let workflow_id_owned = workflow_id_for_owner.clone();
                let result_bytes_owned = result_bytes_clone.clone();
                let proposal_router_clone = proposal_router.clone();
                let logger_owned = logger_clone.clone();

                async move {
                    // Owner operation: Propose WorkflowEnd command
                    let command = WorkflowCommand::WorkflowEnd {
                        workflow_id: workflow_id_owned.clone(),
                        result: result_bytes_owned,
                    };

                    if let Err(e) = proposal_router_clone.propose_and_wait(command).await {
                        error!(logger_owned, "Failed to propose WorkflowEnd";
                            "workflow_id" => &workflow_id_owned,
                            "error" => format!("{}", e)
                        );
                        return Err(WorkflowError::ClusterError(format!("Failed to propose WorkflowEnd: {}", e)));
                    }

                    Ok(())
                }
            },
            move |event| {
                // Wait for WorkflowCompleted or WorkflowFailed event
                match event {
                    WorkflowEvent::WorkflowCompleted { workflow_id: event_wf_id, .. } |
                    WorkflowEvent::WorkflowFailed { workflow_id: event_wf_id, .. } => {
                        if event_wf_id == &workflow_id_for_wait {
                            Some(())
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            },
            None, // No timeout (wait indefinitely for completion)
        ).await {
            Ok(_) => {
                // Workflow end successfully recorded
            }
            Err(e) => {
                error!(self.logger, "Failed to record workflow end";
                    "workflow_id" => &workflow_id,
                    "error" => format!("{}", e)
                );
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::generic2::{InProcessNetwork, InProcessNetworkSender, TransportLayer, RaftNodeConfig};
    use crate::workflow2::ReplicatedVar;
    use serde::{Deserialize, Serialize};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn create_logger() -> slog::Logger {
        use slog::Drain;
        let decorator = slog_term::PlainDecorator::new(std::io::stdout());
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        slog::Logger::root(drain, slog::o!())
    }

    #[derive(Serialize, Deserialize, Clone, Debug)]
    struct FibonacciInput {
        n: u32,
    }

    #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
    struct FibonacciOutput {
        result: u64,
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_workflow_execution_end_to_end() {
        let logger = create_logger();
        let network = Arc::new(InProcessNetwork::new());
        let transport = Arc::new(TransportLayer::new(Arc::new(InProcessNetworkSender::new(
            network.clone(),
        ))));

        let (tx, rx) = mpsc::channel(100);

        // Register node with network
        network.register_node(1, tx.clone()).await;

        let config = RaftNodeConfig {
            node_id: 1,
            cluster_id: 1,
            snapshot_interval: 0,
            ..Default::default()
        };

        // Create shared registry
        let registry = Arc::new(Mutex::new(WorkflowRegistry::new()));

        let (runtime, node) = WorkflowRuntime::new(config, transport, rx, registry.clone(), logger.clone()).unwrap();
        let runtime = Arc::new(runtime);

        // Campaign to become leader
        node.lock().await.campaign().await.expect("Campaign should succeed");

        // Run node in background
        let node_clone = node.clone();
        tokio::spawn(async move {
            use crate::raft::generic2::RaftNode;
            let _ = RaftNode::run_from_arc(node_clone).await;
        });

        // Wait for leadership
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        // Register a Fibonacci workflow that uses checkpoints
        runtime
            .register_workflow_closure(
                "fibonacci",
                1,
                |input: FibonacciInput, ctx: WorkflowContext| async move {
                    if input.n == 0 {
                        return Ok(FibonacciOutput { result: 0 });
                    }
                    if input.n == 1 {
                        return Ok(FibonacciOutput { result: 1 });
                    }

                    // Use replicated variables for state
                    let mut a = ReplicatedVar::with_value("a", &ctx, 0u64).await?;
                    let mut b = ReplicatedVar::with_value("b", &ctx, 1u64).await?;

                    for _ in 2..=input.n {
                        let next = a.get() + b.get();
                        a.set(b.get()).await?;
                        b.set(next).await?;
                    }

                    Ok(FibonacciOutput {
                        result: b.get(),
                    })
                },
            )
            .await
            .expect("Workflow registration should succeed");

        // Start workflow
        let workflow = runtime
            .start_workflow::<FibonacciInput, FibonacciOutput>(
                "fib-test-1".to_string(),
                "fibonacci".to_string(),
                1,
                FibonacciInput { n: 10 },
            )
            .await
            .expect("Workflow start should succeed");

        // Wait for completion
        let result = workflow
            .wait_for_completion()
            .await
            .expect("Workflow should complete successfully");

        // Verify result (10th Fibonacci number is 55)
        assert_eq!(result.result, 55);

        // Verify workflow status is Completed
        let status = runtime.get_workflow_status("fib-test-1").await;
        assert_eq!(status, Some(WorkflowStatus::Completed));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_workflow_not_found() {
        let logger = create_logger();
        let network = Arc::new(InProcessNetwork::new());
        let transport = Arc::new(TransportLayer::new(Arc::new(InProcessNetworkSender::new(
            network.clone(),
        ))));

        let (tx, rx) = mpsc::channel(100);

        // Register node with network
        network.register_node(1, tx.clone()).await;

        let config = RaftNodeConfig {
            node_id: 1,
            cluster_id: 1,
            snapshot_interval: 0,
            ..Default::default()
        };

        // Create shared registry
        let registry = Arc::new(Mutex::new(WorkflowRegistry::new()));

        let (runtime, node) = WorkflowRuntime::new(config, transport, rx, registry.clone(), logger.clone()).unwrap();
        let runtime = Arc::new(runtime);

        // Campaign to become leader
        node.lock().await.campaign().await.expect("Campaign should succeed");

        // Run node in background
        let node_clone = node.clone();
        tokio::spawn(async move {
            use crate::raft::generic2::RaftNode;
            let _ = RaftNode::run_from_arc(node_clone).await;
        });

        // Wait for leadership
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        // Try to start a workflow that was never registered
        let workflow = runtime
            .start_workflow::<FibonacciInput, FibonacciOutput>(
                "missing-wf-1".to_string(),
                "nonexistent".to_string(),
                1,
                FibonacciInput { n: 5 },
            )
            .await
            .expect("Workflow start should succeed (even if function not found)");

        // The workflow will start but execution will fail silently because the function isn't registered
        // This is a limitation of the current design - we should improve error handling here
        // For now, just verify the workflow was marked as started
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        let status = runtime.get_workflow_status("missing-wf-1").await;
        assert_eq!(status, Some(WorkflowStatus::Running));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_two_node_owner_wait_pattern() {
        use slog::info;

        let logger = create_logger();
        info!(logger, "Starting two-node owner/wait pattern test");

        // Create shared network infrastructure
        let network = Arc::new(InProcessNetwork::new());

        // Create shared registry for all nodes
        let registry = Arc::new(Mutex::new(WorkflowRegistry::new()));

        // === Setup Node 1 (Leader/Owner) ===
        let (tx1, rx1) = mpsc::channel(100);

        // Register node 1's mailbox with the network
        network.register_node(1, tx1.clone()).await;

        let transport1 = Arc::new(TransportLayer::new(Arc::new(InProcessNetworkSender::new(
            network.clone(),
        ))));

        let config1 = RaftNodeConfig {
            node_id: 1,
            cluster_id: 1,
            snapshot_interval: 0,
            ..Default::default()
        };

        let (runtime1, node1) = WorkflowRuntime::new(config1, transport1.clone(), rx1, registry.clone(), logger.clone()).unwrap();
        let runtime1 = Arc::new(runtime1);

        // Campaign node 1 to become leader
        node1.lock().await.campaign().await.expect("Node 1 campaign should succeed");

        // Run node 1 in background
        let node1_clone = node1.clone();
        tokio::spawn(async move {
            use crate::raft::generic2::RaftNode;
            let _ = RaftNode::run_from_arc(node1_clone).await;
        });

        // Wait for node 1 to become leader
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        info!(logger, "Node 1 is now leader");

        // === Setup Node 2 (Follower) ===
        let (tx2, rx2) = mpsc::channel(100);

        // Register node 2's mailbox with the network
        network.register_node(2, tx2.clone()).await;

        let transport2 = Arc::new(TransportLayer::new(Arc::new(InProcessNetworkSender::new(
            network.clone(),
        ))));

        let config2 = RaftNodeConfig {
            node_id: 2,
            cluster_id: 1,
            snapshot_interval: 0,
            ..Default::default()
        };

        let (runtime2, node2) = WorkflowRuntime::new_joining_node(
            config2,
            transport2.clone(),
            rx2,
            vec![],  // Empty initial voters - will be populated via snapshot from node 1
            registry.clone(),
            logger.clone(),
        ).unwrap();
        let runtime2 = Arc::new(runtime2);

        // Add peers to both transports for bidirectional communication
        transport1.add_peer(2, "node2".to_string()).await;
        transport2.add_peer(1, "node1".to_string()).await;

        // Run node 2 in background
        let node2_clone = node2.clone();
        tokio::spawn(async move {
            use crate::raft::generic2::RaftNode;
            let _ = RaftNode::run_from_arc(node2_clone).await;
        });

        // Give node 2 a moment to start up
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Add node 2 to the cluster
        let rx = runtime1.add_node(2, "node2".to_string()).await
            .expect("Add node should succeed");
        rx.await.expect("Add node receiver should work")
            .expect("Add node should complete");

        info!(logger, "Node 2 added to cluster");

        // Wait for cluster to stabilize
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // Register workflow once (both nodes share the same registry)
        runtime1
            .register_workflow_closure(
                "checkpoint_test",
                1,
                |input: FibonacciInput, ctx: WorkflowContext| async move {
                    // Create multiple checkpoints to test owner/wait pattern
                    let mut counter = ReplicatedVar::with_value("counter", &ctx, 0u64).await?;

                    for i in 1..=input.n {
                        counter.set(i as u64).await?;
                    }

                    Ok(FibonacciOutput {
                        result: counter.get(),
                    })
                },
            )
            .await
            .expect("Workflow registration should succeed");

        info!(logger, "Workflow registered (shared registry)");

        // Start workflow on node 1 (node 1 will be the owner since it's starting it)
        let workflow1 = runtime1
            .start_workflow::<FibonacciInput, FibonacciOutput>(
                "two-node-test-1".to_string(),
                "checkpoint_test".to_string(),
                1,
                FibonacciInput { n: 5 },
            )
            .await
            .expect("Workflow start should succeed");

        info!(logger, "Workflow started on node 1");

        // Both nodes should be able to wait for completion
        let result1_future = workflow1.wait_for_completion();

        // Node 2 can also wait for the same workflow
        let workflow2 = runtime2
            .get_workflow_run::<FibonacciOutput>("two-node-test-1".to_string());
        let result2_future = workflow2.wait_for_completion();

        // Wait for both to complete
        let (result1, result2): (Result<FibonacciOutput, WorkflowError>, Result<FibonacciOutput, WorkflowError>) =
            tokio::join!(result1_future, result2_future);

        let result1 = result1.expect("Node 1 should get result");
        let result2 = result2.expect("Node 2 should get result");

        info!(logger, "Both nodes received workflow completion"; "result1" => result1.result, "result2" => result2.result);

        // Verify both nodes got the same result
        assert_eq!(result1.result, 5);
        assert_eq!(result2.result, 5);
        assert_eq!(result1, result2);

        // Verify workflow status on both nodes
        let status1 = runtime1.get_workflow_status("two-node-test-1").await;
        let status2 = runtime2.get_workflow_status("two-node-test-1").await;

        assert_eq!(status1, Some(WorkflowStatus::Completed));
        assert_eq!(status2, Some(WorkflowStatus::Completed));

        info!(logger, "Two-node owner/wait pattern test completed successfully");
    }
}

// Implement SubClusterRuntime trait for WorkflowRuntime
impl crate::management::SubClusterRuntime for WorkflowRuntime {
    type StateMachine = WorkflowStateMachine;
    type Registry = WorkflowRegistry;

    fn new_single_node(
        config: RaftNodeConfig,
        transport: Arc<dyn crate::raft::generic2::Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc::server::raft_proto::GenericMessage>,
        registry: Arc<Mutex<Self::Registry>>,
        logger: slog::Logger,
    ) -> Result<
        (Self, Arc<Mutex<RaftNode<Self::StateMachine>>>),
        Box<dyn std::error::Error>,
    > {
        WorkflowRuntime::new(config, transport, mailbox_rx, registry, logger)
    }

    fn new_joining_node(
        config: RaftNodeConfig,
        transport: Arc<dyn crate::raft::generic2::Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc::server::raft_proto::GenericMessage>,
        initial_voters: Vec<u64>,
        registry: Arc<Mutex<Self::Registry>>,
        logger: slog::Logger,
    ) -> Result<
        (Self, Arc<Mutex<RaftNode<Self::StateMachine>>>),
        Box<dyn std::error::Error>,
    > {
        WorkflowRuntime::new_joining_node(config, transport, mailbox_rx, initial_voters, registry, logger)
    }

    async fn add_node(
        &self,
        node_id: u64,
        address: String,
    ) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        self.add_node(node_id, address).await
    }

    async fn remove_node(
        &self,
        node_id: u64,
    ) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        self.remove_node(node_id).await
    }

    fn node_id(&self) -> u64 {
        self.node_id()
    }

    fn cluster_id(&self) -> u32 {
        self.cluster_id()
    }
}
