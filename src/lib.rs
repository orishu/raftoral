pub mod raft;
pub mod workflow;

// Type alias for our workflow-specific cluster
pub type WorkflowCluster = raft::RaftCluster<workflow::WorkflowCommandExecutor>;

pub use raft::{RaftCluster, PlaceholderCommand, RaftCommand, RoleChange};
pub use workflow::{
    WorkflowCommand, WorkflowCommandExecutor, start_workflow, end_workflow, WorkflowError,
    ReplicatedVar, ReplicatedVarError, WorkflowRuntime, WorkflowRun, WorkflowContext
};