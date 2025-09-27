use crate::raft::generic::{RaftCluster, RaftCommandType};
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
        let runtime = WorkflowRuntime::instance();

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
    CheckpointSet { workflow_id: String, key: String },
}

/// Workflow state for tracking workflows across the cluster
#[derive(Default)]
pub struct WorkflowState {
    /// Track workflow status by ID
    pub workflows: HashMap<String, WorkflowStatus>,
    /// Store checkpoints: workflow_id -> (key -> value)
    pub checkpoints: HashMap<String, HashMap<String, Vec<u8>>>,
}

/// Event-driven workflow runtime singleton
pub struct WorkflowRuntime {
    /// Internal state protected by mutex
    state: Arc<Mutex<WorkflowState>>,
    /// Event broadcaster for workflow state changes
    event_tx: broadcast::Sender<WorkflowEvent>,
}

/// Subscription helper for workflow-specific events
pub struct WorkflowSubscription {
    receiver: broadcast::Receiver<WorkflowEvent>,
    target_workflow_id: String,
}

impl WorkflowSubscription {
    /// Wait for a specific event for this workflow
    pub async fn wait_for_event<F>(&mut self, event_check: F, timeout_duration: Option<Duration>) -> Result<(), WorkflowError>
    where
        F: Fn(&WorkflowEvent) -> bool,
    {
        let wait_future = async {
            loop {
                match self.receiver.recv().await {
                    Ok(event) => {
                        // Check if this event is for our workflow and matches the condition
                        if self.matches_workflow(&event) && event_check(&event) {
                            return Ok(());
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

        // Apply timeout - use provided duration or default to 10 seconds
        let timeout_duration = timeout_duration.unwrap_or(Duration::from_secs(10));
        timeout(timeout_duration, wait_future)
            .await
            .map_err(|_| WorkflowError::Timeout)?
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
    /// Create a new workflow runtime
    fn new() -> Self {
        let (event_tx, _) = broadcast::channel(1000);
        Self {
            state: Arc::new(Mutex::new(WorkflowState::default())),
            event_tx,
        }
    }

    /// Get the global workflow runtime instance
    pub fn instance() -> &'static WorkflowRuntime {
        static INSTANCE: std::sync::OnceLock<WorkflowRuntime> = std::sync::OnceLock::new();
        INSTANCE.get_or_init(WorkflowRuntime::new)
    }

    /// Subscribe to workflow events for a specific workflow ID
    pub fn subscribe_to_workflow(&self, workflow_id: &str) -> WorkflowSubscription {
        WorkflowSubscription {
            receiver: self.event_tx.subscribe(),
            target_workflow_id: workflow_id.to_string(),
        }
    }

    /// Subscribe to workflow events (all workflows)
    pub fn subscribe(&self) -> broadcast::Receiver<WorkflowEvent> {
        self.event_tx.subscribe()
    }

    /// Update workflow status and emit events
    pub fn set_workflow_status(&self, workflow_id: &str, status: WorkflowStatus) {
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
    pub fn set_checkpoint(&self, workflow_id: &str, key: &str, value: Vec<u8>) {
        {
            let mut state = self.state.lock().unwrap();
            let checkpoints = state.checkpoints
                .entry(workflow_id.to_string())
                .or_insert_with(HashMap::new);
            checkpoints.insert(key.to_string(), value);
        }

        let event = WorkflowEvent::CheckpointSet {
            workflow_id: workflow_id.to_string(),
            key: key.to_string(),
        };
        let _ = self.event_tx.send(event);
    }

    /// Get workflow status
    pub fn get_workflow_status(&self, workflow_id: &str) -> Option<WorkflowStatus> {
        let state = self.state.lock().unwrap();
        state.workflows.get(workflow_id).cloned()
    }

    /// Get a checkpoint value
    pub fn get_checkpoint(&self, workflow_id: &str, key: &str) -> Option<Vec<u8>> {
        let state = self.state.lock().unwrap();
        state.checkpoints
            .get(workflow_id)?
            .get(key)
            .cloned()
    }

    /// Clear all state (useful for testing)
    pub fn clear(&self) {
        let mut state = self.state.lock().unwrap();
        state.workflows.clear();
        state.checkpoints.clear();
    }

    /// Execute a leader/follower operation with retry logic for leadership changes
    async fn execute_leader_follower_operation<F, Fut>(
        &self,
        workflow_id: &str,
        cluster: &RaftCluster<WorkflowCommand>,
        leader_operation: F,
        wait_for_event_fn: fn(&WorkflowEvent) -> bool,
        follower_timeout: Duration,
    ) -> Result<(), WorkflowError>
    where
        F: Fn() -> Fut + Send,
        Fut: std::future::Future<Output = Result<(), WorkflowError>> + Send,
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
                // Use a shorter timeout to allow retrying if we become leader
                let timeout_duration = if attempt < max_retries - 1 {
                    Some(Duration::from_secs(5)) // Shorter timeout for retry attempts
                } else {
                    Some(follower_timeout) // Use provided timeout for final attempt
                };

                match self.wait_for_specific_event(workflow_id, subscription, wait_for_event_fn, timeout_duration).await {
                    Ok(_) => return Ok(()),
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
    async fn wait_for_specific_event(
        &self,
        _workflow_id: &str,
        mut subscription: WorkflowSubscription,
        event_check: fn(&WorkflowEvent) -> bool,
        timeout_duration: Option<Duration>
    ) -> Result<(), WorkflowError> {
        subscription.wait_for_event(event_check, timeout_duration).await
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

                match cluster_ref.propose_and_sync(command).await {
                    Ok(_) => {
                        // Verify the workflow was started
                        match self.get_workflow_status(&workflow_id_owned) {
                            Some(WorkflowStatus::Running) => Ok(()),
                            Some(_) => Err(WorkflowError::ClusterError("Workflow in unexpected state".to_string())),
                            None => Err(WorkflowError::ClusterError("Workflow start was not applied".to_string())),
                        }
                    },
                    Err(e) => {
                        // Check if the error is because workflow already exists due to race condition
                        if let Some(WorkflowStatus::Running) = self.get_workflow_status(&workflow_id_owned) {
                            Ok(()) // Another client started it, that's fine
                        } else {
                            Err(WorkflowError::ClusterError(e.to_string()))
                        }
                    }
                }
            },
            |event| matches!(event, WorkflowEvent::Started { .. }),
            Duration::from_secs(10),
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
                    .map_err(|e| WorkflowError::ClusterError(e.to_string()))
            },
            |event| matches!(event, WorkflowEvent::Completed { .. } | WorkflowEvent::Failed { .. }),
            Duration::from_secs(10),
        ).await?;

        // Final state verification - check for failure after completion
        match self.get_workflow_status(workflow_id) {
            Some(WorkflowStatus::Completed) => Ok(()),
            Some(WorkflowStatus::Failed) => Err(WorkflowError::ClusterError("Workflow failed".to_string())),
            _ => Err(WorkflowError::ClusterError("Unexpected workflow state".to_string())),
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
    WorkflowRuntime::instance().start_workflow(workflow_id, cluster).await
}

/// End a workflow with the given ID
pub async fn end_workflow(
    workflow_id: &str,
    cluster: &RaftCluster<WorkflowCommand>
) -> Result<(), WorkflowError> {
    WorkflowRuntime::instance().end_workflow(workflow_id, cluster).await
}

/// Get the status of a workflow
pub fn get_workflow_status(workflow_id: &str) -> Option<WorkflowStatus> {
    WorkflowRuntime::instance().get_workflow_status(workflow_id)
}

/// Get a checkpoint value for a workflow
pub fn get_workflow_checkpoint(workflow_id: &str, key: &str) -> Option<Vec<u8>> {
    WorkflowRuntime::instance().get_checkpoint(workflow_id, key)
}

/// Clear all workflow state (useful for testing)
pub fn clear_workflow_state() {
    WorkflowRuntime::instance().clear();
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