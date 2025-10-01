use crate::raft::generic::RaftCluster;
use crate::raft::generic::message::CommandExecutor;
use crate::workflow::registry::WorkflowRegistry;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::sync::atomic::{AtomicBool, Ordering};
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

impl WorkflowCommandExecutor {
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(1000);
        Self {
            state: Arc::new(Mutex::new(WorkflowState::default())),
            event_tx,
            registry: Arc::new(Mutex::new(WorkflowRegistry::new())),
            runtime: Arc::new(Mutex::new(None)),
        }
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

    /// Subscribe to workflow events
    pub fn subscribe_to_workflow(&self, workflow_id: &str) -> WorkflowSubscription {
        WorkflowSubscription {
            receiver: self.event_tx.subscribe(),
            target_workflow_id: workflow_id.to_string(),
        }
    }
}

impl CommandExecutor for WorkflowCommandExecutor {
    type Command = WorkflowCommand;

    fn apply(&self, command: &Self::Command, logger: &slog::Logger) -> Result<(), Box<dyn std::error::Error>> {
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

                            // Create workflow context
                            let context = WorkflowContext {
                                workflow_id: workflow_id.clone(),
                                runtime: runtime.clone(),
                            };

                            // Deserialize input and execute the workflow function
                            let input_any: Box<dyn std::any::Any + Send> = Box::new(input_bytes);

                            match workflow_function.execute(input_any, context).await {
                                Ok(_output_any) => {
                                    // Serialize the output
                                    // Note: The workflow function should call finish_with() which will
                                    // propose WorkflowEnd, so we don't need to do anything here
                                    slog::info!(slog::Logger::root(slog::Discard, slog::o!()),
                                               "Workflow execution completed successfully";
                                               "workflow_id" => &workflow_id);
                                },
                                Err(e) => {
                                    eprintln!("Workflow {} failed during execution: {}", workflow_id, e);
                                }
                            }
                        });
                    }
                }
            },
            WorkflowCommand::WorkflowEnd(data) => {
                slog::info!(logger, "Ended workflow"; "workflow_id" => &data.workflow_id);
                self.emit_completed_event(&data.workflow_id, data.result.clone());
            },
            WorkflowCommand::SetCheckpoint(data) => {
                slog::info!(logger, "Set checkpoint";
                           "workflow_id" => &data.workflow_id,
                           "key" => &data.key,
                           "value_size" => data.value.len());

                self.emit_checkpoint_event(&data.workflow_id, &data.key, data.value.clone());
            },
        }
        Ok(())
    }
}

