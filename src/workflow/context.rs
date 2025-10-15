use crate::workflow::commands::*;
use crate::workflow::error::*;
use crate::workflow::executor::WorkflowCommandExecutor;
use crate::workflow::runtime::WorkflowRuntime;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

/// Shared workflow context that can be safely shared with ReplicatedVars
#[derive(Clone)]
pub struct WorkflowContext {
    pub workflow_id: String,
    pub runtime: Arc<WorkflowRuntime>,
}

impl std::fmt::Debug for WorkflowContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowContext")
            .field("workflow_id", &self.workflow_id)
            .field("runtime", &"<WorkflowRuntime>")
            .finish()
    }
}

impl WorkflowContext {
    /// Create a ReplicatedVar from within a workflow execution context
    ///
    /// This is used by workflow functions that are spawned via consensus-driven execution.
    pub async fn create_replicated_var<T>(
        &self,
        key: &str,
        value: T,
    ) -> Result<crate::workflow::replicated_var::ReplicatedVar<T>, WorkflowError>
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de> + Clone + Send + Sync + 'static,
    {
        // Create a WorkflowRun for ReplicatedVar operations
        // This WorkflowRun is marked as finished since the workflow lifecycle is managed elsewhere
        let workflow_run = Arc::new(WorkflowRun {
            context: self.clone(),
            runtime: self.runtime.clone(),
        });

        crate::workflow::replicated_var::ReplicatedVar::with_value(key, &workflow_run, value).await
    }

    /// Create a ReplicatedVar with computation from within a workflow execution context
    pub async fn create_replicated_var_with_computation<T, F, Fut>(
        &self,
        key: &str,
        computation: F,
    ) -> Result<crate::workflow::replicated_var::ReplicatedVar<T>, WorkflowError>
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de> + Clone + Send + Sync + 'static,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T> + Send + 'static,
    {
        // Create a WorkflowRun for ReplicatedVar operations
        let workflow_run = Arc::new(WorkflowRun {
            context: self.clone(),
            runtime: self.runtime.clone(),
        });

        crate::workflow::replicated_var::ReplicatedVar::with_computation(key, &workflow_run, computation).await
    }
}

/// A scoped workflow run that must be explicitly finished
#[derive(Debug)]
pub struct WorkflowRun {
    pub context: WorkflowContext,
    pub runtime: Arc<WorkflowRuntime>,
}

impl WorkflowRun {
    /// Create a WorkflowRun for an existing workflow (internal use)
    pub fn for_existing(
        workflow_id: &str,
        runtime: Arc<WorkflowRuntime>
    ) -> Self {
        let context = WorkflowContext {
            workflow_id: workflow_id.to_string(),
            runtime: runtime.clone(),
        };
        WorkflowRun {
            context,
            runtime,
        }
    }

    /// Start a new workflow and return a WorkflowRun instance
    pub async fn start<T>(
        workflow_id: &str,
        runtime: &Arc<WorkflowRuntime>,
        input: T
    ) -> Result<Arc<Self>, WorkflowError>
    where
        T: serde::Serialize + Send + 'static,
    {
        let cluster = runtime.cluster.clone();

        // Check if workflow already exists and is running
        if let Some(status) = runtime.get_workflow_status(workflow_id) {
            if status == WorkflowStatus::Running {
                return Err(WorkflowError::AlreadyExists(workflow_id.to_string()));
            }
        }

        // Serialize the input
        let input_bytes = serde_json::to_vec(&input).map_err(|e| WorkflowError::SerializationError(e.to_string()))?;

        // Propose WorkflowStart command - proposer becomes the owner
        let command = WorkflowCommand::WorkflowStart(WorkflowStartData {
            workflow_id: workflow_id.to_string(),
            workflow_type: String::new(),
            version: 0,
            input: input_bytes.clone(),
            owner_node_id: cluster.node_id(),
        });

        cluster.propose_and_sync(command).await
            .map_err(|e| WorkflowError::ClusterError(e.to_string()))?;

        let context = WorkflowContext {
            workflow_id: workflow_id.to_string(),
            runtime: runtime.clone(),
        };
        Ok(Arc::new(WorkflowRun {
            context,
            runtime: runtime.clone(),
        }))
    }

