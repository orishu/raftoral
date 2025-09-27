pub mod raft;
pub mod workflow;

// Type alias for our workflow-specific cluster
pub type WorkflowCluster = raft::RaftCluster<workflow::WorkflowCommand>;

pub use raft::{RaftCluster, PlaceholderCommand, RaftCommand, RaftCommandType};
pub use workflow::{WorkflowCommand, start_workflow, end_workflow, get_workflow_status, WorkflowError};