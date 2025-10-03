use crate::raft::generic::RaftCluster;
use crate::raft::generic::message::CommandExecutor;
use crate::workflow::registry::WorkflowRegistry;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::timeout;

/// Data structure for starting a workflow with type and version information
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowStartData {
    pub workflow_id: String,
    pub workflow_type: String,
    pub version: u32,
    pub input: Vec<u8>,
}

/// Data structure for ending a workflow with serialized result
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowEndData {
    pub workflow_id: String,
    pub result: Vec<u8>,  // Serialized Result<T, E>
}

/// Data structure for checkpoint operations
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckpointData {
    pub workflow_id: String,
    pub key: String,
    pub value: Vec<u8>,
}

/// Commands for workflow lifecycle management
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WorkflowCommand {
    /// Start a new workflow with input data
    WorkflowStart(WorkflowStartData),
    /// End a workflow with output data
    WorkflowEnd(WorkflowEndData),
    /// Set a replicated checkpoint for a workflow
    SetCheckpoint(CheckpointData),
}


/// Command executor for workflow commands with embedded state
pub struct WorkflowCommandExecutor {
    /// Internal state protected by mutex
    state: Arc<Mutex<WorkflowState>>,
    /// Event broadcaster for workflow state changes
    event_tx: broadcast::Sender<WorkflowEvent>,
    /// Reference to workflow registry for spawning executions
    registry: Arc<Mutex<WorkflowRegistry>>,
    /// Reference to workflow runtime (set after construction)
    runtime: Arc<Mutex<Option<Arc<WorkflowRuntime>>>>,
}

impl Default for WorkflowCommandExecutor {
    fn default() -> Self {
        let (event_tx, _) = broadcast::channel(1000);
        Self {
            state: Arc::new(Mutex::new(WorkflowState::default())),
            event_tx,
            registry: Arc::new(Mutex::new(WorkflowRegistry::new())),
            runtime: Arc::new(Mutex::new(None)),
        }
    }
}

impl WorkflowCommandExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the workflow runtime reference (called after runtime construction)
    pub fn set_runtime(&self, runtime: Arc<WorkflowRuntime>) {
        let mut rt = self.runtime.lock().unwrap();
        *rt = Some(runtime);
    }

    /// Get the workflow registry for registration
    pub fn registry(&self) -> Arc<Mutex<WorkflowRegistry>> {
        self.registry.clone()
    }

    fn emit_started_event(&self, workflow_id: &str) {
        let event = WorkflowEvent::Started {
            workflow_id: workflow_id.to_string(),
        };
        let _ = self.event_tx.send(event);
    }

    fn emit_completed_event(&self, workflow_id: &str, result: Vec<u8>) {
        let mut state = self.state.lock().unwrap();
        state.workflows.insert(workflow_id.to_string(), WorkflowStatus::Completed);
        drop(state);

        let event = WorkflowEvent::Completed {
            workflow_id: workflow_id.to_string(),
            result,
        };
        let _ = self.event_tx.send(event);
    }

    fn emit_checkpoint_event(&self, workflow_id: &str, key: &str, value: Vec<u8>) {
        let event = WorkflowEvent::CheckpointSet {
            workflow_id: workflow_id.to_string(),
            key: key.to_string(),
            value,
        };
        let _ = self.event_tx.send(event);
    }

    /// Get workflow status from executor state
    pub fn get_workflow_status(&self, workflow_id: &str) -> Option<WorkflowStatus> {
        let state = self.state.lock().unwrap();
        state.workflows.get(workflow_id).cloned()
    }

    /// Get stored workflow result (if completed)
    pub fn get_workflow_result(&self, workflow_id: &str) -> Option<Vec<u8>> {
        let state = self.state.lock().unwrap();
        state.results.get(workflow_id).cloned()
    }

    /// Subscribe to workflow events
    pub fn subscribe_to_workflow(&self, workflow_id: &str) -> WorkflowSubscription {
        WorkflowSubscription {
            receiver: self.event_tx.subscribe(),
            target_workflow_id: workflow_id.to_string(),
        }
    }

    /// Get state for snapshot creation (only active workflows)
    pub fn get_state_for_snapshot(&self) -> crate::workflow::snapshot::SnapshotState {
        let state = self.state.lock().unwrap();

        // Only include active workflows
        let active_workflows: HashMap<String, WorkflowStatus> = state.workflows
            .iter()
            .filter(|(_, status)| matches!(status, WorkflowStatus::Running))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        crate::workflow::snapshot::SnapshotState {
            active_workflows,
            checkpoint_history: state.checkpoint_history.clone(),
        }
    }

    /// Restore state from snapshot
    pub fn restore_from_snapshot(&self, snapshot: crate::workflow::snapshot::WorkflowSnapshot) -> Result<(), Box<dyn std::error::Error>> {
        let mut state = self.state.lock().unwrap();

        // Clear current state
        state.workflows.clear();
        state.checkpoint_history = crate::workflow::snapshot::CheckpointHistory::default();

        // Restore active workflows
        for (workflow_id, status) in snapshot.active_workflows {
            state.workflows.insert(workflow_id, status);
        }

        // Restore checkpoint history
        state.checkpoint_history = snapshot.checkpoint_history.clone();

        // IMPORTANT: Build checkpoint queues from history
        // New node needs queues populated for when execution starts
        for ((workflow_id, key), entries) in snapshot.checkpoint_history.all_checkpoints() {
            let queue_key = (workflow_id.clone(), key.clone());
            let mut queue = std::collections::VecDeque::new();

            // Add all historical values to queue (oldest to newest)
            for entry in entries {
                queue.push_back(entry.value.clone());
            }

            state.checkpoint_queues.insert(queue_key, queue);
        }

        Ok(())
    }
}

impl CommandExecutor for WorkflowCommandExecutor {
    type Command = WorkflowCommand;

