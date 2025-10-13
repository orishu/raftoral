use crate::raft::generic::RaftCluster;
use crate::workflow::commands::*;
use crate::workflow::error::*;
use crate::workflow::executor::{WorkflowCommandExecutor, WorkflowEvent, make_queue_key};
use crate::workflow::context::{WorkflowContext, WorkflowRun, TypedWorkflowRun};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::timeout;

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
            WorkflowEvent::Started { workflow_id } => workflow_id == &self.target_workflow_id,
            WorkflowEvent::Completed { workflow_id, .. } => workflow_id == &self.target_workflow_id,
            WorkflowEvent::Failed { workflow_id, .. } => workflow_id == &self.target_workflow_id,
            WorkflowEvent::CheckpointSet { workflow_id, .. } => workflow_id == &self.target_workflow_id,
            WorkflowEvent::OwnershipChanged { workflow_id, .. } => workflow_id == &self.target_workflow_id,
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
    /// let transport = Arc::new(InMemoryClusterTransport::<WorkflowCommandExecutor>::new(vec![1, 2, 3]));
    /// transport.start().await?;
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

    /// Create a new workflow runtime and cluster with in-memory transport
    pub async fn new_single_node(node_id: u64) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        use crate::raft::generic::transport::{InMemoryClusterTransport, ClusterTransport};

        // Create in-memory transport for single node
        let transport = Arc::new(InMemoryClusterTransport::<WorkflowCommandExecutor>::new(vec![node_id]));
        transport.start().await?;

        // Create cluster via transport
        let cluster = transport.create_cluster(node_id).await?;

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
    pub(crate) fn subscribe_to_workflow(&self, workflow_id: &str) -> WorkflowSubscription {
        self.cluster.executor.subscribe_to_workflow(workflow_id)
    }

    /// Start a new workflow and return a WorkflowRun instance
    pub async fn start<T>(self: &Arc<Self>, workflow_id: &str, input: T) -> Result<Arc<WorkflowRun>, WorkflowError>
    where
        T: serde::Serialize + Send + 'static,
    {
        WorkflowRun::start(workflow_id, self, input).await
    }

    /// Execute an operation based on workflow ownership with retry logic for ownership changes
    ///
    /// If this node owns the workflow, execute the operation immediately.
    /// If not, wait for the owner to execute and propagate the result via events.
    /// Handles ownership changes (e.g., due to failover) by re-checking ownership on OwnershipChanged events.
    pub(crate) async fn execute_owner_or_wait<F, Fut, E, T>(
        &self,
        workflow_id: &str,
        cluster: &RaftCluster<WorkflowCommandExecutor>,
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
            let is_owner = self.is_workflow_owner(workflow_id, cluster).await;

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
                        if self.is_workflow_owner(workflow_id, cluster).await {
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
                WorkflowEvent::Started { workflow_id } => workflow_id,
                WorkflowEvent::Completed { workflow_id, .. } => workflow_id,
                WorkflowEvent::Failed { workflow_id, .. } => workflow_id,
                WorkflowEvent::CheckpointSet { workflow_id, .. } => workflow_id,
                WorkflowEvent::OwnershipChanged { workflow_id, .. } => workflow_id,
            };

            if event_workflow_id != &workflow_id {
                return None; // Not for our workflow
            }

            // Check if this is an ownership change event
            if matches!(event, WorkflowEvent::OwnershipChanged { .. }) {
                // Signal that we should re-check ownership by returning a special marker
                // We'll use Option<Option<T>> internally: Some(None) = ownership changed
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

    /// Check if this node owns the given workflow
    async fn is_workflow_owner(
        &self,
        workflow_id: &str,
        cluster: &RaftCluster<WorkflowCommandExecutor>,
    ) -> bool {
        // Check ownership map to see if we're the owner
        if let Some(owner_node_id) = cluster.executor.ownership_map.get_owner(workflow_id) {
            if let Some(our_node_id) = cluster.executor.get_node_id() {
                return owner_node_id == our_node_id;
            }
        }
        false
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
        let queue_key = make_queue_key(workflow_id, key);
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
        // Leaders clean up their own proposed values from the queue (see line 340-346 below)
        if let Some(queued_value) = self.try_dequeue_checkpoint(workflow_id, key, cluster)? {
            return Ok(queued_value);
        }

        // No queued value - proceed with normal consensus-based flow
        // Serialize the value
        let serialized_value = serde_json::to_vec(&value)
            .map_err(|e| WorkflowError::ClusterError(format!("Serialization error: {}", e)))?;

        // Use the reusable owner/wait pattern with return value
        self.execute_owner_or_wait(
            workflow_id,
            cluster,
            || {
                let value_clone = value.clone();
                let serialized_value_clone = serialized_value.clone();
                let workflow_id_owned_clone = workflow_id_owned.clone();
                let key_owned_clone = key_owned.clone();
                async move {
                    // Owner operation: Propose SetCheckpoint command and return the value
                    let command = WorkflowCommand::SetCheckpoint(CheckpointData {
                        workflow_id: workflow_id_owned_clone.clone(),
                        key: key_owned_clone.clone(),
                        value: serialized_value_clone,
                    });

                    cluster.propose_and_sync(command).await
                        .map_err(|e| WorkflowError::ClusterError(e.to_string()))?;

                    // Pop the value we just enqueued (during apply in propose_and_sync)
                    // This prevents leader's own execution from consuming its proposed values
                    let queue_key = make_queue_key(&workflow_id_owned_clone, &key_owned_clone);
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

        // Propose WorkflowStart command with proposer as owner
        // The proposing node becomes the owner and will execute the workflow
        let command = WorkflowCommand::WorkflowStart(WorkflowStartData {
            workflow_id: workflow_id.clone(),
            workflow_type: workflow_type.to_string(),
            version,
            input: input_bytes.clone(),
            owner_node_id: self.cluster.node_id(),
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
