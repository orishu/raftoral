pub mod raft;
pub mod workflow;
pub mod grpc;
pub mod runtime;

/// Create a replicated variable with an explicit key.
///
/// This macro creates a ReplicatedVar with a user-provided key that remains stable
/// across code changes and workflow versions.
///
/// # Example
///
/// ```ignore
/// let mut counter = checkpoint!(ctx, "counter", 0i32);
/// let history = checkpoint!(ctx, "history", Vec::<String>::new());
/// ```
///
/// The variable can then be used with:
/// - `*counter` - to read the value (via Deref)
/// - `counter.set(value)` - to update the value
/// - `counter.update(|v| ...)` - to atomically update
#[macro_export]
macro_rules! checkpoint {
    ($ctx:expr, $key:literal, $value:expr) => {
        $ctx.create_replicated_var($key, $value).await?
    };
}

/// Create a replicated variable from a computed value (side effect).
///
/// This macro executes the provided computation once and stores the result
/// in the Raft cluster. Useful for side effects like API calls or database queries
/// that should only be executed once and have their results replicated.
///
/// # Example
///
/// ```ignore
/// let api_data = checkpoint_compute!(ctx, "api_data", || async {
///     fetch_from_external_api().await
/// });
/// ```
#[macro_export]
macro_rules! checkpoint_compute {
    ($ctx:expr, $key:literal, $computation:expr) => {
        $ctx.create_replicated_var_with_computation($key, $computation).await?
    };
}

// Type alias for our workflow-specific cluster
pub type WorkflowCluster = raft::RaftCluster<workflow::WorkflowCommandExecutor>;

pub use raft::{RaftCluster, PlaceholderCommand, RaftCommand, RoleChange};
pub use workflow::{
    WorkflowCommand, WorkflowStartData, WorkflowEndData, CheckpointData,
    WorkflowCommandExecutor, WorkflowError,
    ReplicatedVar, ReplicatedVarError, WorkflowRuntime, WorkflowRun, WorkflowContext
};
pub use grpc::{start_grpc_server, start_grpc_server_with_config, GrpcServerHandle, RaftClient, discover_peers, DiscoveredPeer, bootstrap};
pub use runtime::{RaftoralGrpcRuntime, RaftoralConfig};