    fn apply_with_index(&self, command: &Self::Command, logger: &slog::Logger, log_index: u64) -> Result<(), Box<dyn std::error::Error>> {
        match command {
            WorkflowCommand::WorkflowStart(data) => {
                slog::info!(logger, "Started workflow";
                           "workflow_id" => &data.workflow_id,
                           "workflow_type" => &data.workflow_type,
                           "version" => data.version);

                // Set workflow status to Running
                {
                    let mut state = self.state.lock().unwrap();
                    state.workflows.insert(data.workflow_id.clone(), WorkflowStatus::Running);
                }

                // Emit started event
                self.emit_started_event(&data.workflow_id);

                // Spawn workflow execution on ALL nodes (leader and followers)
                // This is the key architectural change - all nodes execute workflows
                if !data.workflow_type.is_empty() && data.version > 0 {
                    let registry = self.registry.clone();
                    let runtime_opt = self.runtime.lock().unwrap().clone();

                    if let Some(runtime) = runtime_opt {
                        let workflow_id = data.workflow_id.clone();
                        let workflow_type = data.workflow_type.clone();
                        let version = data.version;
                        let input_bytes = data.input.clone();

                        // Spawn the workflow execution in the background
                        tokio::spawn(async move {
                            // Look up the workflow function
                            let workflow_function = {
                                let registry = registry.lock().unwrap();
                                match registry.get(&workflow_type, version) {
                                    Some(func) => func,
                                    None => {
                                        eprintln!("Workflow '{}' version {} not found in registry", workflow_type, version);
                                        return;
                                    }
                                }
                            };

                            // Create workflow context and WorkflowRun for this execution
                            let context = WorkflowContext {
                                workflow_id: workflow_id.clone(),
                                runtime: runtime.clone(),
                            };

                            let workflow_run = Arc::new(WorkflowRun {
                                context: context.clone(),
                                runtime: runtime.clone(),
                            });

                            // Deserialize input and execute the workflow function
                            let input_any: Box<dyn std::any::Any + Send> = Box::new(input_bytes);

                            match workflow_function.execute(input_any, context).await {
                                Ok(result_bytes) => {
                                    // Workflow execution succeeded - call finish_with to propose WorkflowEnd
                                    // Only the leader's call will actually propose (followers will get NotLeader error)
                                    // Propose WorkflowEnd with the serialized result
                                    // This will succeed on leader, fail with NotLeader on followers
                                    let _ = workflow_run.finish_with_bytes(result_bytes).await;

                                    slog::info!(slog::Logger::root(slog::Discard, slog::o!()),
                                               "Workflow execution completed successfully";
                                               "workflow_id" => &workflow_id);
                                },
                                Err(e) => {
                                    eprintln!("Workflow {} failed during execution: {}", workflow_id, e);
                                    // TODO: Emit failure event or propose WorkflowEnd with error
                                }
                            }
                        });
                    }
                }
            },
            WorkflowCommand::WorkflowEnd(data) => {
                slog::info!(logger, "Ended workflow"; "workflow_id" => &data.workflow_id);

                // Store the result in state for later retrieval
                self.state.lock().unwrap().results.insert(data.workflow_id.clone(), data.result.clone());

                // Clean up checkpoint history for completed workflow
                self.state.lock().unwrap()
                    .checkpoint_history
                    .remove_workflow(&data.workflow_id);

                // Clean up checkpoint queues for completed workflow
                self.state.lock().unwrap()
                    .checkpoint_queues
                    .retain(|(wf_id, _), _| wf_id != &data.workflow_id);

                self.emit_completed_event(&data.workflow_id, data.result.clone());
            },
            WorkflowCommand::SetCheckpoint(data) => {
                slog::info!(logger, "Set checkpoint";
                           "workflow_id" => &data.workflow_id,
                           "key" => &data.key,
                           "value_size" => data.value.len());

                // Enqueue the checkpoint value for late followers
                // All nodes enqueue - the consumption side will determine whether to use it
                let queue_key = (data.workflow_id.clone(), data.key.clone());
                self.state.lock().unwrap()
                    .checkpoint_queues
                    .entry(queue_key)
                    .or_insert_with(std::collections::VecDeque::new)
                    .push_back(data.value.clone());

                // Add to checkpoint history for snapshots (only for active workflows)
                {
                    let state = self.state.lock().unwrap();
                    if let Some(status) = state.workflows.get(&data.workflow_id) {
                        if matches!(status, WorkflowStatus::Running) {
                            drop(state); // Release lock before getting mutable access
                            let timestamp = crate::workflow::snapshot::current_timestamp();
                            self.state.lock().unwrap().checkpoint_history.add_checkpoint(
                                data.workflow_id.clone(),
                                data.key.clone(),
                                crate::workflow::snapshot::CheckpointEntry {
                                    value: data.value.clone(),
                                    log_index,
                                    timestamp,
                                }
                            );
                        }
                    }
                }

                self.emit_checkpoint_event(&data.workflow_id, &data.key, data.value.clone());
            },
        }
        Ok(())
    }
}

/// Status of a workflow
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WorkflowStatus {
    Running,
    Completed,
    Failed,
}

/// Events emitted when workflow state changes
#[derive(Clone, Debug)]
pub enum WorkflowEvent {
    /// A workflow has started with input data
    Started { workflow_id: String },
    /// A workflow has completed with output data
    Completed { workflow_id: String, result: Vec<u8> },
    /// A workflow has failed with error data
    Failed { workflow_id: String, error: Vec<u8> },
    /// A checkpoint was set for a workflow
    CheckpointSet { workflow_id: String, key: String, value: Vec<u8> },
}

/// Workflow state for tracking workflows across the cluster
#[derive(Default)]
pub struct WorkflowState {
    /// Track workflow status by ID
    pub workflows: HashMap<String, WorkflowStatus>,
    /// Store workflow results (serialized) by ID for retrieval after completion
    pub results: HashMap<String, Vec<u8>>,
    /// Queue of checkpoint values that arrived before execution reached them
    /// Key: (workflow_id, checkpoint_key), Value: Queue of serialized values
    /// This enables late followers to catch up without blocking on consensus
    pub checkpoint_queues: HashMap<(String, String), std::collections::VecDeque<Vec<u8>>>,
    /// Checkpoint history for snapshot creation (never popped, only appended)
    /// Complete history for active workflows, cleaned up on workflow completion
    pub checkpoint_history: crate::workflow::snapshot::CheckpointHistory,
}


