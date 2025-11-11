//! Workflow execution context
//!
//! Provides the context for workflow execution including workflow ID
//! and methods for creating replicated variables (checkpoints).

use crate::workflow::{WorkflowError, WorkflowRuntime};
use std::sync::Arc;

/// Context provided to workflow functions during execution
///
/// This context provides access to workflow metadata and operations
/// like creating replicated variables (checkpoints).
#[derive(Clone)]
pub struct WorkflowContext {
    /// Unique ID for this workflow instance
    pub workflow_id: String,

    /// Runtime for proposing checkpoints
    pub runtime: Arc<WorkflowRuntime>,
}

impl WorkflowContext {
    /// Create a new workflow context
    pub fn new(workflow_id: String, runtime: Arc<WorkflowRuntime>) -> Self {
        Self {
            workflow_id,
            runtime,
        }
    }

    /// Get the workflow ID
    pub fn workflow_id(&self) -> &str {
        &self.workflow_id
    }

    /// Create a replicated variable with an initial value
    ///
    /// This creates a checkpoint that is replicated across the cluster.
    pub async fn create_replicated_var<T>(
        &self,
        key: &str,
        value: T,
    ) -> Result<crate::workflow::ReplicatedVar<T>, WorkflowError>
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de> + Clone + Send + Sync + 'static,
    {
        crate::workflow::ReplicatedVar::with_value(key, self, value).await
    }

    /// Create a replicated variable from a computed value (side effect)
    ///
    /// This executes the provided computation once and stores the result
    /// in the Raft cluster. Useful for side effects like API calls or database queries
    /// that should only be executed once and have their results replicated.
    pub async fn create_replicated_var_with_computation<T, F, Fut>(
        &self,
        key: &str,
        compute: F,
    ) -> Result<crate::workflow::ReplicatedVar<T>, WorkflowError>
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de> + Clone + Send + Sync + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T> + Send + 'static,
    {
        crate::workflow::ReplicatedVar::with_computation(key, self, compute).await
    }
}

impl std::fmt::Debug for WorkflowContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowContext")
            .field("workflow_id", &self.workflow_id)
            .field("runtime", &"<WorkflowRuntime>")
            .finish()
    }
}

/// Handle to a running workflow instance
///
/// This handle allows waiting for workflow completion and retrieving results.
#[derive(Clone)]
pub struct WorkflowRun {
    /// Workflow ID
    pub workflow_id: String,

    /// Runtime for accessing workflow state
    pub runtime: Arc<WorkflowRuntime>,
}

impl WorkflowRun {
    /// Create a new workflow run handle
    pub fn new(workflow_id: String, runtime: Arc<WorkflowRuntime>) -> Self {
        Self {
            workflow_id,
            runtime,
        }
    }

    /// Get the workflow ID
    pub fn workflow_id(&self) -> &str {
        &self.workflow_id
    }

    /// Get the workflow context (for creating replicated variables)
    pub fn context(&self) -> WorkflowContext {
        WorkflowContext::new(self.workflow_id.clone(), self.runtime.clone())
    }
}

/// Typed workflow run handle
///
/// Provides type-safe access to workflow results.
pub struct TypedWorkflowRun<O> {
    inner: WorkflowRun,
    _phantom: std::marker::PhantomData<O>,
}

impl<O> TypedWorkflowRun<O>
where
    O: serde::de::DeserializeOwned,
{
    /// Create a new typed workflow run
    pub fn new(inner: WorkflowRun) -> Self {
        Self {
            inner,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Get the workflow ID
    pub fn workflow_id(&self) -> &str {
        self.inner.workflow_id()
    }

    /// Wait for workflow completion and get typed result
    ///
    /// This subscribes to workflow events and waits for completion.
    pub async fn wait_for_completion(&self) -> Result<O, WorkflowError> {
        use crate::workflow::WorkflowEvent;

        // Subscribe to events
        let mut event_rx = self.inner.runtime.subscribe_events();

        // Wait for WorkflowCompleted event for this workflow
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    match event {
                        WorkflowEvent::WorkflowCompleted {
                            workflow_id,
                            result,
                        } => {
                            if workflow_id == self.inner.workflow_id {
                                // Deserialize the result
                                // The result is stored as serialized Result<O, WorkflowError>
                                let result: Result<O, WorkflowError> =
                                    serde_json::from_slice(&result).map_err(|e| {
                                        WorkflowError::DeserializationError(e.to_string())
                                    })?;
                                return result;
                            }
                        }
                        WorkflowEvent::WorkflowFailed {
                            workflow_id,
                            error,
                        } => {
                            if workflow_id == self.inner.workflow_id {
                                // Deserialize the error
                                let error: WorkflowError =
                                    serde_json::from_slice(&error).map_err(|e| {
                                        WorkflowError::DeserializationError(e.to_string())
                                    })?;
                                return Err(error);
                            }
                        }
                        _ => {
                            // Ignore other events
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Continue on lag
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return Err(WorkflowError::ClusterError(
                        "Event channel closed".to_string(),
                    ));
                }
            }
        }
    }
}
