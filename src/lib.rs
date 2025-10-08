pub mod raft;
pub mod workflow;
pub mod grpc;
pub mod runtime;

/// Declarative macro to create replicated variables with clean syntax.
///
/// This macro transforms a `let` statement into a ReplicatedVar creation with an
/// auto-generated key based on the variable name and source location.
///
/// # Example
///
/// ```ignore
/// replicated!(ctx, let mut counter = 0i32);
/// replicated!(ctx, let results = Vec::<String>::new());
/// ```
///
/// Expands to:
///
/// ```ignore
/// let mut counter = ctx.create_replicated_var(
///     concat!("counter@", file!(), ":", line!(), ":", column!()),
///     0i32
/// ).await?;
/// ```
///
/// The variable can then be used with:
/// - `*counter` - to read the value (via Deref)
/// - `counter.set(value)` - to update the value
/// - `counter.update(|v| ...)` - to atomically update
#[macro_export]
macro_rules! replicated {
    ($ctx:expr, let mut $name:ident = $init:expr) => {
        let mut $name = $ctx.create_replicated_var(
            concat!(stringify!($name), "@", file!(), ":", line!(), ":", column!()),
            $init
        ).await?;
    };
    ($ctx:expr, let $name:ident = $init:expr) => {
        let $name = $ctx.create_replicated_var(
            concat!(stringify!($name), "@", file!(), ":", line!(), ":", column!()),
            $init
        ).await?;
    };
    ($ctx:expr, let mut $name:ident: $ty:ty = $init:expr) => {
        let mut $name: $crate::workflow::ReplicatedVar<$ty> = $ctx.create_replicated_var(
            concat!(stringify!($name), "@", file!(), ":", line!(), ":", column!()),
            $init
        ).await?;
    };
    ($ctx:expr, let $name:ident: $ty:ty = $init:expr) => {
        let $name: $crate::workflow::ReplicatedVar<$ty> = $ctx.create_replicated_var(
            concat!(stringify!($name), "@", file!(), ":", line!(), ":", column!()),
            $init
        ).await?;
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