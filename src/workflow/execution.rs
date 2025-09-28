use crate::raft::generic::{RaftCluster, RaftCommandType};
use crate::workflow::replicated_var::ReplicatedVar;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

impl RaftCommandType for WorkflowCommand {
    fn apply(&self, logger: &slog::Logger) -> Result<(), Box<dyn std::error::Error>> {
        let runtime = InternalWorkflowRuntime::instance();

        match self {
            WorkflowCommand::WorkflowStart { workflow_id } => {
                slog::info!(logger, "Started workflow"; "workflow_id" => workflow_id);
                runtime.set_workflow_status(workflow_id, WorkflowStatus::Running);
            },
            WorkflowCommand::WorkflowEnd { workflow_id } => {
                slog::info!(logger, "Ended workflow"; "workflow_id" => workflow_id);
                runtime.set_workflow_status(workflow_id, WorkflowStatus::Completed);
            },
            WorkflowCommand::SetCheckpoint { workflow_id, key, value } => {
                slog::info!(logger, "Set checkpoint";
                           "workflow_id" => workflow_id,
                           "key" => key,
                           "value_size" => value.len());
                runtime.set_checkpoint(workflow_id, key, value.clone());
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
    /// Store checkpoints: workflow_id -> (key -> value)
    pub checkpoints: HashMap<String, HashMap<String, Vec<u8>>>,
}

/// Internal workflow runtime (singleton for Raft apply operations)
pub(crate) struct InternalWorkflowRuntime {
    /// Internal state protected by mutex
    state: Arc<Mutex<WorkflowState>>,
    /// Event broadcaster for workflow state changes
    event_tx: broadcast::Sender<WorkflowEvent>,
}

/// User-facing workflow runtime with cluster reference
pub struct WorkflowRuntime {
    cluster: Arc<RaftCluster<WorkflowCommand>>,
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

impl InternalWorkflowRuntime {
    /// Create a new internal workflow runtime
    fn new() -> Self {
        let (event_tx, _) = broadcast::channel(1000);
        Self {
            state: Arc::new(Mutex::new(WorkflowState::default())),
            event_tx,
        }
    }

    /// Get the global internal workflow runtime instance
    pub(crate) fn instance() -> &'static InternalWorkflowRuntime {
        static INSTANCE: std::sync::OnceLock<InternalWorkflowRuntime> = std::sync::OnceLock::new();
        INSTANCE.get_or_init(InternalWorkflowRuntime::new)
    }

    /// Update workflow status and emit events
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

    /// Set a checkpoint and emit event
    fn set_checkpoint(&self, workflow_id: &str, key: &str, value: Vec<u8>) {
        {
            let mut state = self.state.lock().unwrap();
            let checkpoints = state.checkpoints
                .entry(workflow_id.to_string())
                .or_insert_with(HashMap::new);
            checkpoints.insert(key.to_string(), value.clone());
        }

        let event = WorkflowEvent::CheckpointSet {
            workflow_id: workflow_id.to_string(),
            key: key.to_string(),
            value: value,
        };
        let _ = self.event_tx.send(event);
    }

    /// Subscribe to workflow events for a specific workflow ID
    fn subscribe_to_workflow(&self, workflow_id: &str) -> WorkflowSubscription {
        WorkflowSubscription {
            receiver: self.event_tx.subscribe(),
            target_workflow_id: workflow_id.to_string(),
        }
    }

    /// Subscribe to workflow events (all workflows)
    fn subscribe(&self) -> broadcast::Receiver<WorkflowEvent> {
        self.event_tx.subscribe()
    }

    /// Get workflow status
    pub(crate) fn get_workflow_status(&self, workflow_id: &str) -> Option<WorkflowStatus> {
        let state = self.state.lock().unwrap();
        state.workflows.get(workflow_id).cloned()
    }

    /// Get a checkpoint value
    pub(crate) fn get_checkpoint(&self, workflow_id: &str, key: &str) -> Option<Vec<u8>> {
        let state = self.state.lock().unwrap();
        state.checkpoints
            .get(workflow_id)?
            .get(key)
            .cloned()
    }

    /// Clear all state (useful for testing)
    pub(crate) fn clear(&self) {
        let mut state = self.state.lock().unwrap();
        state.workflows.clear();
        state.checkpoints.clear();
    }
}

impl WorkflowRuntime {
    /// Create a new workflow runtime with a cluster reference
    pub fn new(cluster: Arc<RaftCluster<WorkflowCommand>>) -> Self {
        Self { cluster }
    }

    /// Start a new workflow and return a WorkflowRun instance
    pub async fn start(&self, workflow_id: &str) -> Result<WorkflowRun, WorkflowError> {
        WorkflowRun::start(workflow_id, self.cluster.clone()).await
    }

    /// Get workflow status (delegate to internal runtime)
    pub fn get_workflow_status(&self, workflow_id: &str) -> Option<WorkflowStatus> {
        InternalWorkflowRuntime::instance().get_workflow_status(workflow_id)
    }

    /// Execute a leader/follower operation with retry logic for leadership changes
    async fn execute_leader_follower_operation<F, Fut, E, T>(
        &self,
        workflow_id: &str,
        cluster: &RaftCluster<WorkflowCommand>,
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
            let subscription = InternalWorkflowRuntime::instance().subscribe_to_workflow(workflow_id);

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


    /// Start a workflow with the given ID
    ///
    /// This function implements different behavior for leaders and followers:
    /// - **Leader**: Proposes a WorkflowStart command to the cluster and waits for it to be applied
    /// - **Follower**: Waits for the WorkflowStart command to be applied by the leader
    ///
    /// # Arguments
    /// * `workflow_id` - Unique identifier for the workflow
    /// * `cluster` - Reference to the Raft cluster
    ///
    /// # Returns
    /// * `Ok(())` if the workflow was successfully started
    /// * `Err(WorkflowError)` if the operation failed
    pub async fn start_workflow(
        &self,
        workflow_id: &str,
        cluster: &RaftCluster<WorkflowCommand>
    ) -> Result<(), WorkflowError> {
        // Check if workflow already exists and is running
        if let Some(status) = self.get_workflow_status(workflow_id) {
            if status == WorkflowStatus::Running {
                return Err(WorkflowError::AlreadyExists(workflow_id.to_string()));
            }
        }

        // Check if already started after pre-check (race condition protection)
        let state_check = || async {
            if let Some(WorkflowStatus::Running) = self.get_workflow_status(workflow_id) {
                return Ok(());
            }
            Ok(())
        };

        // Use the reusable leader/follower pattern
        let workflow_id_owned = workflow_id.to_string();
        let cluster_ref = cluster;

        self.execute_leader_follower_operation(
            workflow_id,
            cluster_ref,
            || async {
                // Leader operation: Propose WorkflowStart command
                let command = WorkflowCommand::WorkflowStart {
                    workflow_id: workflow_id_owned.clone(),
                };

                cluster_ref.propose_and_sync(command).await
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

        // Final state verification after subscription
        state_check().await
    }

    /// End a workflow with the given ID
    pub async fn end_workflow(
        &self,
        workflow_id: &str,
        cluster: &RaftCluster<WorkflowCommand>
    ) -> Result<(), WorkflowError> {
        // Check if workflow exists and is running
        match self.get_workflow_status(workflow_id) {
            Some(WorkflowStatus::Running) => {},
            Some(WorkflowStatus::Completed) => return Ok(()), // Already completed
            Some(_) => return Err(WorkflowError::ClusterError("Workflow is not running".to_string())),
            None => return Err(WorkflowError::NotFound(workflow_id.to_string())),
        }

        // Use the reusable leader/follower pattern
        let workflow_id_owned = workflow_id.to_string();
        let cluster_ref = cluster;

        self.execute_leader_follower_operation(
            workflow_id,
            cluster_ref,
            || async {
                // Leader operation: Propose WorkflowEnd command
                let command = WorkflowCommand::WorkflowEnd {
                    workflow_id: workflow_id_owned.clone(),
                };

                cluster_ref.propose_and_sync(command).await
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
        match self.get_workflow_status(workflow_id) {
            Some(WorkflowStatus::Completed) => Ok(()),
            Some(WorkflowStatus::Failed) => Err(WorkflowError::ClusterError("Workflow failed".to_string())),
            _ => Err(WorkflowError::ClusterError("Unexpected workflow state".to_string())),
        }
    }

    /// Set a replicated variable value (used by ReplicatedVar)
    ///
    /// This method implements the leader/follower pattern for setting checkpoint values:
    /// - **Leader**: Proposes SetCheckpoint command and returns the value immediately
    /// - **Follower**: Waits indefinitely for the checkpoint to be applied by the leader and returns the deserialized value
    ///
    /// # Arguments
    /// * `workflow_id` - ID of the workflow
    /// * `key` - Key for the checkpoint
    /// * `value` - Typed value to store
    /// * `cluster` - Raft cluster reference
    ///
    /// # Returns
    /// * `Ok(T)` with the value if the checkpoint was successfully set
    /// * `Err(WorkflowError)` if the operation failed
    pub async fn set_replicated_var<T>(
        &self,
        workflow_id: &str,
        key: &str,
        value: T,
        cluster: &RaftCluster<WorkflowCommand>,
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

}

/// A scoped workflow run that must be explicitly finished
/// Shared workflow context that can be safely shared with ReplicatedVars
#[derive(Clone)]
pub struct WorkflowContext {
    pub workflow_id: String,
    pub cluster: Arc<RaftCluster<WorkflowCommand>>,
}

pub struct WorkflowRun {
    context: WorkflowContext,
    finished: bool,
}

impl WorkflowRun {
    /// Start a new workflow and return a WorkflowRun instance
    pub async fn start(
        workflow_id: &str,
        cluster: Arc<RaftCluster<WorkflowCommand>>
    ) -> Result<Self, WorkflowError> {
        start_workflow(workflow_id, &cluster).await?;
        let context = WorkflowContext {
            workflow_id: workflow_id.to_string(),
            cluster,
        };
        Ok(WorkflowRun {
            context,
            finished: false,
        })
    }

    /// Get the workflow ID
    pub fn workflow_id(&self) -> &str {
        &self.context.workflow_id
    }

    /// Get a reference to the cluster
    pub fn cluster(&self) -> &Arc<RaftCluster<WorkflowCommand>> {
        &self.context.cluster
    }

    /// Get the workflow context (for ReplicatedVar creation)
    pub fn context(&self) -> &WorkflowContext {
        &self.context
    }

    /// Finish the workflow without a result
    pub async fn finish(mut self) -> Result<(), WorkflowError> {
        end_workflow(&self.context.workflow_id, &self.context.cluster).await?;
        self.finished = true;
        Ok(())
    }

    /// Finish the workflow with a result from a ReplicatedVar
    pub async fn finish_with<T>(mut self, result: &ReplicatedVar<T>) -> Result<T, WorkflowError>
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de> + Clone + Send + Sync + 'static,
    {
        let value = result.get()
            .ok_or_else(|| WorkflowError::ClusterError("ReplicatedVar has no value to use as result".to_string()))?;

        end_workflow(&self.context.workflow_id, &self.context.cluster).await?;
        self.finished = true;

        Ok(value)
    }

    /// End the workflow manually (for backward compatibility)
    pub async fn end(mut self) -> Result<(), WorkflowError> {
        end_workflow(&self.context.workflow_id, &self.context.cluster).await?;
        self.finished = true;
        Ok(())
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

/// Start a workflow with the given ID
///
/// This function implements different behavior for leaders and followers:
/// - **Leader**: Proposes a WorkflowStart command to the cluster and waits for it to be applied
/// - **Follower**: Waits for the WorkflowStart command to be applied by the leader
///
/// # Arguments
/// * `workflow_id` - Unique identifier for the workflow
/// * `cluster` - Reference to the Raft cluster
///
/// # Returns
/// * `Ok(())` if the workflow was successfully started
/// * `Err(WorkflowError)` if the operation failed
pub async fn start_workflow(
    workflow_id: &str,
    cluster: &RaftCluster<WorkflowCommand>
) -> Result<(), WorkflowError> {
    let runtime = WorkflowRuntime::new(Arc::new(cluster.clone()));
    runtime.start_workflow(workflow_id, cluster).await
}

/// End a workflow with the given ID
pub async fn end_workflow(
    workflow_id: &str,
    cluster: &RaftCluster<WorkflowCommand>
) -> Result<(), WorkflowError> {
    let runtime = WorkflowRuntime::new(Arc::new(cluster.clone()));
    runtime.end_workflow(workflow_id, cluster).await
}

/// Get the status of a workflow
pub fn get_workflow_status(workflow_id: &str) -> Option<WorkflowStatus> {
    InternalWorkflowRuntime::instance().get_workflow_status(workflow_id)
}

/// Get a checkpoint value for a workflow
pub fn get_workflow_checkpoint(workflow_id: &str, key: &str) -> Option<Vec<u8>> {
    InternalWorkflowRuntime::instance().get_checkpoint(workflow_id, key)
}

/// Clear all workflow state (useful for testing)
pub fn clear_workflow_state() {
    InternalWorkflowRuntime::instance().clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::generic::RaftCluster;

    #[tokio::test]
    async fn test_start_workflow_single_node() {
        clear_workflow_state();

        let cluster = RaftCluster::<WorkflowCommand>::new_single_node(1).await
            .expect("Failed to create single node cluster");

        // Wait for leadership establishment
        tokio::time::sleep(Duration::from_millis(500)).await;

        let workflow_id = "test_workflow_1";

        // Start the workflow
        let result = start_workflow(workflow_id, &cluster).await;
        assert!(result.is_ok(), "Failed to start workflow: {:?}", result);

        // Verify workflow is running
        assert_eq!(get_workflow_status(workflow_id), Some(WorkflowStatus::Running));

        // Try to start the same workflow again (should fail)
        let result = start_workflow(workflow_id, &cluster).await;
        assert!(matches!(result, Err(WorkflowError::AlreadyExists(_))));
    }

    #[tokio::test]
    async fn test_end_workflow_single_node() {
        clear_workflow_state();

        let cluster = RaftCluster::<WorkflowCommand>::new_single_node(1).await
            .expect("Failed to create single node cluster");

        // Wait for leadership establishment
        tokio::time::sleep(Duration::from_millis(500)).await;

        let workflow_id = "test_workflow_2";

        // Start the workflow
        start_workflow(workflow_id, &cluster).await
            .expect("Failed to start workflow");

        // End the workflow
        let result = end_workflow(workflow_id, &cluster).await;
        assert!(result.is_ok(), "Failed to end workflow: {:?}", result);

        // Verify workflow is completed
        assert_eq!(get_workflow_status(workflow_id), Some(WorkflowStatus::Completed));
    }

    #[tokio::test]
    async fn test_workflow_lifecycle() {
        clear_workflow_state();

        let cluster = RaftCluster::<WorkflowCommand>::new_single_node(1).await
            .expect("Failed to create single node cluster");

        // Wait for leadership establishment
        tokio::time::sleep(Duration::from_millis(500)).await;

        let workflow_id = "test_workflow_3";

        // Initially, workflow should not exist
        assert_eq!(get_workflow_status(workflow_id), None);

        // Start workflow
        start_workflow(workflow_id, &cluster).await
            .expect("Failed to start workflow");
        assert_eq!(get_workflow_status(workflow_id), Some(WorkflowStatus::Running));

        // End workflow
        end_workflow(workflow_id, &cluster).await
            .expect("Failed to end workflow");
        assert_eq!(get_workflow_status(workflow_id), Some(WorkflowStatus::Completed));
    }
}