/// Unified workflow runtime with cluster reference and workflow registry
#[derive(Clone)]
pub struct WorkflowRuntime {
    pub cluster: Arc<RaftCluster<WorkflowCommandExecutor>>,
}

impl std::fmt::Debug for WorkflowRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowRuntime")
            .field("cluster", &"Arc<RaftCluster<WorkflowCommandExecutor>>")
            .finish()
    }
}

/// Subscription helper for workflow-specific events
pub struct WorkflowSubscription {
    receiver: broadcast::Receiver<WorkflowEvent>,
    target_workflow_id: String,
}

impl WorkflowSubscription {
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
            WorkflowEvent::Started { workflow_id } => workflow_id == &self.target_workflow_id,
            WorkflowEvent::Completed { workflow_id, .. } => workflow_id == &self.target_workflow_id,
            WorkflowEvent::Failed { workflow_id, .. } => workflow_id == &self.target_workflow_id,
            WorkflowEvent::CheckpointSet { workflow_id, .. } => workflow_id == &self.target_workflow_id,
        }
    }
}

impl WorkflowRuntime {
    /// Create a new workflow runtime from an existing cluster
    ///
    /// This is the primary constructor for multi-node setups where the cluster
    /// is created via a ClusterTransport. The runtime automatically sets itself
    /// in the cluster's executor to enable workflow execution.
    ///
    /// # Example
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use raftoral::raft::generic::transport::{ClusterTransport, InMemoryClusterTransport};
    /// # use raftoral::workflow::{WorkflowCommandExecutor, WorkflowRuntime};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let transport = InMemoryClusterTransport::<WorkflowCommandExecutor>::new(vec![1, 2, 3]);
    /// let cluster = transport.create_cluster(1).await?;
    /// let runtime = WorkflowRuntime::new(cluster);
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(cluster: Arc<RaftCluster<WorkflowCommandExecutor>>) -> Arc<Self> {
        let runtime = Arc::new(WorkflowRuntime {
            cluster: cluster.clone(),
        });

        // Automatically set the runtime reference in the executor
        cluster.executor.set_runtime(runtime.clone());

        runtime
    }

    /// Create a new workflow runtime and cluster
    pub async fn new_single_node(node_id: u64) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        let executor = WorkflowCommandExecutor::new();
        let cluster = Arc::new(RaftCluster::new_single_node(node_id, executor).await?);

        let runtime = Arc::new(WorkflowRuntime {
            cluster: cluster.clone(),
        });

        // Set the runtime reference in the executor so it can spawn workflows
        cluster.executor.set_runtime(runtime.clone());