/// Status of a workflow
#[derive(Clone, Debug, PartialEq)]
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
        _workflow_id: &str,
        mut subscription: WorkflowSubscription,
        event_extractor: E,
        timeout_duration: Option<Duration>
    ) -> Result<T, WorkflowError>
    where
        E: Fn(&WorkflowEvent) -> Option<T> + Send + Sync + Clone,
        T: Send + 'static,
    {
        subscription.wait_for_event(event_extractor, timeout_duration).await
    }


    /// Set a replicated variable value (used by ReplicatedVar)
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
                        workflow_id: workflow_id_owned_clone,
                        key: key_owned_clone,
                        value: serialized_value_clone,
                    });

                    cluster.propose_and_sync(command).await
                        .map_err(|e| WorkflowError::ClusterError(e.to_string()))?;
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
    pub async fn start_workflow_typed<I, O>(
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

        let cluster = self.cluster.clone();
        let workflow_id_owned = workflow_id.clone();

        // Use the reusable leader/follower pattern to start the workflow with type and version info
        self.execute_leader_follower_operation(
            &workflow_id,
            &cluster,
            || async {
                // Leader operation: Propose WorkflowStart command with type, version, and input
                let command = WorkflowCommand::WorkflowStart(WorkflowStartData {
                    workflow_id: workflow_id_owned.clone(),
                    workflow_type: workflow_type.to_string(),
                    version,
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

        // Create and return TypedWorkflowRun
        let context = WorkflowContext {
            workflow_id: workflow_id.clone(),
            runtime: self.clone(),
        };
        let workflow_run = Arc::new(WorkflowRun {
            context,
            runtime: self.clone(),
            finished: AtomicBool::new(false),
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
            finished: AtomicBool::new(true), // Pre-marked as finished to avoid drop panic
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
            finished: AtomicBool::new(true), // Pre-marked as finished to avoid drop panic
        });

        crate::workflow::replicated_var::ReplicatedVar::with_computation(key, &workflow_run, computation).await
    }
}


#[derive(Debug)]
pub struct WorkflowRun {
    pub context: WorkflowContext,
    pub runtime: Arc<WorkflowRuntime>,
    finished: AtomicBool,
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
            finished: AtomicBool::new(false),
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
            finished: AtomicBool::new(false),
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

    /// Finish the workflow without a result
    pub async fn finish(&self) -> Result<(), WorkflowError> {
        // Check if workflow exists and is running
        match self.runtime.get_workflow_status(&self.context.workflow_id) {
            Some(WorkflowStatus::Running) => {},
            Some(WorkflowStatus::Completed) => {
                self.finished.store(true, Ordering::SeqCst);
                return Ok(()); // Already completed
            },
            Some(_) => return Err(WorkflowError::ClusterError("Workflow is not running".to_string())),
            None => return Err(WorkflowError::NotFound(self.context.workflow_id.to_string())),
        }

        // Use the reusable leader/follower pattern to end the workflow
        let workflow_id_owned = self.context.workflow_id.clone();

        self.runtime.execute_leader_follower_operation(
            &self.context.workflow_id,
            &self.runtime.cluster,
            || async {
                // Leader operation: Propose WorkflowEnd command
                let command = WorkflowCommand::WorkflowEnd(WorkflowEndData {
                    workflow_id: workflow_id_owned.clone(),
                    result: Vec::new(),  // Empty result for workflows without explicit result
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

        // Final state verification - check for failure after completion
        match self.runtime.get_workflow_status(&self.context.workflow_id) {
            Some(WorkflowStatus::Completed) => {
                self.finished.store(true, Ordering::SeqCst);
                Ok(())
            },
            Some(WorkflowStatus::Failed) => Err(WorkflowError::ClusterError("Workflow failed".to_string())),
            _ => Err(WorkflowError::ClusterError("Unexpected workflow state".to_string())),
        }
    }

    /// Finish the workflow with a result value
    pub async fn finish_with<T>(&self, result_value: T) -> Result<T, WorkflowError>
    where
        T: Clone + Send + Sync + 'static,
    {
        let value = result_value;

        // Check if workflow exists and is running
        match self.runtime.get_workflow_status(&self.context.workflow_id) {
            Some(WorkflowStatus::Running) => {},
            Some(WorkflowStatus::Completed) => {
                self.finished.store(true, Ordering::SeqCst);
                return Ok(value); // Already completed
            },
            Some(_) => return Err(WorkflowError::ClusterError("Workflow is not running".to_string())),
            None => return Err(WorkflowError::NotFound(self.context.workflow_id.to_string())),
        }

        // Note: Results are now stored directly in WorkflowEnd commands via finish_workflow

        // Use the reusable leader/follower pattern to end the workflow
        let workflow_id_owned = self.context.workflow_id.clone();

        self.runtime.execute_leader_follower_operation(
            &self.context.workflow_id,
            &self.runtime.cluster,
            || async {
                // Leader operation: Propose WorkflowEnd command
                let command = WorkflowCommand::WorkflowEnd(WorkflowEndData {
                    workflow_id: workflow_id_owned.clone(),
                    result: Vec::new(),  // Empty result for workflows without explicit result
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

        // Final state verification - check for failure after completion
        match self.runtime.get_workflow_status(&self.context.workflow_id) {
            Some(WorkflowStatus::Completed) => {
                self.finished.store(true, Ordering::SeqCst);
                Ok(value)
            },
            Some(WorkflowStatus::Failed) => Err(WorkflowError::ClusterError("Workflow failed".to_string())),
            _ => Err(WorkflowError::ClusterError("Unexpected workflow state".to_string())),
        }
    }

    /// Wait for the workflow to complete and return the result
    /// This method waits for the workflow to finish naturally without manually ending it
    pub async fn wait_for_completion(&self) -> Result<(), WorkflowError> {
        // Check current status
        match self.runtime.get_workflow_status(&self.context.workflow_id) {
            Some(WorkflowStatus::Completed) => {
                self.finished.store(true, Ordering::SeqCst);
                return Ok(());
            },
            Some(WorkflowStatus::Failed) => {
                self.finished.store(true, Ordering::SeqCst);
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
                self.finished.store(true, Ordering::SeqCst);
                Ok(())
            },
            Some(WorkflowStatus::Failed) => {
                self.finished.store(true, Ordering::SeqCst);
                Err(WorkflowError::ClusterError("Workflow failed".to_string()))
            },
            _ => Err(WorkflowError::ClusterError("Unexpected workflow state".to_string())),
        }
    }

    /// End the workflow manually (for backward compatibility)
    pub async fn end(&self) -> Result<(), WorkflowError> {
        self.finish().await
    }
}

impl Drop for WorkflowRun {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::SeqCst) {
            // Panic if the workflow was not properly finished
            panic!(
                "WorkflowRun for '{}' was dropped without calling finish() or finish_with(). \
                 This indicates a programming error - workflows must be explicitly finished.",
                self.context.workflow_id
            );
        }
        // If finished = true, workflow was already properly ended, so it's safe to drop
    }
}

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
        // Subscribe to workflow events to get the result from the WorkflowEnd event
        let mut subscription = self.inner.runtime.subscribe_to_workflow(&self.inner.context.workflow_id);

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
        let workflow_result: Result<O, String> = serde_json::from_slice(&result_bytes)
            .map_err(|e| WorkflowError::SerializationError(e.to_string()))?;

        // Mark the inner WorkflowRun as finished to prevent panic on drop
        self.inner.finished.store(true, Ordering::SeqCst);

        // Return the final result or error
        workflow_result.map_err(|e| WorkflowError::ClusterError(e))
    }
}

/// Error types for workflow operations
#[derive(Debug, Clone)]
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
    /// Serialization/deserialization error
    SerializationError(String),
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
        }
    }
}

impl std::error::Error for WorkflowError {}



