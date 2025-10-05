pub mod raft;
pub mod workflow;
pub mod grpc;

// Type alias for our workflow-specific cluster
pub type WorkflowCluster = raft::RaftCluster<workflow::WorkflowCommandExecutor>;

pub use raft::{RaftCluster, PlaceholderCommand, RaftCommand, RoleChange};
pub use workflow::{
    WorkflowCommand, WorkflowStartData, WorkflowEndData, CheckpointData,
    WorkflowCommandExecutor, WorkflowError,
    ReplicatedVar, ReplicatedVarError, WorkflowRuntime, WorkflowRun, WorkflowContext
};
pub use grpc::{start_grpc_server, start_grpc_server_with_config, GrpcServerHandle, RaftClient, discover_peers, DiscoveredPeer, bootstrap};