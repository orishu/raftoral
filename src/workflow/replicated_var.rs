use std::future::Future;
use serde::{Serialize, Deserialize};
use std::sync::Arc;
use crate::workflow::execution::{WorkflowError, WorkflowRun};

/// A type-safe replicated variable that stores its value as a checkpoint in the Raft cluster.
///
/// ReplicatedVar provides a high-level abstraction over the checkpoint system, allowing
/// workflows to store and retrieve typed values that are automatically serialized,
/// replicated via Raft consensus, and available for recovery after failover.
///
/// # Direct Value Usage
/// ```rust
/// # use raftoral::{WorkflowRuntime, ReplicatedVar};
/// #
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let workflow_runtime = WorkflowRuntime::new_single_node(1).await?;
/// # tokio::time::sleep(std::time::Duration::from_millis(100)).await; // Wait for leadership
/// # let workflow_run = workflow_runtime.start("example_workflow", ()).await?;
///
/// // Direct value initialization - value is immediately stored in Raft
/// let mut counter = ReplicatedVar::with_value("counter", &workflow_run, 42).await?;
/// println!("Counter value: {}", counter.get()); // 42
///
/// // Update the value
/// let updated_counter = counter.update(|old_val| old_val + 1).await?;
/// println!("Updated counter: {}", updated_counter); // 43
///
/// # workflow_run.finish_with(()).await?;
/// # Ok(())
/// # }
/// ```
///
/// # Computed Value Usage (Side Effects)
/// ```rust
/// # use raftoral::{WorkflowRuntime, ReplicatedVar};
/// #
/// # async fn compute_something() -> i32 { 100 }
/// #
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let workflow_runtime = WorkflowRuntime::new_single_node(1).await?;
/// # tokio::time::sleep(std::time::Duration::from_millis(100)).await; // Wait for leadership
/// # let workflow_run = workflow_runtime.start("example_workflow", ()).await?;
///
/// // Computed value - lambda is executed and result stored in Raft
/// let computed_result = ReplicatedVar::with_computation(
///     "computed_result",
///     &workflow_run,
///     || async { compute_something().await }
/// ).await?;
/// println!("Computed value: {}", computed_result.get()); // 100
///
/// # workflow_run.finish_with(()).await?;
/// # Ok(())
/// # }
/// ```
pub struct ReplicatedVar<T> {
    /// Unique key for this replicated variable within the workflow
    key: String,
    /// Reference to the workflow run this variable belongs to
    workflow_run: Arc<WorkflowRun>,
    /// The current value (cached after initialization/updates)
    value: T,
}

impl<T> ReplicatedVar<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
{
    /// Create and initialize a ReplicatedVar with a direct value
    ///
    /// This method immediately stores the value in the Raft cluster and returns
    /// a ReplicatedVar instance with the value cached locally.
    ///
    /// # Arguments
    /// * `key` - Unique identifier for this variable within the workflow
    /// * `workflow_run` - Reference to the workflow this variable belongs to
    /// * `value` - The value to store
    ///
    /// # Returns
    /// * `Ok(ReplicatedVar<T>)` with the value stored and cached
    /// * `Err(WorkflowError)` if the operation failed
    pub async fn with_value(
        key: &str,
        workflow_run: &Arc<WorkflowRun>,
        value: T,
    ) -> Result<Self, WorkflowError> {
        // Store the value using the workflow run's runtime
        let stored_value = workflow_run.runtime.set_replicated_var(
            &workflow_run.context.workflow_id,
            key,
            value.clone(),
            &workflow_run.runtime.cluster
        ).await?;

        Ok(ReplicatedVar {
            key: key.to_string(),
            workflow_run: workflow_run.clone(),
            value: stored_value,
        })
    }


