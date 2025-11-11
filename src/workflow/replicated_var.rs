//! Replicated variables (checkpoints) for workflow state
//!
//! Replicated variables provide a way to store workflow state that is
//! replicated across the Raft cluster for fault tolerance.

use crate::workflow::{WorkflowContext, WorkflowError};
use serde::{Deserialize, Serialize};
use std::ops::Deref;

/// A replicated variable that stores workflow state across the cluster
///
/// Values are stored through Raft consensus, ensuring all nodes have
/// consistent state and enabling recovery after failures.
pub struct ReplicatedVar<T> {
    /// Workflow ID this variable belongs to
    workflow_id: String,

    /// Checkpoint key
    key: String,

    /// Cached value (once set)
    value: T,

    /// Context for proposing updates
    context: WorkflowContext,
}

impl<T> ReplicatedVar<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
{
    /// Create a new replicated variable with an initial value
    ///
    /// This proposes a checkpoint through Raft and returns the variable with
    /// the value cached locally.
    pub async fn with_value(
        key: &str,
        context: &WorkflowContext,
        value: T,
    ) -> Result<Self, WorkflowError> {
        // Propose the checkpoint (handles queue checking internally)
        let stored_value = context
            .runtime
            .set_checkpoint(&context.workflow_id, key, value)
            .await?;

        Ok(ReplicatedVar {
            workflow_id: context.workflow_id.clone(),
            key: key.to_string(),
            value: stored_value,
            context: context.clone(),
        })
    }

    /// Create a new replicated variable by computing an initial value
    ///
    /// This executes the computation function to produce a value, proposes it
    /// as a checkpoint through Raft, and returns the variable with the value cached.
    ///
    /// # Example
    /// ```ignore
    /// let counter = ReplicatedVar::with_computation("counter", &ctx, || async {
    ///     // Compute initial value
    ///     expensive_calculation().await
    /// }).await?;
    /// ```
    pub async fn with_computation<F, Fut>(
        key: &str,
        context: &WorkflowContext,
        compute: F,
    ) -> Result<Self, WorkflowError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T> + Send + 'static,
    {
        // Execute the computation to get the initial value
        let value = compute().await;

        // Propose the checkpoint (handles queue checking internally)
        let stored_value = context
            .runtime
            .set_checkpoint(&context.workflow_id, key, value)
            .await?;

        Ok(ReplicatedVar {
            workflow_id: context.workflow_id.clone(),
            key: key.to_string(),
            value: stored_value,
            context: context.clone(),
        })
    }

    /// Get the current cached value
    ///
    /// This returns the locally cached value without accessing Raft.
    pub fn get(&self) -> T {
        self.value.clone()
    }

    /// Set a new value
    ///
    /// This proposes a checkpoint update through Raft and updates the local cache.
    pub async fn set(&mut self, value: T) -> Result<(), WorkflowError> {
        let stored_value = self
            .context
            .runtime
            .set_checkpoint(&self.workflow_id, &self.key, value)
            .await?;

        self.value = stored_value;
        Ok(())
    }

    /// Update the value using a function
    ///
    /// This applies the update function to the current value, proposes the result
    /// through Raft, and updates the local cache.
    pub async fn update<F>(&mut self, updater: F) -> Result<T, WorkflowError>
    where
        F: FnOnce(T) -> T + Send + 'static,
    {
        let new_value = updater(self.value.clone());

        let stored_value = self
            .context
            .runtime
            .set_checkpoint(&self.workflow_id, &self.key, new_value)
            .await?;

        self.value = stored_value.clone();
        Ok(stored_value)
    }

    /// Get the key for this variable
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Get the workflow ID
    pub fn workflow_id(&self) -> &str {
        &self.workflow_id
    }
}

