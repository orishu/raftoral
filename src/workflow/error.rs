use serde::{Deserialize, Serialize};

/// Status of a workflow
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WorkflowStatus {
    Running,
    Completed,
    Failed,
}

/// Errors that can occur during workflow operations
#[derive(Clone, Debug, Serialize, Deserialize)]
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