    /// Create and initialize a ReplicatedVar with a computed value
    ///
    /// This method executes the provided computation and stores the result
    /// in the Raft cluster. This is useful for side effects that should only
    /// be executed once and have their results replicated.
    ///
    /// # Arguments
    /// * `key` - Unique identifier for this variable within the workflow
    /// * `workflow_run` - Reference to the workflow this variable belongs to
    /// * `computation` - An async closure that computes the value
    ///
    /// # Returns
    /// * `Ok(ReplicatedVar<T>)` with the computed value stored and cached
    /// * `Err(WorkflowError)` if the operation failed
    pub async fn with_computation<F, Fut>(
        key: &str,
        workflow_run: &Arc<WorkflowRun>,
        computation: F,
    ) -> Result<Self, WorkflowError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
    {
        // Execute the computation to get the value
        let computed_value = computation().await;

        // Store the computed value using the workflow run's runtime
        let stored_value = workflow_run.runtime.set_replicated_var(
            &workflow_run.context.workflow_id,
            key,
            computed_value.clone(),
            &workflow_run.runtime.cluster
        ).await?;

        Ok(ReplicatedVar {
            key: key.to_string(),
            workflow_run: workflow_run.clone(),
            value: stored_value,
        })
    }


    /// Get the current cached value
    ///
    /// This returns the locally cached value without accessing Raft.
    /// The value is guaranteed to be the latest value that was stored.
    pub fn get(&self) -> T {
        self.value.clone()
    }

    /// Update the value using a function
    ///
    /// This method applies the update function to the current value,
    /// stores the result in Raft, and updates the local cache.
    ///
    /// # Arguments
    /// * `updater` - Function that transforms the current value
    ///
    /// # Returns
    /// * `Ok(T)` with the new value if successful
    /// * `Err(WorkflowError)` if the operation failed
    pub async fn update<F>(&mut self, updater: F) -> Result<T, WorkflowError>
    where
        F: FnOnce(T) -> T + Send + 'static,
    {
        // Apply the update function
        let new_value = updater(self.value.clone());

        // Store the new value in Raft using the workflow run's runtime
        let stored_value = self.workflow_run.runtime.set_replicated_var(
            &self.workflow_run.context.workflow_id,
            &self.key,
            new_value,
            &self.workflow_run.runtime.cluster
        ).await?;

        // Update the local cache
        self.value = stored_value.clone();

        Ok(stored_value)
    }


    /// Get the key used for this replicated variable
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Get the workflow ID this variable belongs to
    pub fn workflow_id(&self) -> &str {
        &self.workflow_run.context.workflow_id
    }
}

impl<T> Clone for ReplicatedVar<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        ReplicatedVar {
            key: self.key.clone(),
            workflow_run: self.workflow_run.clone(),
            value: self.value.clone(),
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
        let workflow_run = workflow_runtime.start(workflow_id, ()).await.expect("Failed to start workflow");

        // Test new with_value() interface for direct initialization
        let counter = ReplicatedVar::with_value("counter", &workflow_run, 42i32).await.expect("Failed to create counter");
        assert_eq!(counter.get(), 42);

        // Test update functionality
        let mut counter = counter;
        let updated_value = counter.update(|val| val + 58).await.expect("Failed to update counter");
        assert_eq!(updated_value, 100);
        assert_eq!(counter.get(), 100);

        // Test string variable with direct initialization
        let message = ReplicatedVar::with_value("message", &workflow_run, "Hello, Raft!".to_string())
            .await.expect("Failed to create message");
        assert_eq!(message.get(), "Hello, Raft!".to_string());

