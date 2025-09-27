use crate::raft::generic::{RaftCluster, RaftCommandType};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::sleep;

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
        match self {
            WorkflowCommand::WorkflowStart { workflow_id } => {
                slog::info!(logger, "Started workflow"; "workflow_id" => workflow_id);

                // Register workflow as started in global state
                let workflows = get_workflow_state();
                let mut state = workflows.lock().unwrap();
                state.workflows.insert(workflow_id.clone(), WorkflowStatus::Running);
            },
            WorkflowCommand::WorkflowEnd { workflow_id } => {
                slog::info!(logger, "Ended workflow"; "workflow_id" => workflow_id);

                // Mark workflow as completed
                let workflows = get_workflow_state();
                let mut state = workflows.lock().unwrap();
                state.workflows.insert(workflow_id.clone(), WorkflowStatus::Completed);
            },
            WorkflowCommand::SetCheckpoint { workflow_id, key, value } => {
                slog::info!(logger, "Set checkpoint";
                           "workflow_id" => workflow_id,
                           "key" => key,
                           "value_size" => value.len());

                // Store checkpoint in global state
                let workflows = get_workflow_state();
                let mut state = workflows.lock().unwrap();

                let checkpoints = state.checkpoints
                    .entry(workflow_id.clone())
                    .or_insert_with(HashMap::new);
                checkpoints.insert(key.clone(), value.clone());
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

/// Global workflow state for tracking workflows across the cluster
#[derive(Default)]
pub struct WorkflowState {
    /// Track workflow status by ID
    pub workflows: HashMap<String, WorkflowStatus>,
    /// Store checkpoints: workflow_id -> (key -> value)
    pub checkpoints: HashMap<String, HashMap<String, Vec<u8>>>,
}

/// Global workflow state storage
static WORKFLOW_STATE: std::sync::OnceLock<Arc<Mutex<WorkflowState>>> = std::sync::OnceLock::new();

fn get_workflow_state() -> Arc<Mutex<WorkflowState>> {
    WORKFLOW_STATE.get_or_init(|| Arc::new(Mutex::new(WorkflowState::default()))).clone()
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
    // Check if workflow already exists
    {
        let workflows = get_workflow_state();
        let state = workflows.lock().unwrap();
        if let Some(status) = state.workflows.get(workflow_id) {
            if *status == WorkflowStatus::Running {
                return Err(WorkflowError::AlreadyExists(workflow_id.to_string()));
            }
        }
    }

    // Check if we're the leader
    if cluster.is_leader().await {
        // Leader behavior: Propose the WorkflowStart command and wait for it to be applied
        let command = WorkflowCommand::WorkflowStart {
            workflow_id: workflow_id.to_string(),
        };

        cluster.propose_and_sync(command).await
            .map_err(|e| WorkflowError::ClusterError(e.to_string()))?;

        // Verify the workflow was started
        let workflows = get_workflow_state();
        let state = workflows.lock().unwrap();
        match state.workflows.get(workflow_id) {
            Some(WorkflowStatus::Running) => Ok(()),
            Some(_) => Err(WorkflowError::ClusterError("Workflow in unexpected state".to_string())),
            None => Err(WorkflowError::ClusterError("Workflow start was not applied".to_string())),
        }
    } else {
        // Follower behavior: Wait for the WorkflowStart command to be applied
        wait_for_workflow_start(workflow_id).await
    }
}

/// End a workflow with the given ID
pub async fn end_workflow(
    workflow_id: &str,
    cluster: &RaftCluster<WorkflowCommand>
) -> Result<(), WorkflowError> {
    // Check if workflow exists and is running
    {
        let workflows = get_workflow_state();
        let state = workflows.lock().unwrap();
        match state.workflows.get(workflow_id) {
            Some(WorkflowStatus::Running) => {},
            Some(_) => return Err(WorkflowError::ClusterError("Workflow is not running".to_string())),
            None => return Err(WorkflowError::NotFound(workflow_id.to_string())),
        }
    }

    if cluster.is_leader().await {
        // Leader behavior: Propose the WorkflowEnd command and wait for it to be applied
        let command = WorkflowCommand::WorkflowEnd {
            workflow_id: workflow_id.to_string(),
        };

        cluster.propose_and_sync(command).await
            .map_err(|e| WorkflowError::ClusterError(e.to_string()))?;

        Ok(())
    } else {
        // Follower behavior: Wait for the WorkflowEnd command to be applied
        wait_for_workflow_end(workflow_id).await
    }
}


/// Wait for a workflow to be started (follower behavior)
async fn wait_for_workflow_start(workflow_id: &str) -> Result<(), WorkflowError> {
    let timeout = Duration::from_secs(10);
    let start_time = std::time::Instant::now();

    while start_time.elapsed() < timeout {
        {
            let workflows = get_workflow_state();
            let state = workflows.lock().unwrap();
            if let Some(WorkflowStatus::Running) = state.workflows.get(workflow_id) {
                return Ok(());
            }
        }

        sleep(Duration::from_millis(50)).await;
    }

    Err(WorkflowError::Timeout)
}

/// Wait for a workflow to be ended (follower behavior)
async fn wait_for_workflow_end(workflow_id: &str) -> Result<(), WorkflowError> {
    let timeout = Duration::from_secs(10);
    let start_time = std::time::Instant::now();

    while start_time.elapsed() < timeout {
        {
            let workflows = get_workflow_state();
            let state = workflows.lock().unwrap();
            match state.workflows.get(workflow_id) {
                Some(WorkflowStatus::Completed) => return Ok(()),
                Some(WorkflowStatus::Failed) => return Err(WorkflowError::ClusterError("Workflow failed".to_string())),
                _ => {},
            }
        }

        sleep(Duration::from_millis(50)).await;
    }

    Err(WorkflowError::Timeout)
}

/// Get the status of a workflow
pub fn get_workflow_status(workflow_id: &str) -> Option<WorkflowStatus> {
    let workflows = get_workflow_state();
    let state = workflows.lock().unwrap();
    state.workflows.get(workflow_id).cloned()
}

/// Get a checkpoint value for a workflow
pub fn get_workflow_checkpoint(workflow_id: &str, key: &str) -> Option<Vec<u8>> {
    let workflows = get_workflow_state();
    let state = workflows.lock().unwrap();
    state.checkpoints
        .get(workflow_id)?
        .get(key)
        .cloned()
}

/// Clear all workflow state (useful for testing)
pub fn clear_workflow_state() {
    let workflows = get_workflow_state();
    let mut state = workflows.lock().unwrap();
    state.workflows.clear();
    state.checkpoints.clear();
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
        sleep(Duration::from_millis(500)).await;

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
        sleep(Duration::from_millis(500)).await;

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
        sleep(Duration::from_millis(500)).await;

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