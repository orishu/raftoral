use crate::raft::generic::RaftCluster;
use crate::raft::generic::message::CommandExecutor;
use crate::workflow::registry::WorkflowRegistry;
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::timeout;

/// Commands for workflow lifecycle management
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WorkflowCommand {
    /// Start a new workflow with the given ID
    WorkflowStart { workflow_id: String },
    /// End a workflow with the given ID
    WorkflowEnd { workflow_id: String },
    /// Set a replicated checkpoint for a workflow
    SetCheckpoint {
        workflow_id: String,
        key: String,
        value: Vec<u8>
    },
}


/// Command executor for workflow commands with embedded state
pub struct WorkflowCommandExecutor {
    /// Internal state protected by mutex
    state: Arc<Mutex<WorkflowState>>,
    /// Event broadcaster for workflow state changes
    event_tx: broadcast::Sender<WorkflowEvent>,
}

impl WorkflowCommandExecutor {
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(1000);
        Self {
            state: Arc::new(Mutex::new(WorkflowState::default())),
            event_tx,
        }
    }

    fn set_workflow_status(&self, workflow_id: &str, status: WorkflowStatus) {
        {
            let mut state = self.state.lock().unwrap();
            state.workflows.insert(workflow_id.to_string(), status.clone());
        }

        // Emit appropriate event
        let event = match status {
            WorkflowStatus::Running => WorkflowEvent::Started { workflow_id: workflow_id.to_string() },
            WorkflowStatus::Completed => WorkflowEvent::Completed { workflow_id: workflow_id.to_string() },
            WorkflowStatus::Failed => WorkflowEvent::Failed { workflow_id: workflow_id.to_string() },
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
            WorkflowCommand::WorkflowStart { workflow_id } => {
                slog::info!(logger, "Started workflow"; "workflow_id" => workflow_id);
                self.set_workflow_status(workflow_id, WorkflowStatus::Running);
            },
            WorkflowCommand::WorkflowEnd { workflow_id } => {
                slog::info!(logger, "Ended workflow"; "workflow_id" => workflow_id);
                self.set_workflow_status(workflow_id, WorkflowStatus::Completed);
            },
            WorkflowCommand::SetCheckpoint { workflow_id, key, value } => {
                slog::info!(logger, "Set checkpoint";
                           "workflow_id" => workflow_id,
                           "key" => key,
                           "value_size" => value.len());

                // Emit event for subscribers (followers waiting for this checkpoint)
                self.emit_checkpoint_event(workflow_id, key, value.clone());
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
    /// A workflow has started
    Started { workflow_id: String },
    /// A workflow has completed
    Completed { workflow_id: String },
    /// A workflow has failed
    Failed { workflow_id: String },
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
    registry: Arc<Mutex<WorkflowRegistry>>,
    /// In-memory store for workflow results (not replicated via Raft)
    results: Arc<Mutex<HashMap<String, Box<dyn Any + Send + Sync>>>>,
}

impl std::fmt::Debug for WorkflowRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowRuntime")
            .field("cluster", &"Arc<RaftCluster<WorkflowCommandExecutor>>")
            .field("registry", &"Arc<Mutex<WorkflowRegistry>>")
            .field("results", &"Arc<Mutex<HashMap<String, Box<dyn Any + Send + Sync>>>>")
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
            WorkflowEvent::Completed { workflow_id } => workflow_id == &self.target_workflow_id,
            WorkflowEvent::Failed { workflow_id } => workflow_id == &self.target_workflow_id,
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
            cluster,
            registry: Arc::new(Mutex::new(WorkflowRegistry::new())),
            results: Arc::new(Mutex::new(HashMap::new())),
        });

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
    pub async fn start(self: &Arc<Self>, workflow_id: &str) -> Result<WorkflowRun, WorkflowError> {
        WorkflowRun::start(workflow_id, self).await
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
                    let command = WorkflowCommand::SetCheckpoint {
                        workflow_id: workflow_id_owned_clone,
                        key: key_owned_clone,
                        value: serialized_value_clone,
                    };

                    cluster.propose_and_sync(command).await
                        .map_err(|e| WorkflowError::ClusterError(e.to_string()))?;
                    Ok(value_clone)
                }
            },
            |event| {
                if let WorkflowEvent::CheckpointSet { workflow_id: wid, key: k, value: v } = event {
                    if wid == &workflow_id_owned && k == &key_owned {
                        // Deserialize and return the value
                        if let Ok(deserialized) = serde_json::from_slice::<T>(v) {
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
        let mut registry = self.registry.lock().unwrap();
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
        let mut registry = self.registry.lock().unwrap();
        registry.register_closure(workflow_type, version, function)
    }

    /// Start a workflow by type and version instead of arbitrary ID
    ///
    /// This method looks up the registered workflow function and executes it with the given input.
    /// It will generate a unique workflow ID internally.
    ///
    /// # Arguments
    /// * `workflow_type` - The registered workflow type
    /// * `version` - The workflow version
    /// * `input` - Serializable input for the workflow function
    ///
    /// # Returns
    /// * `Ok(WorkflowRun)` if the workflow was started successfully
    /// * `Err(WorkflowError)` if the workflow type is not registered or startup failed
    pub async fn start_workflow_typed<I, O>(
        self: &Arc<Self>,
        workflow_type: &str,
        version: u32,
        input: I,
    ) -> Result<TypedWorkflowRun<O>, WorkflowError>
    where
        I: Send + 'static,
        O: Clone + Send + Sync + 'static,
    {
        // Look up the workflow function
        let workflow_function = {
            let registry = self.registry.lock().unwrap();
            registry.get(workflow_type, version)
                .ok_or_else(|| WorkflowError::NotFound(format!("Workflow '{}' version {} not found", workflow_type, version)))?
        };

        // Generate a unique workflow ID
        let workflow_id = format!("{}_v{}_{}",
                                workflow_type,
                                version,
                                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis());

        // Start the workflow with the generated ID
        let workflow_run = self.start(&workflow_id).await?;

        // Create context for workflow execution
        let context = WorkflowContext {
            workflow_id: workflow_id.clone(),
            cluster: self.cluster.clone(),
        };

        // Execute the workflow function in the background
        let runtime_clone = self.clone();
        let input_any: Box<dyn std::any::Any + Send> = Box::new(input);

        tokio::spawn(async move {
            // Create a WorkflowRun for the background task to use for finishing
            let bg_workflow_run = WorkflowRun::for_existing(&workflow_id, runtime_clone);

            // Execute the workflow function
            match workflow_function.execute(input_any, context).await {
                Ok(output_any) => {
                    // Downcast the output to the expected type
                    if let Ok(output) = output_any.downcast::<O>() {
                        // Finish the workflow with the result using finish_with
                        if let Err(e) = bg_workflow_run.finish_with(*output).await {
                            eprintln!("Failed to finish workflow {} with result: {}", workflow_id, e);
                        }
                    } else {
                        eprintln!("Failed to downcast workflow output for {}", workflow_id);
                        // Try to finish anyway
                        if let Err(e) = bg_workflow_run.finish().await {
                            eprintln!("Failed to finish failed workflow {}: {}", workflow_id, e);
                        }
                    }
                },
                Err(e) => {
                    eprintln!("Workflow {} failed: {}", workflow_id, e);
                    // Try to finish the workflow anyway
                    if let Err(e) = bg_workflow_run.finish().await {
                        eprintln!("Failed to finish failed workflow {}: {}", workflow_id, e);
                    }
                }
            }
        });

        Ok(TypedWorkflowRun::new(workflow_run))
    }

    /// Check if a workflow type and version is registered
    pub fn has_workflow(&self, workflow_type: &str, version: u32) -> bool {
        let registry = self.registry.lock().unwrap();
        registry.contains(workflow_type, version)
    }

    /// List all registered workflow types and versions
    pub fn list_workflows(&self) -> Vec<(String, u32)> {
        let registry = self.registry.lock().unwrap();
        registry.list_workflows()
    }

    /// Create a temporary runtime for internal operations (used by ReplicatedVar)
    /// This is an internal method and should not be used by external code
    #[doc(hidden)]
    pub fn _create_temporary(cluster: Arc<RaftCluster<WorkflowCommandExecutor>>) -> Self {
        WorkflowRuntime {
            cluster,
            registry: Arc::new(Mutex::new(WorkflowRegistry::new())),
            results: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Store a workflow result (internal use)
    pub(crate) fn store_result<T>(&self, workflow_id: &str, result: T)
    where
        T: Send + Sync + 'static,
    {
        let mut results = self.results.lock().unwrap();
        results.insert(workflow_id.to_string(), Box::new(result));
    }

    /// Retrieve a workflow result (internal use)
    pub(crate) fn get_result<T>(&self, workflow_id: &str) -> Option<T>
    where
        T: Clone + Send + Sync + 'static,
    {
        let mut results = self.results.lock().unwrap();
        if let Some(boxed_result) = results.remove(workflow_id) {
            boxed_result.downcast::<T>().ok().map(|boxed| *boxed)
        } else {
            None
        }
    }
}


/// A scoped workflow run that must be explicitly finished
/// Shared workflow context that can be safely shared with ReplicatedVars
#[derive(Clone)]
pub struct WorkflowContext {
    pub workflow_id: String,
    pub cluster: Arc<RaftCluster<WorkflowCommandExecutor>>,
}

impl std::fmt::Debug for WorkflowContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowContext")
            .field("workflow_id", &self.workflow_id)
            .field("cluster", &"<RaftCluster>")
            .finish()
    }
}

#[derive(Debug)]
pub struct WorkflowRun {
    context: WorkflowContext,
    runtime: Arc<WorkflowRuntime>,
    finished: bool,
}

impl WorkflowRun {
    /// Create a WorkflowRun for an existing workflow (internal use)
    pub fn for_existing(
        workflow_id: &str,
        runtime: Arc<WorkflowRuntime>
    ) -> Self {
        let context = WorkflowContext {
            workflow_id: workflow_id.to_string(),
            cluster: runtime.cluster.clone(),
        };
        WorkflowRun {
            context,
            runtime,
            finished: false,
        }
    }

    /// Start a new workflow and return a WorkflowRun instance
    pub async fn start(
        workflow_id: &str,
        runtime: &Arc<WorkflowRuntime>
    ) -> Result<Self, WorkflowError> {
        let cluster = runtime.cluster.clone();

        // Check if workflow already exists and is running
        if let Some(status) = runtime.get_workflow_status(workflow_id) {
            if status == WorkflowStatus::Running {
                return Err(WorkflowError::AlreadyExists(workflow_id.to_string()));
            }
        }

        // Use the reusable leader/follower pattern to start the workflow
        let workflow_id_owned = workflow_id.to_string();

        runtime.execute_leader_follower_operation(
            workflow_id,
            &cluster,
            || async {
                // Leader operation: Propose WorkflowStart command
                let command = WorkflowCommand::WorkflowStart {
                    workflow_id: workflow_id_owned.clone(),
                };

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
            cluster,
        };
        Ok(WorkflowRun {
            context,
            runtime: runtime.clone(),
            finished: false,
        })
    }

    /// Get the workflow ID
    pub fn workflow_id(&self) -> &str {
        &self.context.workflow_id
    }

    /// Get a reference to the cluster
    pub fn cluster(&self) -> &Arc<RaftCluster<WorkflowCommandExecutor>> {
        &self.context.cluster
    }

    /// Get the workflow context (for ReplicatedVar creation)
    pub fn context(&self) -> &WorkflowContext {
        &self.context
    }

    /// Finish the workflow without a result
    pub async fn finish(mut self) -> Result<(), WorkflowError> {
        // Check if workflow exists and is running
        match self.runtime.get_workflow_status(&self.context.workflow_id) {
            Some(WorkflowStatus::Running) => {},
            Some(WorkflowStatus::Completed) => {
                self.finished = true;
                return Ok(()); // Already completed
            },
            Some(_) => return Err(WorkflowError::ClusterError("Workflow is not running".to_string())),
            None => return Err(WorkflowError::NotFound(self.context.workflow_id.to_string())),
        }

        // Use the reusable leader/follower pattern to end the workflow
        let workflow_id_owned = self.context.workflow_id.clone();

        self.runtime.execute_leader_follower_operation(
            &self.context.workflow_id,
            &self.context.cluster,
            || async {
                // Leader operation: Propose WorkflowEnd command
                let command = WorkflowCommand::WorkflowEnd {
                    workflow_id: workflow_id_owned.clone(),
                };

                self.context.cluster.propose_and_sync(command).await
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
                self.finished = true;
                Ok(())
            },
            Some(WorkflowStatus::Failed) => Err(WorkflowError::ClusterError("Workflow failed".to_string())),
            _ => Err(WorkflowError::ClusterError("Unexpected workflow state".to_string())),
        }
    }

    /// Finish the workflow with a result value
    pub async fn finish_with<T>(mut self, result_value: T) -> Result<T, WorkflowError>
    where
        T: Clone + Send + Sync + 'static,
    {
        let value = result_value;

        // Check if workflow exists and is running
        match self.runtime.get_workflow_status(&self.context.workflow_id) {
            Some(WorkflowStatus::Running) => {},
            Some(WorkflowStatus::Completed) => {
                self.finished = true;
                return Ok(value); // Already completed
            },
            Some(_) => return Err(WorkflowError::ClusterError("Workflow is not running".to_string())),
            None => return Err(WorkflowError::NotFound(self.context.workflow_id.to_string())),
        }

        // Store the result for later retrieval
        self.runtime.store_result(&self.context.workflow_id, value.clone());

        // Use the reusable leader/follower pattern to end the workflow
        let workflow_id_owned = self.context.workflow_id.clone();

        self.runtime.execute_leader_follower_operation(
            &self.context.workflow_id,
            &self.context.cluster,
            || async {
                // Leader operation: Propose WorkflowEnd command
                let command = WorkflowCommand::WorkflowEnd {
                    workflow_id: workflow_id_owned.clone(),
                };

                self.context.cluster.propose_and_sync(command).await
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
                self.finished = true;
                Ok(value)
            },
            Some(WorkflowStatus::Failed) => Err(WorkflowError::ClusterError("Workflow failed".to_string())),
            _ => Err(WorkflowError::ClusterError("Unexpected workflow state".to_string())),
        }
    }

    /// Wait for the workflow to complete and return the result
    /// This method waits for the workflow to finish naturally without manually ending it
    pub async fn wait_for_completion(&mut self) -> Result<(), WorkflowError> {
        // Check current status
        match self.runtime.get_workflow_status(&self.context.workflow_id) {
            Some(WorkflowStatus::Completed) => {
                self.finished = true;
                return Ok(());
            },
            Some(WorkflowStatus::Failed) => {
                self.finished = true;
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
            &self.context.cluster,
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
                self.finished = true;
                Ok(())
            },
            Some(WorkflowStatus::Failed) => {
                self.finished = true;
                Err(WorkflowError::ClusterError("Workflow failed".to_string()))
            },
            _ => Err(WorkflowError::ClusterError("Unexpected workflow state".to_string())),
        }
    }

    /// End the workflow manually (for backward compatibility)
    pub async fn end(self) -> Result<(), WorkflowError> {
        self.finish().await
    }
}

impl Drop for WorkflowRun {
    fn drop(&mut self) {
        if !self.finished {
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
    inner: WorkflowRun,
    _phantom: PhantomData<O>,
}

impl<O> TypedWorkflowRun<O>
where
    O: Clone + Send + Sync + 'static,
{
    /// Create a new TypedWorkflowRun from a WorkflowRun
    pub(crate) fn new(inner: WorkflowRun) -> Self {
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
    pub async fn wait_for_completion(mut self) -> Result<O, WorkflowError> {
        // First wait for the workflow to complete using the inner wait_for_completion
        self.inner.wait_for_completion().await?;

        // Now retrieve the result from the runtime's result store
        match self.inner.runtime.get_result::<O>(&self.inner.context.workflow_id) {
            Some(result) => Ok(result),
            None => Err(WorkflowError::ClusterError("Workflow result not found".to_string())),
        }
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
}

impl std::fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkflowError::AlreadyExists(id) => write!(f, "Workflow '{}' already exists", id),
            WorkflowError::NotFound(id) => write!(f, "Workflow '{}' not found", id),
            WorkflowError::NotLeader => write!(f, "Not the leader - cannot start workflows"),
            WorkflowError::ClusterError(msg) => write!(f, "Cluster error: {}", msg),
            WorkflowError::Timeout => write!(f, "Timeout waiting for workflow to start"),
        }
    }
}

impl std::error::Error for WorkflowError {}



#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_workflow_runtime_start_workflow() {
        let workflow_runtime = WorkflowRuntime::new_single_node(1).await
            .expect("Failed to create workflow runtime");

        // Wait for leadership establishment
        tokio::time::sleep(Duration::from_millis(500)).await;

        let workflow_id = "test_workflow_1";

        // Start the workflow
        let workflow_run = workflow_runtime.start(workflow_id).await;
        assert!(workflow_run.is_ok(), "Failed to start workflow: {:?}", workflow_run);

        let workflow_run = workflow_run.unwrap();

        // Verify workflow is running
        assert_eq!(
            workflow_runtime.get_workflow_status(workflow_id),
            Some(WorkflowStatus::Running)
        );

        // Verify workflow ID is correct
        assert_eq!(workflow_run.workflow_id(), workflow_id);

        // Finish the workflow
        workflow_run.finish().await.expect("Failed to finish workflow");

        // Verify workflow is completed
        assert_eq!(
            workflow_runtime.get_workflow_status(workflow_id),
            Some(WorkflowStatus::Completed)
        );
    }

    #[tokio::test]
    async fn test_workflow_runtime_duplicate_start() {
        let workflow_runtime = WorkflowRuntime::new_single_node(1).await
            .expect("Failed to create workflow runtime");

        // Wait for leadership establishment
        tokio::time::sleep(Duration::from_millis(500)).await;

        let workflow_id = "test_workflow_duplicate";

        // Start the workflow
        let workflow_run1 = workflow_runtime.start(workflow_id).await
            .expect("Failed to start first workflow");

        // Try to start the same workflow again (should fail)
        let result = workflow_runtime.start(workflow_id).await;
        assert!(matches!(result, Err(WorkflowError::AlreadyExists(_))),
                "Expected AlreadyExists error, got: {:?}", result);

        // Finish the first workflow
        workflow_run1.finish().await.expect("Failed to finish workflow");
    }

    #[tokio::test]
    async fn test_workflow_runtime_lifecycle() {
        let workflow_runtime = WorkflowRuntime::new_single_node(1).await
            .expect("Failed to create workflow runtime");

        // Wait for leadership establishment
        tokio::time::sleep(Duration::from_millis(500)).await;

        let workflow_id = "test_workflow_lifecycle";

        // Initially, workflow should not exist
        assert_eq!(workflow_runtime.get_workflow_status(workflow_id), None);

        // Start workflow
        let workflow_run = workflow_runtime.start(workflow_id).await
            .expect("Failed to start workflow");
        assert_eq!(
            workflow_runtime.get_workflow_status(workflow_id),
            Some(WorkflowStatus::Running)
        );

        // End workflow
        workflow_run.finish().await.expect("Failed to finish workflow");
        assert_eq!(
            workflow_runtime.get_workflow_status(workflow_id),
            Some(WorkflowStatus::Completed)
        );
    }

    #[tokio::test]
    async fn test_workflow_runtime_multiple_workflows() {
        let workflow_runtime = WorkflowRuntime::new_single_node(1).await
            .expect("Failed to create workflow runtime");

        // Wait for leadership establishment
        tokio::time::sleep(Duration::from_millis(500)).await;

        let workflow_id1 = "test_workflow_multi_1";
        let workflow_id2 = "test_workflow_multi_2";

        // Start both workflows
        let workflow_run1 = workflow_runtime.start(workflow_id1).await
            .expect("Failed to start workflow 1");
        let workflow_run2 = workflow_runtime.start(workflow_id2).await
            .expect("Failed to start workflow 2");

        // Both should be running
        assert_eq!(
            workflow_runtime.get_workflow_status(workflow_id1),
            Some(WorkflowStatus::Running)
        );
        assert_eq!(
            workflow_runtime.get_workflow_status(workflow_id2),
            Some(WorkflowStatus::Running)
        );

        // Finish first workflow
        workflow_run1.finish().await.expect("Failed to finish workflow 1");
        assert_eq!(
            workflow_runtime.get_workflow_status(workflow_id1),
            Some(WorkflowStatus::Completed)
        );
        assert_eq!(
            workflow_runtime.get_workflow_status(workflow_id2),
            Some(WorkflowStatus::Running)
        );

        // Finish second workflow
        workflow_run2.finish().await.expect("Failed to finish workflow 2");
        assert_eq!(
            workflow_runtime.get_workflow_status(workflow_id2),
            Some(WorkflowStatus::Completed)
        );
    }

    #[tokio::test]
    async fn test_workflow_runtime_finish_with_value() {
        let workflow_runtime = WorkflowRuntime::new_single_node(1).await
            .expect("Failed to create workflow runtime");

        // Wait for leadership establishment
        tokio::time::sleep(Duration::from_millis(500)).await;

        let workflow_id = "test_workflow_finish_with";

        // Start workflow
        let workflow_run = workflow_runtime.start(workflow_id).await
            .expect("Failed to start workflow");

        // Finish with a result value
        let result_value = 42i32;
        let returned_value = workflow_run.finish_with(result_value).await
            .expect("Failed to finish workflow with value");

        assert_eq!(returned_value, result_value);
        assert_eq!(
            workflow_runtime.get_workflow_status(workflow_id),
            Some(WorkflowStatus::Completed)
        );
    }

    #[tokio::test]
    async fn test_workflow_registry_integration() {
        use serde::{Serialize, Deserialize};

        #[derive(Serialize, Deserialize)]
        struct FibonacciInput {
            n: u32,
        }

        #[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
        struct FibonacciOutput {
            result: u32,
        }

        // Create workflow runtime
        let workflow_runtime = WorkflowRuntime::new_single_node(1).await
            .expect("Failed to create workflow runtime");

        // Wait for leadership establishment
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Test workflow registration using the new closure-based approach
        let registration_result = workflow_runtime.register_workflow_closure(
            "fibonacci",
            1,
            |input: FibonacciInput, _context: WorkflowContext| async move {
                let mut a = 0;
                let mut b = 1;
                for _ in 2..=input.n {
                    let temp = a + b;
                    a = b;
                    b = temp;
                }
                Ok(FibonacciOutput { result: b })
            }
        );
        assert!(registration_result.is_ok());

        // Test duplicate registration fails
        let duplicate_result = workflow_runtime.register_workflow_closure(
            "fibonacci",
            1,
            |input: FibonacciInput, _context: WorkflowContext| async move {
                Ok(FibonacciOutput { result: input.n })
            }
        );
        assert!(duplicate_result.is_err());

        // Test has_workflow
        assert!(workflow_runtime.has_workflow("fibonacci", 1));
        assert!(!workflow_runtime.has_workflow("fibonacci", 2));
        assert!(!workflow_runtime.has_workflow("missing", 1));

        // Test list_workflows
        let workflows = workflow_runtime.list_workflows();
        assert_eq!(workflows.len(), 1);
        assert!(workflows.contains(&("fibonacci".to_string(), 1)));

        // Test starting a typed workflow - this now actually executes the workflow function
        let input = FibonacciInput { n: 10 };
        let workflow_run = workflow_runtime.start_workflow_typed::<FibonacciInput, FibonacciOutput>("fibonacci", 1, input).await
            .expect("Failed to start typed workflow");

        // Verify the workflow was started (it has a generated ID)
        assert!(workflow_run.workflow_id().starts_with("fibonacci_v1_"));

        // Give the background task a moment to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Wait for the workflow to complete and get the result
        let result = workflow_run.wait_for_completion().await.expect("Failed to wait for workflow completion");

        // Verify the result is correct (10th Fibonacci number)
        assert_eq!(result.result, 55);

        println!("✅ Workflow registry integration test completed successfully!");
    }
}