        Ok(runtime)
    }

    /// Get workflow status (delegate to executor)
    pub fn get_workflow_status(&self, workflow_id: &str) -> Option<WorkflowStatus> {
        self.cluster.executor.get_workflow_status(workflow_id)
    }

    /// Get stored workflow result (delegate to executor)
    pub fn get_workflow_result(&self, workflow_id: &str) -> Option<Vec<u8>> {
        self.cluster.executor.get_workflow_result(workflow_id)
    }

    /// Subscribe to workflow events for a specific workflow ID
    fn subscribe_to_workflow(&self, workflow_id: &str) -> WorkflowSubscription {
        self.cluster.executor.subscribe_to_workflow(workflow_id)
    }

    /// Start a new workflow and return a WorkflowRun instance
    pub async fn start<T>(self: &Arc<Self>, workflow_id: &str, input: T) -> Result<Arc<WorkflowRun>, WorkflowError>
    where
        T: serde::Serialize + Send + 'static,
    {
        WorkflowRun::start(workflow_id, self, input).await
    }

    /// Execute a leader/follower operation with retry logic for leadership changes
    async fn execute_leader_follower_operation<F, Fut, E, T>(
        &self,
        workflow_id: &str,
        cluster: &RaftCluster<WorkflowCommandExecutor>,
        leader_operation: F,
        wait_for_event_fn: E,
        follower_timeout: Option<Duration>,
    ) -> Result<T, WorkflowError>
    where
        F: Fn() -> Fut + Send,
        Fut: std::future::Future<Output = Result<T, WorkflowError>> + Send,
        E: Fn(&WorkflowEvent) -> Option<T> + Send + Sync + Clone,
        T: Send + 'static,
    {
        // Retry loop to handle leadership changes
        let max_retries = 3;
        for attempt in 0..max_retries {
            // Subscribe first to avoid race conditions
            let subscription = self.subscribe_to_workflow(workflow_id);

            // Check if we're the leader
            if cluster.is_leader().await {
                // Leader behavior: Execute the provided operation
                return leader_operation().await;
            } else {
                // Follower behavior: Wait for command to be applied by leader
                // Use a shorter timeout to allow retrying if we become leader or use provided timeout on final attempt
                let timeout_duration = if attempt < max_retries - 1 {
                    Some(Duration::from_secs(5)) // Shorter timeout for retry attempts
                } else {
                    follower_timeout // Use provided timeout for final attempt (could be None for indefinite wait)
                };

                match WorkflowRuntime::wait_for_specific_event(workflow_id, subscription, wait_for_event_fn.clone(), timeout_duration).await {
                    Ok(value) => return Ok(value),
                    Err(WorkflowError::Timeout) => {
                        // Timeout - check if we became leader and should retry
                        if cluster.is_leader().await {
                            continue; // Retry as leader
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

    /// Wait for a specific event with state checking
    async fn wait_for_specific_event<E, T>(
        workflow_id: &str,
        mut subscription: WorkflowSubscription,
        event_extractor: E,
        timeout_duration: Option<Duration>
    ) -> Result<T, WorkflowError>
    where
        E: Fn(&WorkflowEvent) -> Option<T> + Send + Sync + Clone,
        T: Send + 'static,
    {
        let workflow_id = workflow_id.to_string();
        let filtered_extractor = move |event: &WorkflowEvent| {
            // First check if this event is for our workflow_id
            let event_workflow_id = match event {
                WorkflowEvent::Started { workflow_id } => workflow_id,
                WorkflowEvent::Completed { workflow_id, .. } => workflow_id,
                WorkflowEvent::Failed { workflow_id, .. } => workflow_id,
                WorkflowEvent::CheckpointSet { workflow_id, .. } => workflow_id,
            };

            if event_workflow_id != &workflow_id {
                return None; // Not for our workflow
            }

            // Then apply the user's event extractor
            event_extractor(event)
        };

        subscription.wait_for_event(filtered_extractor, timeout_duration).await
    }


    /// Try to get a queued checkpoint value for a late follower (internal use)
    /// Returns Some(value) if a queued checkpoint exists, None otherwise
    ///
    /// Uses simple FIFO queue semantics: pop once from front.
    /// Deterministic execution guarantees the queue order matches execution order.
    fn try_dequeue_checkpoint<T>(
        &self,
        workflow_id: &str,
        key: &str,
        cluster: &RaftCluster<WorkflowCommandExecutor>,
    ) -> Result<Option<T>, WorkflowError>
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de> + Clone + Send + Sync + 'static,
    {
        let queue_key = (workflow_id.to_string(), key.to_string());
        let mut state = cluster.executor.state.lock().unwrap();

        if let Some(queue) = state.checkpoint_queues.get_mut(&queue_key) {
            // Simple FIFO: pop once from front
            // Deterministic execution ensures this is the correct value
            if let Some(serialized_value) = queue.pop_front() {
                let deserialized = serde_json::from_slice::<T>(&serialized_value)
                    .map_err(|e| WorkflowError::ClusterError(format!("Deserialization error: {}", e)))?;
                return Ok(Some(deserialized));
            }
        }

        Ok(None)
    }

    /// Set a replicated variable value (used by ReplicatedVar)
    ///
    /// Followers check for queued values first (late follower catch-up).
    /// Leaders always propose through consensus.
    /// Deterministic execution ensures FIFO queue order matches execution order.
    pub async fn set_replicated_var<T>(
        &self,
        workflow_id: &str,
        key: &str,
        value: T,
        cluster: &RaftCluster<WorkflowCommandExecutor>,
    ) -> Result<T, WorkflowError>
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de> + Clone + Send + Sync + 'static,
    {
        let key_owned = key.to_string();
        let workflow_id_owned = workflow_id.to_string();

        // Always check queue first (for late followers and new leaders catching up)
        // Leaders clean up their own proposed values from the queue (see line 554-560)
        if let Some(queued_value) = self.try_dequeue_checkpoint(workflow_id, key, cluster)? {
            return Ok(queued_value);
        }

        // No queued value - proceed with normal consensus-based flow
        // Serialize the value
        let serialized_value = serde_json::to_vec(&value)
            .map_err(|e| WorkflowError::ClusterError(format!("Serialization error: {}", e)))?;

        // Use the reusable leader/follower pattern with return value
        self.execute_leader_follower_operation(
            workflow_id,
            cluster,
            || {
                let value_clone = value.clone();
                let serialized_value_clone = serialized_value.clone();
                let workflow_id_owned_clone = workflow_id_owned.clone();
                let key_owned_clone = key_owned.clone();
                async move {
                    // Leader operation: Propose SetCheckpoint command and return the value
                    let command = WorkflowCommand::SetCheckpoint(CheckpointData {
                        workflow_id: workflow_id_owned_clone.clone(),
                        key: key_owned_clone.clone(),
                        value: serialized_value_clone,
                    });

                    cluster.propose_and_sync(command).await
                        .map_err(|e| WorkflowError::ClusterError(e.to_string()))?;

                    // Pop the value we just enqueued (during apply in propose_and_sync)
                    // This prevents leader's own execution from consuming its proposed values
                    let queue_key = (workflow_id_owned_clone, key_owned_clone);
                    cluster.executor.state.lock().unwrap()
                        .checkpoint_queues
                        .get_mut(&queue_key)
                        .and_then(|queue| queue.pop_back()); // Pop the value we just pushed

                    Ok(value_clone)
                }
            },
            |event| {
                if let WorkflowEvent::CheckpointSet { workflow_id, key, value } = event {
                    if workflow_id == &workflow_id_owned && key == &key_owned {
                        // Deserialize and return the value
                        if let Ok(deserialized) = serde_json::from_slice::<T>(value) {
                            return Some(deserialized);
                        }
                    }
                }
                None
            },
            None, // Wait indefinitely
        ).await
    }

    /// Register a workflow function with the given type and version
    ///
    /// # Arguments
    /// * `workflow_type` - Unique identifier for the workflow type
    /// * `version` - Version number for this workflow implementation
    /// * `function` - The workflow function to register
    ///
    /// # Returns
    /// * `Ok(())` if registration was successful
    /// * `Err(WorkflowError)` if a workflow with the same type and version already exists
    pub fn register_workflow<I, O, F>(
        &self,
        workflow_type: &str,
        version: u32,
        function: F,
    ) -> Result<(), WorkflowError>
    where
        I: for<'de> serde::Deserialize<'de> + Send + 'static,
        O: serde::Serialize + Send + 'static,
        F: crate::workflow::registry::WorkflowFunction<I, O>,
    {
        let registry = self.cluster.executor.registry();
        let mut registry = registry.lock().unwrap();
        registry.register(workflow_type, version, function)
    }

    /// Register a workflow function using a closure for easier testing and simple workflows
    pub fn register_workflow_closure<I, O, F, Fut>(
        &self,
        workflow_type: &str,
        version: u32,
        function: F,
    ) -> Result<(), WorkflowError>
    where
        I: for<'de> serde::Deserialize<'de> + Send + Sync + 'static,
        O: serde::Serialize + Send + Sync + 'static,
        F: Fn(I, WorkflowContext) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<O, WorkflowError>> + Send + Sync + 'static,
    {
        let registry = self.cluster.executor.registry();
        let mut registry = registry.lock().unwrap();
        registry.register_closure(workflow_type, version, function)
    }

    /// Start a workflow by type and version instead of arbitrary ID
    ///
    /// This method proposes a WorkflowStart command with the workflow type, version, and input.
    /// The workflow execution will be automatically spawned by the executor on all nodes when
    /// they apply the WorkflowStart command through Raft consensus.
    ///
    /// # Arguments
    /// * `workflow_type` - The registered workflow type
    /// * `version` - The workflow version
    /// * `input` - Serializable input for the workflow function
    ///
    /// # Returns
    /// * `Ok(TypedWorkflowRun<O>)` if the workflow was started successfully
    /// * `Err(WorkflowError)` if the workflow type is not registered or startup failed
    pub async fn start_workflow<I, O>(
        self: &Arc<Self>,
        workflow_type: &str,
        version: u32,
        input: I,
    ) -> Result<TypedWorkflowRun<O>, WorkflowError>
    where
        I: Send + Clone + serde::Serialize + 'static,
        O: Clone + Send + Sync + serde::Serialize + for<'de> serde::Deserialize<'de> + 'static,
    {
        // Verify the workflow function is registered
        {
            let registry = self.cluster.executor.registry();
            let registry = registry.lock().unwrap();
            if !registry.contains(workflow_type, version) {
                return Err(WorkflowError::NotFound(format!("Workflow '{}' version {} not found", workflow_type, version)));
            }
        }

        // Generate a unique workflow ID
        let workflow_id = format!("{}_v{}_{}",
                                workflow_type,
                                version,
                                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis());

        // Serialize the input
        let input_bytes = serde_json::to_vec(&input).map_err(|e| WorkflowError::SerializationError(e.to_string()))?;

        // Check if workflow already exists and is running
        if let Some(status) = self.get_workflow_status(&workflow_id) {
            if status == WorkflowStatus::Running {
                return Err(WorkflowError::AlreadyExists(workflow_id));
            }
        }

        // Propose WorkflowStart command directly - Raft will handle leader routing
        // All nodes will execute the workflow when they see it applied via consensus
        let command = WorkflowCommand::WorkflowStart(WorkflowStartData {
            workflow_id: workflow_id.clone(),
            workflow_type: workflow_type.to_string(),
            version,
            input: input_bytes.clone(),
        });

        self.cluster.propose_and_sync(command).await
            .map_err(|e| WorkflowError::ClusterError(e.to_string()))?;

        // Create and return TypedWorkflowRun
        let context = WorkflowContext {
            workflow_id: workflow_id.clone(),
            runtime: self.clone(),
        };
        let workflow_run = Arc::new(WorkflowRun {
            context,
            runtime: self.clone(),
        });

        Ok(TypedWorkflowRun::new(workflow_run))
    }

    /// Check if a workflow type and version is registered
    pub fn has_workflow(&self, workflow_type: &str, version: u32) -> bool {
        let registry = self.cluster.executor.registry();
        let registry = registry.lock().unwrap();
        registry.contains(workflow_type, version)
    }

    /// List all registered workflow types and versions
    pub fn list_workflows(&self) -> Vec<(String, u32)> {
        let registry = self.cluster.executor.registry();
        let registry = registry.lock().unwrap();
        registry.list_workflows()
    }
}


/// A scoped workflow run that must be explicitly finished
/// Shared workflow context that can be safely shared with ReplicatedVars
#[derive(Clone)]
pub struct WorkflowContext {
    pub workflow_id: String,
    pub runtime: Arc<WorkflowRuntime>,
}

impl std::fmt::Debug for WorkflowContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowContext")
            .field("workflow_id", &self.workflow_id)
            .field("runtime", &"<WorkflowRuntime>")
            .finish()
    }
}

impl WorkflowContext {
    /// Create a ReplicatedVar from within a workflow execution context
    ///
    /// This is used by workflow functions that are spawned via consensus-driven execution.
    pub async fn create_replicated_var<T>(
        &self,
        key: &str,
        value: T,
    ) -> Result<crate::workflow::replicated_var::ReplicatedVar<T>, WorkflowError>
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de> + Clone + Send + Sync + 'static,
    {
        // Create a WorkflowRun for ReplicatedVar operations
        // This WorkflowRun is marked as finished since the workflow lifecycle is managed elsewhere
        let workflow_run = Arc::new(WorkflowRun {
            context: self.clone(),
            runtime: self.runtime.clone(),
        });

        crate::workflow::replicated_var::ReplicatedVar::with_value(key, &workflow_run, value).await
    }

    /// Create a ReplicatedVar with computation from within a workflow execution context
    pub async fn create_replicated_var_with_computation<T, F, Fut>(
        &self,
        key: &str,
        computation: F,
    ) -> Result<crate::workflow::replicated_var::ReplicatedVar<T>, WorkflowError>
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de> + Clone + Send + Sync + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T> + Send + 'static,
    {
        // Create a WorkflowRun for ReplicatedVar operations
        let workflow_run = Arc::new(WorkflowRun {
            context: self.clone(),
            runtime: self.runtime.clone(),
        });

        crate::workflow::replicated_var::ReplicatedVar::with_computation(key, &workflow_run, computation).await
    }
}


#[derive(Debug)]
pub struct WorkflowRun {
    pub context: WorkflowContext,
    pub runtime: Arc<WorkflowRuntime>,
}

impl WorkflowRun {
    /// Create a WorkflowRun for an existing workflow (internal use)
    pub fn for_existing(
        workflow_id: &str,
        runtime: Arc<WorkflowRuntime>
    ) -> Self {
        let context = WorkflowContext {
            workflow_id: workflow_id.to_string(),
            runtime: runtime.clone(),
        };
        WorkflowRun {
            context,
            runtime,
        }
    }

    /// Start a new workflow and return a WorkflowRun instance
    pub async fn start<T>(
        workflow_id: &str,
        runtime: &Arc<WorkflowRuntime>,
        input: T
    ) -> Result<Arc<Self>, WorkflowError>
    where
        T: serde::Serialize + Send + 'static,
    {
        let cluster = runtime.cluster.clone();

        // Check if workflow already exists and is running
        if let Some(status) = runtime.get_workflow_status(workflow_id) {
            if status == WorkflowStatus::Running {
                return Err(WorkflowError::AlreadyExists(workflow_id.to_string()));
            }
        }

        // Serialize the input
        let input_bytes = serde_json::to_vec(&input).map_err(|e| WorkflowError::SerializationError(e.to_string()))?;

        // Use the reusable leader/follower pattern to start the workflow
        let workflow_id_owned = workflow_id.to_string();

        runtime.execute_leader_follower_operation(
            workflow_id,
            &cluster,
            || async {
                // Leader operation: Propose WorkflowStart command with input
                // Note: workflow_type and version are empty when not using start_workflow_typed()
                let command = WorkflowCommand::WorkflowStart(WorkflowStartData {
                    workflow_id: workflow_id_owned.clone(),
                    workflow_type: String::new(),
                    version: 0,
                    input: input_bytes.clone(),
                });

                cluster.propose_and_sync(command).await
                    .map_err(|e| WorkflowError::ClusterError(e.to_string()))?;
                Ok(())
            },
            |event| {
                if matches!(event, WorkflowEvent::Started { .. }) {
                    Some(())
                } else {
                    None
                }
            },
            Some(Duration::from_secs(10)),
        ).await?;

        let context = WorkflowContext {
            workflow_id: workflow_id.to_string(),
            runtime: runtime.clone(),
        };
        Ok(Arc::new(WorkflowRun {
            context,
            runtime: runtime.clone(),
        }))
    }

    /// Get the workflow ID
    pub fn workflow_id(&self) -> &str {
        &self.context.workflow_id
    }

    /// Get a reference to the cluster
    pub fn cluster(&self) -> &Arc<RaftCluster<WorkflowCommandExecutor>> {
        &self.runtime.cluster
    }

    /// Get the workflow context (for ReplicatedVar creation)
    pub fn context(&self) -> &WorkflowContext {
        &self.context
    }


    /// Finish the workflow with a result value
    pub async fn finish_with<T>(&self, result_value: T) -> Result<T, WorkflowError>
    where
        T: Clone + Send + Sync + serde::Serialize + 'static,
    {
        // Serialize the result value as Result<T, WorkflowError>
        let result_as_result: Result<T, WorkflowError> = Ok(result_value.clone());
        let result_bytes = serde_json::to_vec(&result_as_result)
            .map_err(|e| WorkflowError::SerializationError(e.to_string()))?;

        // Call finish_with_bytes to do the actual work
        self.finish_with_bytes(result_bytes).await?;

        Ok(result_value)
    }

    /// Finish the workflow with raw bytes (internal use by executor)
    pub async fn finish_with_bytes(&self, result_bytes: Vec<u8>) -> Result<(), WorkflowError> {
        // Check if workflow exists and is running
        match self.runtime.get_workflow_status(&self.context.workflow_id) {
            Some(WorkflowStatus::Running) => {},
            Some(WorkflowStatus::Completed) => {
                        return Ok(()); // Already completed
            },
            Some(_) => return Err(WorkflowError::ClusterError("Workflow is not running".to_string())),
            None => return Err(WorkflowError::NotFound(self.context.workflow_id.to_string())),
        }

        let workflow_id_owned = self.context.workflow_id.clone();
        let result_bytes_owned = result_bytes.clone();

        self.runtime.execute_leader_follower_operation(
            &self.context.workflow_id,
            &self.runtime.cluster,
            || async {
                // Leader operation: Propose WorkflowEnd command with result
                let command = WorkflowCommand::WorkflowEnd(WorkflowEndData {
                    workflow_id: workflow_id_owned.clone(),
                    result: result_bytes_owned.clone(),
                });

                self.runtime.cluster.propose_and_sync(command).await
                    .map_err(|e| WorkflowError::ClusterError(e.to_string()))?;
                Ok(())
            },
            |event| {
                if matches!(event, WorkflowEvent::Completed { .. } | WorkflowEvent::Failed { .. }) {
                    Some(())
                } else {
                    None
                }
            },
            None, // Wait indefinitely
        ).await?;

        // Mark as finished
        Ok(())
    }

    /// Wait for the workflow to complete and return the result
    /// This method waits for the workflow to finish naturally without manually ending it
    pub async fn wait_for_completion(&self) -> Result<(), WorkflowError> {
        // Check current status
        match self.runtime.get_workflow_status(&self.context.workflow_id) {
            Some(WorkflowStatus::Completed) => {
                        return Ok(());
            },
            Some(WorkflowStatus::Failed) => {
                        return Err(WorkflowError::ClusterError("Workflow failed".to_string()));
            },
            Some(WorkflowStatus::Running) => {
                // Continue to wait
            },
            None => return Err(WorkflowError::NotFound(self.context.workflow_id.to_string())),
        }

        // Wait for completion event
        self.runtime.execute_leader_follower_operation(
            &self.context.workflow_id,
            &self.runtime.cluster,
            || async {
                // For followers, we don't need to do anything - just wait for the event
                Ok(())
            },
            |event| {
                if matches!(event, WorkflowEvent::Completed { .. } | WorkflowEvent::Failed { .. }) {
                    Some(())
                } else {
                    None
                }
            },
            None, // Wait indefinitely
        ).await?;

        // Final state verification
        match self.runtime.get_workflow_status(&self.context.workflow_id) {
            Some(WorkflowStatus::Completed) => {
                        Ok(())
            },
            Some(WorkflowStatus::Failed) => {
                        Err(WorkflowError::ClusterError("Workflow failed".to_string()))
            },
            _ => Err(WorkflowError::ClusterError("Unexpected workflow state".to_string())),
        }
    }

}

// Drop implementation removed - no longer needed since workflow execution
// happens in a separate spawned task, not tied to WorkflowRun lifetime

/// A typed wrapper for WorkflowRun that preserves the output type from start_workflow_typed
/// This allows wait_for_completion to return the correctly typed result
#[derive(Debug)]
pub struct TypedWorkflowRun<O> {
    inner: Arc<WorkflowRun>,
    _phantom: PhantomData<O>,
}

impl<O> TypedWorkflowRun<O>
where
    O: Clone + Send + Sync + serde::Serialize + for<'de> serde::Deserialize<'de> + 'static,
{
    /// Create a new TypedWorkflowRun from a WorkflowRun
    pub(crate) fn new(inner: Arc<WorkflowRun>) -> Self {
        Self {
            inner,
            _phantom: PhantomData,
        }
    }

    /// Get the workflow ID
    pub fn workflow_id(&self) -> &str {
        self.inner.workflow_id()
    }

    /// Get the workflow context
    pub fn context(&self) -> &WorkflowContext {
        self.inner.context()
    }

    /// Wait for the workflow to complete and return the typed result
    /// This method waits for the workflow to finish naturally and returns the result
    pub async fn wait_for_completion(self) -> Result<O, WorkflowError> {
        // First, subscribe to events BEFORE checking state to avoid race condition
        let mut subscription = self.inner.runtime.subscribe_to_workflow(&self.inner.context.workflow_id);

        // Check if workflow already completed (race condition: completed before we subscribed)
        match self.inner.runtime.get_workflow_status(&self.inner.context.workflow_id) {
            Some(WorkflowStatus::Completed) => {
                // Workflow already completed - retrieve result from stored state
                if let Some(result_bytes) = self.inner.runtime.get_workflow_result(&self.inner.context.workflow_id) {
                    // Deserialize and return the stored result
                    let workflow_result: Result<O, WorkflowError> = serde_json::from_slice(&result_bytes)
                        .map_err(|e| WorkflowError::DeserializationError(e.to_string()))?;

                    return workflow_result;
                } else {
                    return Err(WorkflowError::ClusterError(
                        "Workflow completed but result not found in state".to_string()
                    ));
                }
            },
            Some(WorkflowStatus::Failed) => {
                // Try to get the error from stored results
                if let Some(error_bytes) = self.inner.runtime.get_workflow_result(&self.inner.context.workflow_id) {
                    let workflow_result: Result<O, WorkflowError> = serde_json::from_slice(&error_bytes)
                        .map_err(|e| WorkflowError::DeserializationError(e.to_string()))?;

                    return workflow_result;
                }
                return Err(WorkflowError::ClusterError("Workflow failed".to_string()));
            },
            _ => {
                // Workflow still running or starting, proceed with subscription
            }
        }

        // Wait for the workflow completion event and extract the result
        let result_bytes = subscription.wait_for_event(
            |event| {
                match event {
                    WorkflowEvent::Completed { workflow_id, result } if workflow_id == &self.inner.context.workflow_id => {
                        Some(result.clone())
                    },
                    WorkflowEvent::Failed { workflow_id, error } if workflow_id == &self.inner.context.workflow_id => {
                        Some(error.clone())
                    },
                    _ => None
                }
            },
            Some(Duration::from_secs(30))
        ).await?;

        // Deserialize the result from the event data
        let workflow_result: Result<O, WorkflowError> = serde_json::from_slice(&result_bytes)
            .map_err(|e| WorkflowError::DeserializationError(e.to_string()))?;

        // Return the final result or error
        workflow_result
    }
}

/// Error types for workflow operations
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum WorkflowError {
    /// Workflow already exists
    AlreadyExists(String),
    /// Workflow not found
    NotFound(String),
    /// Not the leader - cannot start workflows
    NotLeader,
    /// Cluster operation failed
    ClusterError(String),
    /// Timeout waiting for workflow to start
    Timeout,
    /// Serialization error
    SerializationError(String),
    /// Deserialization error
    DeserializationError(String),
}

impl std::fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkflowError::AlreadyExists(id) => write!(f, "Workflow '{}' already exists", id),
            WorkflowError::NotFound(id) => write!(f, "Workflow '{}' not found", id),
            WorkflowError::NotLeader => write!(f, "Not the leader - cannot start workflows"),
            WorkflowError::ClusterError(msg) => write!(f, "Cluster error: {}", msg),
            WorkflowError::Timeout => write!(f, "Timeout waiting for workflow to start"),
            WorkflowError::SerializationError(msg) => write!(f, "Serialization error: {}", msg),
            WorkflowError::DeserializationError(msg) => write!(f, "Deserialization error: {}", msg),
        }
    }
}