        // Finish the workflow
        workflow_run.finish_with(()).await.expect("Failed to finish workflow");
    }

    #[tokio::test]
    async fn test_replicated_var_different_workflows() {
        // Create workflow runtime and start two different workflows
        let workflow_runtime = WorkflowRuntime::new_single_node(1).await.expect("Failed to create workflow runtime");

        // Wait for leadership establishment
        tokio::time::sleep(Duration::from_millis(500)).await;
        let workflow_run1 = workflow_runtime.start("workflow_1", ()).await.expect("Failed to start workflow 1");
        let workflow_run2 = workflow_runtime.start("workflow_2", ()).await.expect("Failed to start workflow 2");

        // Variables in different workflows should be isolated using same key but different workflows
        let var1 = ReplicatedVar::with_value("shared_key", &workflow_run1, 100i32).await.expect("Failed to create var1");
        let var2 = ReplicatedVar::with_value("shared_key", &workflow_run2, 200i32).await.expect("Failed to create var2");

        // Verify values are isolated
        assert_eq!(var1.get(), 100);
        assert_eq!(var2.get(), 200);

        // Finish both workflows
        workflow_run1.finish_with(()).await.expect("Failed to finish workflow 1");
        workflow_run2.finish_with(()).await.expect("Failed to finish workflow 2");
    }

    #[tokio::test]
    async fn test_replicated_var_complex_types() {
        let workflow_id = "test_workflow_complex";

        // Create workflow runtime and start workflow
        let workflow_runtime = WorkflowRuntime::new_single_node(1).await.expect("Failed to create workflow runtime");

        // Wait for leadership establishment
        tokio::time::sleep(Duration::from_millis(500)).await;
        let workflow_run = workflow_runtime.start(workflow_id, ()).await.expect("Failed to start workflow");

        // Test with Result type using new with_value interface
        let mut api_result = ReplicatedVar::with_value(
            "api_result",
            &workflow_run,
            Ok::<String, String>("initial".to_string())
        ).await.expect("Failed to create api_result");

        assert_eq!(api_result.get(), Ok("initial".to_string()));

        // Update to error state
        let returned_error = api_result.update(|_| Err("Network error".to_string())).await.expect("Failed to update to error result");
        assert_eq!(returned_error, Err("Network error".to_string()));
        assert_eq!(api_result.get(), Err("Network error".to_string()));

        // Update to success state
        let returned_success = api_result.update(|_| Ok("Success!".to_string())).await.expect("Failed to update to success result");
        assert_eq!(returned_success, Ok("Success!".to_string()));
        assert_eq!(api_result.get(), Ok("Success!".to_string()));

        // Test with Option type using with_computation for computed values
        let optional_data = ReplicatedVar::with_computation(
            "optional_data",
            &workflow_run,
            || async {
                // Simulate some computation that returns an Option
                Some(vec!["computed_item1".to_string(), "computed_item2".to_string()])
            }
        ).await.expect("Failed to create computed optional data");

        assert_eq!(optional_data.get(), Some(vec!["computed_item1".to_string(), "computed_item2".to_string()]));

        // Finish the workflow
        workflow_run.finish_with(()).await.expect("Failed to finish workflow");
    }

    #[tokio::test]
    async fn test_replicated_var_side_effects() {
        let workflow_id = "test_workflow_side_effects";

        // Create workflow runtime and start workflow
        let workflow_runtime = WorkflowRuntime::new_single_node(1).await.expect("Failed to create workflow runtime");

        // Wait for leadership establishment
        tokio::time::sleep(Duration::from_millis(500)).await;
        let workflow_run = workflow_runtime.start(workflow_id, ()).await.expect("Failed to start workflow");

        // Test with_computation for side effects - simulate an external API call
        let external_data = ReplicatedVar::with_computation(
            "external_data",
            &workflow_run,
            || async {
                // Simulate fetching data from external service
                tokio::time::sleep(Duration::from_millis(10)).await;
                format!("fetched_at_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis())
            }
        ).await.expect("Failed to create computed external data");

        // Verify the computation result is stored
        let value = external_data.get();
        assert!(value.starts_with("fetched_at_"));

        // Test mathematical computation
        let fibonacci_result = ReplicatedVar::with_computation(
            "fibonacci_10",
            &workflow_run,
            || async {
                // Compute 10th Fibonacci number
                let mut a = 0;
                let mut b = 1;
                for _ in 2..=10 {
                    let temp = a + b;
                    a = b;
                    b = temp;
                }
                b
            }
        ).await.expect("Failed to create fibonacci computation");

        assert_eq!(fibonacci_result.get(), 55); // 10th Fibonacci number

        // Finish the workflow
        workflow_run.finish_with(()).await.expect("Failed to finish workflow");
    }
}