impl<T> Deref for ReplicatedVar<T>
where
    T: Clone,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::generic::{InProcessNetwork, InProcessNetworkSender, TransportLayer, RaftNodeConfig};
    use crate::workflow::WorkflowRuntime;
    use crate::{checkpoint, checkpoint_compute};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn create_logger() -> slog::Logger {
        use slog::Drain;
        let decorator = slog_term::PlainDecorator::new(std::io::stdout());
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        slog::Logger::root(drain, slog::o!())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_replicated_var_basic_operations() {
        let logger = create_logger();
        let network = Arc::new(InProcessNetwork::new());
        let transport = Arc::new(TransportLayer::new(Arc::new(InProcessNetworkSender::new(
            network.clone(),
        ))));

        let (tx, rx) = mpsc::channel(100);

        // Register node with network
        network.register_node(1, tx.clone()).await;

        let config = RaftNodeConfig {
            node_id: 1,
            cluster_id: 1,
            snapshot_interval: 0,
            ..Default::default()
        };

        // Create shared registry
        let registry = Arc::new(tokio::sync::Mutex::new(crate::workflow::WorkflowRegistry::new()));

        let (runtime, node) = WorkflowRuntime::new(config, transport, rx, registry, logger).unwrap();
        let runtime = Arc::new(runtime);

        // Campaign to become leader
        node.lock().await.campaign().await.expect("Campaign should succeed");

        // Run node in background
        let node_clone = node.clone();
        tokio::spawn(async move {
            use crate::raft::generic::RaftNode;
            let _ = RaftNode::run_from_arc(node_clone).await;
        });

        // Wait for leadership
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        // Create workflow context
        let context = WorkflowContext::new("test-workflow".to_string(), runtime.clone());

        // Create replicated variable
        let mut counter = ReplicatedVar::with_value("counter", &context, 42i32)
            .await
            .expect("Create replicated var should succeed");

        // Test get
        assert_eq!(counter.get(), 42);
        assert_eq!(*counter, 42); // Test Deref

        // Create a second variable with different key
        let mut value2 = ReplicatedVar::with_value("value2", &context, 100i32)
            .await
            .expect("Create second replicated var should succeed");

        assert_eq!(value2.get(), 100);

        // Create a third variable and test update
        let mut value3 = ReplicatedVar::with_value("value3", &context, 50i32)
            .await
            .expect("Create third replicated var should succeed");

        let new_value = value3.update(|v| v + 10).await.expect("Update should succeed");
        assert_eq!(new_value, 60);
        assert_eq!(value3.get(), 60);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_replicated_var_with_computation() {
        let logger = create_logger();
        let network = Arc::new(InProcessNetwork::new());
        let transport = Arc::new(TransportLayer::new(Arc::new(InProcessNetworkSender::new(
            network.clone(),
        ))));

        let (tx, rx) = mpsc::channel(100);

        // Register node with network
        network.register_node(1, tx.clone()).await;

        let config = RaftNodeConfig {
            node_id: 1,
            cluster_id: 1,
            snapshot_interval: 0,
            ..Default::default()
        };

        // Create shared registry
        let registry = Arc::new(tokio::sync::Mutex::new(crate::workflow::WorkflowRegistry::new()));

        let (runtime, node) = WorkflowRuntime::new(config, transport, rx, registry, logger).unwrap();
        let runtime = Arc::new(runtime);

        // Campaign to become leader
        node.lock().await.campaign().await.expect("Campaign should succeed");

        // Run node in background
        let node_clone = node.clone();
        tokio::spawn(async move {
            use crate::raft::generic::RaftNode;
            let _ = RaftNode::run_from_arc(node_clone).await;
        });

        // Wait for leadership
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        // Create workflow context
        let context = WorkflowContext::new("test-workflow".to_string(), runtime.clone());

        // Test with_computation with async computation
        let computed = ReplicatedVar::with_computation("computed", &context, || async {
            // Simulate async computation
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            42i32 * 2
        })
        .await
        .expect("with_computation should succeed");

        assert_eq!(computed.get(), 84);
        assert_eq!(*computed, 84);

        // Test with_computation with immediate value
        let immediate = ReplicatedVar::with_computation("immediate", &context, || async { 100i32 })
            .await
            .expect("with_computation should succeed");

        assert_eq!(immediate.get(), 100);
    }
}