impl std::error::Error for WorkflowError {}

#[cfg(test)]
mod tests {
    use super::*;
    use slog::{Drain, Logger, o};

    fn test_logger() -> Logger {
        let decorator = slog_term::PlainSyncDecorator::new(std::io::sink());
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        Logger::root(drain, o!())
    }

    #[test]
    fn test_checkpoint_history_tracking() {
        // Create executor
        let executor = WorkflowCommandExecutor::new();
        let logger = test_logger();

        // Start a workflow
        let start_cmd = WorkflowCommand::WorkflowStart(WorkflowStartData {
            workflow_id: "test_wf".to_string(),
            workflow_type: "test_type".to_string(),
            version: 1,
            input: vec![],
        });
        executor.apply_with_index(&start_cmd, &logger, 1).unwrap();

        // Add checkpoint 1 at log index 2
        let checkpoint_cmd1 = WorkflowCommand::SetCheckpoint(CheckpointData {
            workflow_id: "test_wf".to_string(),
            key: "step1".to_string(),
            value: vec![1, 2, 3],
        });
        executor.apply_with_index(&checkpoint_cmd1, &logger, 2).unwrap();

        // Add checkpoint 2 at log index 3
        let checkpoint_cmd2 = WorkflowCommand::SetCheckpoint(CheckpointData {
            workflow_id: "test_wf".to_string(),
            key: "step2".to_string(),
            value: vec![4, 5, 6],
        });
        executor.apply_with_index(&checkpoint_cmd2, &logger, 3).unwrap();

        // Add another value for step1 at log index 4
        let checkpoint_cmd3 = WorkflowCommand::SetCheckpoint(CheckpointData {
            workflow_id: "test_wf".to_string(),
            key: "step1".to_string(),
            value: vec![7, 8, 9],
        });
        executor.apply_with_index(&checkpoint_cmd3, &logger, 4).unwrap();

        // Verify checkpoint history
        let snapshot_state = executor.get_state_for_snapshot();

        // Check step1 has 2 entries
        let step1_checkpoints = snapshot_state.checkpoint_history
            .get_checkpoints("test_wf", "step1")
            .unwrap();
        assert_eq!(step1_checkpoints.len(), 2);
        assert_eq!(step1_checkpoints[0].value, vec![1, 2, 3]);
        assert_eq!(step1_checkpoints[0].log_index, 2);
        assert_eq!(step1_checkpoints[1].value, vec![7, 8, 9]);
        assert_eq!(step1_checkpoints[1].log_index, 4);

        // Check step2 has 1 entry
        let step2_checkpoints = snapshot_state.checkpoint_history
            .get_checkpoints("test_wf", "step2")
            .unwrap();
        assert_eq!(step2_checkpoints.len(), 1);
        assert_eq!(step2_checkpoints[0].value, vec![4, 5, 6]);
        assert_eq!(step2_checkpoints[0].log_index, 3);
    }