    /// Get the workflow ID
    pub fn workflow_id(&self) -> &str {
        &self.context.workflow_id
    }

    /// Get a reference to the cluster
    pub fn cluster(&self) -> &Arc<crate::raft::generic::RaftCluster<WorkflowCommandExecutor>> {
        &self.runtime.cluster
    }

    /// Get the workflow context (for ReplicatedVar creation)
    pub fn context(&self) -> &WorkflowContext {
        &self.context
    }

    /// Finish the workflow with a result value
    pub async fn finish_with<T>(&self, result_value: T) -> Result<T, WorkflowError>
    where
        T: Clone + Send + Sync + serde::Serialize + 'static,
    {
        // Serialize the result value as Result<T, WorkflowError>
        let result_as_result: Result<T, WorkflowError> = Ok(result_value.clone());
        let result_bytes = serde_json::to_vec(&result_as_result)
            .map_err(|e| WorkflowError::SerializationError(e.to_string()))?;

        // Call finish_with_bytes to do the actual work
        self.finish_with_bytes(result_bytes).await?;

        Ok(result_value)
    }

    /// Finish the workflow with raw bytes (internal use by executor)
    pub async fn finish_with_bytes(&self, result_bytes: Vec<u8>) -> Result<(), WorkflowError> {
        // Check if workflow exists and is running
        match self.runtime.get_workflow_status(&self.context.workflow_id) {
            Some(WorkflowStatus::Running) => {},
            Some(WorkflowStatus::Completed) => {
                        return Ok(()); // Already completed
            },
            Some(_) => return Err(WorkflowError::ClusterError("Workflow is not running".to_string())),
            None => return Err(WorkflowError::NotFound(self.context.workflow_id.to_string())),
        }

        let workflow_id_owned = self.context.workflow_id.clone();
        let result_bytes_owned = result_bytes.clone();

        self.runtime.execute_owner_or_wait(
            &self.context.workflow_id,
            &self.runtime.cluster,
            || async {
                // Owner operation: Propose WorkflowEnd command with result
                let command = WorkflowCommand::WorkflowEnd(WorkflowEndData {
                    workflow_id: workflow_id_owned.clone(),
                    result: result_bytes_owned.clone(),
                });

                self.runtime.cluster.propose_and_sync(command).await
                    .map_err(|e| WorkflowError::ClusterError(e.to_string()))?;
                Ok(())
            },
            |event| {
                if matches!(event, crate::workflow::executor::WorkflowEvent::Completed { .. } | crate::workflow::executor::WorkflowEvent::Failed { .. }) {
                    Some(())
                } else {
                    None
                }
            },
            None, // Wait indefinitely
        ).await?;

        // Mark as finished
        Ok(())
    }

    /// Wait for the workflow to complete and return the result
    /// This method waits for the workflow to finish naturally without manually ending it
    pub async fn wait_for_completion(&self) -> Result<(), WorkflowError> {
        // Check current status
        match self.runtime.get_workflow_status(&self.context.workflow_id) {
            Some(WorkflowStatus::Completed) => {
                        return Ok(());
            },
            Some(WorkflowStatus::Failed) => {
                        return Err(WorkflowError::ClusterError("Workflow failed".to_string()));
            },
            Some(WorkflowStatus::Running) => {
                // Continue to wait
            },
            None => return Err(WorkflowError::NotFound(self.context.workflow_id.to_string())),
        }

        // Wait for completion event
        self.runtime.execute_owner_or_wait(
            &self.context.workflow_id,
            &self.runtime.cluster,
            || async {
                // For non-owners, we don't need to do anything - just wait for the event
                Ok(())
            },
            |event| {
                if matches!(event, crate::workflow::executor::WorkflowEvent::Completed { .. } | crate::workflow::executor::WorkflowEvent::Failed { .. }) {
                    Some(())
                } else {
                    None
                }
            },
            None, // Wait indefinitely
        ).await?;

        // Final state verification
        match self.runtime.get_workflow_status(&self.context.workflow_id) {
            Some(WorkflowStatus::Completed) => {
                        Ok(())
            },
            Some(WorkflowStatus::Failed) => {
                        Err(WorkflowError::ClusterError("Workflow failed".to_string()))
            },
            _ => Err(WorkflowError::ClusterError("Unexpected workflow state".to_string())),
        }
    }
}

