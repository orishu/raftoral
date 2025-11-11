pub mod raft;
pub mod grpc2;  // gRPC implementation for generic2 architecture
pub mod kv;  // KV runtime for generic2 architecture
pub mod management;  // Management runtime for generic2 architecture
pub mod workflow2;  // Workflow execution runtime for generic2 architecture
pub mod full_node;  // Complete node stack (Layer 0-7)
pub mod config;  // Configuration for Raftoral nodes

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

// Re-export common types for convenience
pub use config::RaftoralConfig;