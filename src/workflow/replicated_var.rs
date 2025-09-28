use std::marker::PhantomData;
use serde::{Serialize, Deserialize};
use crate::workflow::execution::{WorkflowError, WorkflowRun, WorkflowContext};

/// A type-safe replicated variable that stores its value as a checkpoint in the Raft cluster.
///
/// ReplicatedVar provides a high-level abstraction over the checkpoint system, allowing
/// workflows to store and retrieve typed values that are automatically serialized,
/// replicated via Raft consensus, and available for recovery after failover.
///
/// # Example Usage
/// ```rust
/// # use std::sync::Arc;
/// # use raftoral::{RaftCluster, WorkflowCommand, WorkflowRuntime, ReplicatedVar};
/// #
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let cluster = Arc::new(RaftCluster::<WorkflowCommand>::new_single_node(1).await?);
/// # let workflow_runtime = WorkflowRuntime::new(cluster);
/// # let workflow_run = workflow_runtime.start("example_workflow").await?;
/// let retry_count = ReplicatedVar::new("retry_count", &workflow_run, 0);
/// let result = ReplicatedVar::new("result", &workflow_run, None::<String>);
///
/// let _new_count = retry_count.set(1).await?;
/// let _new_result = result.set(Some("success".to_string())).await?;
/// # Ok(())
/// # }
/// ```
pub struct ReplicatedVar<T> {
    /// Unique key for this replicated variable within the workflow
    key: String,
    /// Reference to the workflow context this variable belongs to
    context: WorkflowContext,
    /// Phantom data to ensure type safety
    _phantom: PhantomData<T>,
}

impl<T> ReplicatedVar<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
{
    /// Create a new ReplicatedVar with an initial value
    ///
    /// # Arguments
    /// * `key` - Unique identifier for this variable within the workflow
    /// * `workflow_run` - Reference to the workflow this variable belongs to
    /// * `_initial_value` - Initial value to set if no checkpoint exists (currently unused)
    ///
    /// # Returns
    /// A new ReplicatedVar instance
    pub fn new(
        key: &str,
        workflow_run: &WorkflowRun,
        _initial_value: T,
    ) -> Self {
        ReplicatedVar {
            key: key.to_string(),
            context: workflow_run.context().clone(),
            _phantom: PhantomData,
        }
    }


    /// Set a new value for this replicated variable
    ///
    /// This method will:
    /// - **Leader**: Propose a SetCheckpoint command and return the value immediately
    /// - **Follower**: Wait indefinitely for the checkpoint to be applied by the leader and return the deserialized value
    ///
    /// # Arguments
    /// * `value` - The new value to set
    ///
    /// # Returns
    /// * `Ok(T)` with the value if the operation was successful
    /// * `Err(WorkflowError)` if the operation failed
    pub async fn set(&self, value: T) -> Result<T, WorkflowError> {
        use crate::workflow::execution::WorkflowRuntime;

        // Create a temporary runtime to use the set_replicated_var method
        let runtime = WorkflowRuntime { cluster: self.context.cluster.clone() };

        // Use the existing set_replicated_var implementation
        runtime.set_replicated_var(
            &self.context.workflow_id,
            &self.key,
            value,
            &self.context.cluster
        ).await
    }



    /// Get the key used for this replicated variable
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Get the workflow ID this variable belongs to
    pub fn workflow_id(&self) -> &str {
        &self.context.workflow_id
    }
}

impl<T> Clone for ReplicatedVar<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        ReplicatedVar {
            key: self.key.clone(),
            context: self.context.clone(),
            _phantom: PhantomData,
        }
    }
}

/// Error type for ReplicatedVar operations
#[derive(Debug, Clone)]
pub enum ReplicatedVarError {
    /// Serialization failed
    SerializationError(String),
    /// Deserialization failed
    DeserializationError(String),
    /// Underlying workflow error
    WorkflowError(WorkflowError),
}

impl std::fmt::Display for ReplicatedVarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplicatedVarError::SerializationError(msg) => write!(f, "Serialization error: {}", msg),
            ReplicatedVarError::DeserializationError(msg) => write!(f, "Deserialization error: {}", msg),
            ReplicatedVarError::WorkflowError(err) => write!(f, "Workflow error: {}", err),
        }
    }
}