    #[test]
    fn test_checkpoint_history_cleanup_on_workflow_end() {
        let executor = WorkflowCommandExecutor::new();
        let logger = test_logger();

        // Start workflow
        let start_cmd = WorkflowCommand::WorkflowStart(WorkflowStartData {
            workflow_id: "test_wf".to_string(),
            workflow_type: "test_type".to_string(),
            version: 1,
            input: vec![],
        });
        executor.apply_with_index(&start_cmd, &logger, 1).unwrap();

        // Add checkpoint
        let checkpoint_cmd = WorkflowCommand::SetCheckpoint(CheckpointData {
            workflow_id: "test_wf".to_string(),
            key: "step1".to_string(),
            value: vec![1, 2, 3],
        });
        executor.apply_with_index(&checkpoint_cmd, &logger, 2).unwrap();

        // Verify checkpoint exists
        let snapshot_state = executor.get_state_for_snapshot();
        assert!(snapshot_state.checkpoint_history.get_checkpoints("test_wf", "step1").is_some());

        // End workflow
        let end_cmd = WorkflowCommand::WorkflowEnd(WorkflowEndData {
            workflow_id: "test_wf".to_string(),
            result: vec![],
        });
        executor.apply_with_index(&end_cmd, &logger, 3).unwrap();

        // Verify checkpoint history is cleaned up
        let snapshot_state = executor.get_state_for_snapshot();
        assert!(snapshot_state.checkpoint_history.get_checkpoints("test_wf", "step1").is_none());

        // Verify checkpoint queue is also cleaned up
        let state = executor.state.lock().unwrap();
        assert!(!state.checkpoint_queues.contains_key(&("test_wf".to_string(), "step1".to_string())));
    }

