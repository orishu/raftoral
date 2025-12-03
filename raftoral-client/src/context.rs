//! Workflow execution context

use crate::client::RaftoralClient;
use crate::error::Result;
use futures::Future;
use std::sync::Arc;

/// Context for workflow execution
///
/// Provides access to checkpoint operations and workflow metadata.
pub struct WorkflowContext {
    /// Workflow ID
    pub workflow_id: String,
    /// Whether this node is the workflow owner
    pub is_owner: bool,
    /// Client connection to sidecar
    client: Arc<RaftoralClient>,
}

impl WorkflowContext {
    /// Create a new workflow context
    pub fn new(workflow_id: String, is_owner: bool, client: Arc<RaftoralClient>) -> Self {
        Self {
            workflow_id,
            is_owner,
            client,
        }
    }

    /// Create a checkpoint with a computed value
    ///
    /// If this is the owner node, computes the value and proposes it to Raft.
    /// If this is a follower node, waits for the checkpoint event from Raft.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let value = ctx.checkpoint("my_checkpoint", || {
    ///     compute_deterministic_value()
    /// }).await?;
    /// ```
    pub async fn checkpoint<T, F>(&self, name: &str, compute: F) -> Result<T>
    where
        T: serde::de::DeserializeOwned + serde::Serialize,
        F: FnOnce() -> T,
    {
        if self.is_owner {
            // Owner: compute value and propose
            let value = compute();
            let serialized = serde_json::to_vec(&value)?;
            self.client
                .propose_checkpoint(&self.workflow_id, name, serialized)
                .await?;

            // Wait for checkpoint event (confirms Raft commit)
            let confirmed = self.client.wait_checkpoint(&self.workflow_id, name).await?;
            Ok(serde_json::from_slice(&confirmed)?)
        } else {
            // Follower: wait for checkpoint event
            let value_bytes = self.client.wait_checkpoint(&self.workflow_id, name).await?;
            Ok(serde_json::from_slice(&value_bytes)?)
        }
    }

    /// Create a checkpoint with an async computed value (for side effects)
    ///
    /// Similar to `checkpoint`, but supports async computation.
    /// Use this for operations like API calls that should only execute once.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let api_data = ctx.checkpoint_compute("api_call", || async {
    ///     fetch_from_external_api().await
    /// }).await?;
    /// ```
    pub async fn checkpoint_compute<T, F, Fut>(&self, name: &str, compute: F) -> Result<T>
    where
        T: serde::de::DeserializeOwned + serde::Serialize,
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        if self.is_owner {
            // Owner: compute value and propose
            let value = compute().await;
            let serialized = serde_json::to_vec(&value)?;
            self.client
                .propose_checkpoint(&self.workflow_id, name, serialized)
                .await?;

            // Wait for checkpoint event (confirms Raft commit)
            let confirmed = self.client.wait_checkpoint(&self.workflow_id, name).await?;
            Ok(serde_json::from_slice(&confirmed)?)
        } else {
            // Follower: wait for checkpoint event
            let value_bytes = self.client.wait_checkpoint(&self.workflow_id, name).await?;
            Ok(serde_json::from_slice(&value_bytes)?)
        }
    }
}