// Drop implementation removed - no longer needed since workflow execution
// happens in a separate spawned task, not tied to WorkflowRun lifetime

/// A typed wrapper for WorkflowRun that preserves the output type from start_workflow_typed
/// This allows wait_for_completion to return the correctly typed result
#[derive(Debug)]
pub struct TypedWorkflowRun<O> {
    inner: Arc<WorkflowRun>,
    _phantom: PhantomData<O>,
}

impl<O> TypedWorkflowRun<O>
where
    O: Clone + Send + Sync + serde::Serialize + for<'de> serde::Deserialize<'de> + 'static,
{
    /// Create a new TypedWorkflowRun from a WorkflowRun
    pub(crate) fn new(inner: Arc<WorkflowRun>) -> Self {
        Self {
            inner,
            _phantom: PhantomData,
        }
    }

    /// Get the workflow ID
    pub fn workflow_id(&self) -> &str {
        self.inner.workflow_id()
    }

    /// Get the workflow context
    pub fn context(&self) -> &WorkflowContext {
        self.inner.context()
    }

    /// Wait for the workflow to complete and return the typed result
    /// This method waits for the workflow to finish naturally and returns the result
    pub async fn wait_for_completion(self) -> Result<O, WorkflowError> {
        // First, subscribe to events BEFORE checking state to avoid race condition
        let mut subscription = self.inner.runtime.subscribe_to_workflow(&self.inner.context.workflow_id);

        // Check if workflow already completed (race condition: completed before we subscribed)
        match self.inner.runtime.get_workflow_status(&self.inner.context.workflow_id) {
            Some(WorkflowStatus::Completed) => {
                // Workflow already completed - retrieve result from stored state
                if let Some(result_bytes) = self.inner.runtime.get_workflow_result(&self.inner.context.workflow_id) {
                    // Deserialize and return the stored result
                    let workflow_result: Result<O, WorkflowError> = serde_json::from_slice(&result_bytes)
                        .map_err(|e| WorkflowError::DeserializationError(e.to_string()))?;

                    return workflow_result;
                } else {
                    return Err(WorkflowError::ClusterError(
                        "Workflow completed but result not found in state".to_string()
                    ));
                }
            },
            Some(WorkflowStatus::Failed) => {
                // Try to get the error from stored results
                if let Some(error_bytes) = self.inner.runtime.get_workflow_result(&self.inner.context.workflow_id) {
                    let workflow_result: Result<O, WorkflowError> = serde_json::from_slice(&error_bytes)
                        .map_err(|e| WorkflowError::DeserializationError(e.to_string()))?;

                    return workflow_result;
                }
                return Err(WorkflowError::ClusterError("Workflow failed".to_string()));
            },
            _ => {
                // Workflow still running or starting, proceed with subscription
            }
        }

        // Wait for the workflow completion event and extract the result
        let result_bytes: Vec<u8> = subscription.wait_for_event(
            |event| {
                match event {
                    crate::workflow::executor::WorkflowEvent::Completed { workflow_id, result } if workflow_id == &self.inner.context.workflow_id => {
                        Some(result.clone())
                    },
                    crate::workflow::executor::WorkflowEvent::Failed { workflow_id, error } if workflow_id == &self.inner.context.workflow_id => {
                        Some(error.clone())
                    },
                    _ => None
                }
            },
            Some(Duration::from_secs(30))
        ).await?;

        // Deserialize the result from the event data
        let workflow_result: Result<O, WorkflowError> = serde_json::from_slice(&result_bytes)
            .map_err(|e| WorkflowError::DeserializationError(e.to_string()))?;

        // Return the final result or error
        workflow_result
    }
}