    #[test]
    fn test_checkpoint_history_only_active_workflows() {
        let executor = WorkflowCommandExecutor::new();
        let logger = test_logger();

        // Start workflow 1
        let start_cmd1 = WorkflowCommand::WorkflowStart(WorkflowStartData {
            workflow_id: "wf1".to_string(),
            workflow_type: "test_type".to_string(),
            version: 1,
            input: vec![],
        });
        executor.apply_with_index(&start_cmd1, &logger, 1).unwrap();

        // Add checkpoint to workflow 1
        let checkpoint_cmd1 = WorkflowCommand::SetCheckpoint(CheckpointData {
            workflow_id: "wf1".to_string(),
            key: "step1".to_string(),
            value: vec![1, 2, 3],
        });
        executor.apply_with_index(&checkpoint_cmd1, &logger, 2).unwrap();

        // End workflow 1
        let end_cmd1 = WorkflowCommand::WorkflowEnd(WorkflowEndData {
            workflow_id: "wf1".to_string(),
            result: vec![],
        });
        executor.apply_with_index(&end_cmd1, &logger, 3).unwrap();

        // Start workflow 2
        let start_cmd2 = WorkflowCommand::WorkflowStart(WorkflowStartData {
            workflow_id: "wf2".to_string(),
            workflow_type: "test_type".to_string(),
            version: 1,
            input: vec![],
        });
        executor.apply_with_index(&start_cmd2, &logger, 4).unwrap();

        // Add checkpoint to workflow 2
        let checkpoint_cmd2 = WorkflowCommand::SetCheckpoint(CheckpointData {
            workflow_id: "wf2".to_string(),
            key: "step1".to_string(),
            value: vec![4, 5, 6],
        });
        executor.apply_with_index(&checkpoint_cmd2, &logger, 5).unwrap();

        // Get snapshot state - should only include active workflow (wf2)
        let snapshot_state = executor.get_state_for_snapshot();

        // wf1 should not be in snapshot (completed)
        assert_eq!(snapshot_state.active_workflows.len(), 1);
        assert!(snapshot_state.active_workflows.contains_key("wf2"));
        assert!(!snapshot_state.active_workflows.contains_key("wf1"));

        // wf1 checkpoints should be cleaned up
        assert!(snapshot_state.checkpoint_history.get_checkpoints("wf1", "step1").is_none());

        // wf2 checkpoints should still exist
        assert!(snapshot_state.checkpoint_history.get_checkpoints("wf2", "step1").is_some());
    }
}