impl std::error::Error for ReplicatedVarError {}

impl From<WorkflowError> for ReplicatedVarError {
    fn from(err: WorkflowError) -> Self {
        ReplicatedVarError::WorkflowError(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::execution::WorkflowRuntime;
    use std::time::Duration;

    #[tokio::test]
    async fn test_replicated_var_basic_operations() {

        let workflow_id = "test_workflow_replicated_var";

        // Create workflow runtime and start workflow
        let workflow_runtime = WorkflowRuntime::new_single_node(1).await.expect("Failed to create workflow runtime");

        // Wait for leadership establishment
        tokio::time::sleep(Duration::from_millis(500)).await;
        let workflow_run = workflow_runtime.start(workflow_id).await.expect("Failed to start workflow");

        // Test integer variable
        let counter = ReplicatedVar::new("counter", &workflow_run, 0i32);

        // Set initial value
        let returned_value = counter.set(42).await.expect("Failed to set counter value");
        assert_eq!(returned_value, 42);

        // Update value
        let returned_value = counter.set(100).await.expect("Failed to update counter value");
        assert_eq!(returned_value, 100);

        // Test string variable
        let message = ReplicatedVar::new("message", &workflow_run, String::new());
        let returned_message = message.set("Hello, Raft!".to_string()).await.expect("Failed to set message");
        assert_eq!(returned_message, "Hello, Raft!".to_string());

        // Finish the workflow
        workflow_run.finish().await.expect("Failed to finish workflow");
    }

    #[tokio::test]
    async fn test_replicated_var_different_workflows() {

        // Create workflow runtime and start two different workflows
        let workflow_runtime = WorkflowRuntime::new_single_node(1).await.expect("Failed to create workflow runtime");

        // Wait for leadership establishment
        tokio::time::sleep(Duration::from_millis(500)).await;
        let workflow_run1 = workflow_runtime.start("workflow_1").await.expect("Failed to start workflow 1");
        let workflow_run2 = workflow_runtime.start("workflow_2").await.expect("Failed to start workflow 2");

        // Variables in different workflows should be isolated
        let var1 = ReplicatedVar::new("shared_key", &workflow_run1, 0i32);
        let var2 = ReplicatedVar::new("shared_key", &workflow_run2, 0i32);

        let returned_var1 = var1.set(100).await.expect("Failed to set var1");
        let returned_var2 = var2.set(200).await.expect("Failed to set var2");

        assert_eq!(returned_var1, 100);
        assert_eq!(returned_var2, 200);

        // Finish both workflows
        workflow_run1.finish().await.expect("Failed to finish workflow 1");
        workflow_run2.finish().await.expect("Failed to finish workflow 2");
    }

    #[tokio::test]
    async fn test_replicated_var_complex_types() {

        let workflow_id = "test_workflow_complex";

        // Create workflow runtime and start workflow
        let workflow_runtime = WorkflowRuntime::new_single_node(1).await.expect("Failed to create workflow runtime");

        // Wait for leadership establishment
        tokio::time::sleep(Duration::from_millis(500)).await;
        let workflow_run = workflow_runtime.start(workflow_id).await.expect("Failed to start workflow");

        // Test with Result type (common for error handling)
        let api_result = ReplicatedVar::new(
            "api_result",
            &workflow_run,
            Ok::<String, String>("initial".to_string())
        );

        let returned_error = api_result.set(Err("Network error".to_string())).await.expect("Failed to set error result");
        assert_eq!(returned_error, Err("Network error".to_string()));

        let returned_success = api_result.set(Ok("Success!".to_string())).await.expect("Failed to set success result");
        assert_eq!(returned_success, Ok("Success!".to_string()));

        // Test with Option type
        let optional_data = ReplicatedVar::new(
            "optional_data",
            &workflow_run,
            None::<Vec<String>>
        );

        let returned_optional = optional_data.set(Some(vec!["item1".to_string(), "item2".to_string()])).await
            .expect("Failed to set optional data");
        assert_eq!(returned_optional, Some(vec!["item1".to_string(), "item2".to_string()]));

        // Finish the workflow
        workflow_run.finish().await.expect("Failed to finish workflow");
    }
}