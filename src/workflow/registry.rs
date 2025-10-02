use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use crate::workflow::execution::{WorkflowError, WorkflowContext};

/// A trait for workflow functions that can be registered and executed
///
/// This trait enables type-safe workflow registration with serializable inputs and outputs.
/// Workflow functions take serializable input parameters and return serializable results.
pub trait WorkflowFunction<I, O>: Send + Sync + 'static
where
    I: Send + 'static,
    O: Send + 'static,
{
    /// Execute the workflow function with the given input and context
    fn execute(
        &self,
        input: I,
        context: WorkflowContext,
    ) -> Pin<Box<dyn Future<Output = Result<O, WorkflowError>> + Send>>;
}

/// A type-erased wrapper for workflow functions to enable storage in collections
pub struct BoxedWorkflowFunction {
    /// The actual function that executes the workflow and returns serialized output
    executor: Box<dyn Fn(Box<dyn Any + Send>, WorkflowContext) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, WorkflowError>> + Send>> + Send + Sync>,
}

impl BoxedWorkflowFunction {
    /// Create a new boxed workflow function from a typed workflow function
    pub fn new<I, O, F>(func: F) -> Self
    where
        I: Send + for<'de> serde::Deserialize<'de> + 'static,
        O: Send + serde::Serialize + 'static,
        F: WorkflowFunction<I, O>,
    {
        let func = Arc::new(func);
        let executor = Box::new(move |input_any: Box<dyn Any + Send>, context: WorkflowContext| {
            let func = func.clone();
            Box::pin(async move {
                // Try to downcast input from Any to Vec<u8> (serialized input)
                let input: I = if let Ok(input_bytes) = input_any.downcast::<Vec<u8>>() {
                    // Deserialize from bytes
                    serde_json::from_slice(&input_bytes)
                        .map_err(|e| WorkflowError::DeserializationError(e.to_string()))?
                } else {
                    // This shouldn't happen in production, but kept for backward compatibility
                    return Err(WorkflowError::ClusterError("Expected serialized input (Vec<u8>)".to_string()));
                };

                // Execute workflow and get the Result<O, WorkflowError>
                let output_result = func.execute(input, context).await;

                // Serialize the entire Result to Vec<u8>
                let result_bytes = serde_json::to_vec(&output_result)
                    .map_err(|e| WorkflowError::SerializationError(e.to_string()))?;

                Ok(result_bytes)
            }) as Pin<Box<dyn Future<Output = Result<Vec<u8>, WorkflowError>> + Send>>
        });

        BoxedWorkflowFunction { executor }
    }

    /// Execute the boxed workflow function
    pub async fn execute(
        &self,
        input: Box<dyn Any + Send>,
        context: WorkflowContext,
    ) -> Result<Vec<u8>, WorkflowError> {
        (self.executor)(input, context).await
    }
}

/// A registry for workflow functions that maps (type, version) pairs to executable functions
///
/// This registry enables registration and lookup of workflow functions by their type identifier
/// and version number, supporting distributed execution across leader and follower nodes.
#[derive(Default)]
pub struct WorkflowRegistry {
    /// Map from (workflow_type, version) to workflow function
    workflows: HashMap<(String, u32), Arc<BoxedWorkflowFunction>>,
}

impl WorkflowRegistry {
    /// Create a new empty workflow registry
    pub fn new() -> Self {
        Self {
            workflows: HashMap::new(),
        }
    }

    /// Register a workflow function with the given type and version
    ///
    /// # Arguments
    /// * `workflow_type` - Unique identifier for the workflow type
    /// * `version` - Version number for this workflow implementation
    /// * `function` - The workflow function to register
    ///
    /// # Returns
    /// * `Ok(())` if registration was successful
    /// * `Err(WorkflowError)` if a workflow with the same type and version already exists
    pub fn register<I, O, F>(
        &mut self,
        workflow_type: &str,
        version: u32,
        function: F,
    ) -> Result<(), WorkflowError>
    where
        I: Send + for<'de> serde::Deserialize<'de> + 'static,
        O: Send + serde::Serialize + 'static,
        F: WorkflowFunction<I, O>,
    {
        let key = (workflow_type.to_string(), version);

        if self.workflows.contains_key(&key) {
            return Err(WorkflowError::AlreadyExists(format!(
                "Workflow '{}' version {} is already registered",
                workflow_type, version
            )));
        }

        let boxed_function = Arc::new(BoxedWorkflowFunction::new(function));
        self.workflows.insert(key, boxed_function);

        Ok(())
    }

    /// Register a workflow function using a closure for easier testing and simple workflows
    ///
    /// # Arguments
    /// * `workflow_type` - Unique identifier for the workflow type
    /// * `version` - Version number for this workflow implementation
    /// * `function` - A closure that takes (input, context) and returns a future with Result<output, WorkflowError>
    ///
    /// # Returns
    /// * `Ok(())` if registration was successful
    /// * `Err(WorkflowError)` if a workflow with the same type and version already exists
    pub fn register_closure<I, O, F, Fut>(
        &mut self,
        workflow_type: &str,
        version: u32,
        function: F,
    ) -> Result<(), WorkflowError>
    where
        I: Send + Sync + for<'de> serde::Deserialize<'de> + 'static,
        O: Send + Sync + serde::Serialize + 'static,
        F: Fn(I, WorkflowContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, WorkflowError>> + Send + Sync + 'static,
    {
        struct ClosureWorkflow<I, O, F, Fut>
        where
            F: Fn(I, WorkflowContext) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = Result<O, WorkflowError>> + Send + Sync + 'static,
        {
            closure: F,
            _phantom: std::marker::PhantomData<(I, O, Fut)>,
        }

        impl<I, O, F, Fut> WorkflowFunction<I, O> for ClosureWorkflow<I, O, F, Fut>
        where
            I: Send + Sync + for<'de> serde::Deserialize<'de> + 'static,
            O: Send + Sync + serde::Serialize + 'static,
            F: Fn(I, WorkflowContext) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = Result<O, WorkflowError>> + Send + Sync + 'static,
        {
            fn execute(
                &self,
                input: I,
                context: WorkflowContext,
            ) -> Pin<Box<dyn Future<Output = Result<O, WorkflowError>> + Send>> {
                Box::pin((self.closure)(input, context))
            }
        }

        let closure_workflow = ClosureWorkflow {
            closure: function,
            _phantom: std::marker::PhantomData,
        };

        self.register(workflow_type, version, closure_workflow)
    }

    /// Look up a workflow function by type and version
    ///
    /// # Arguments
    /// * `workflow_type` - The workflow type identifier
    /// * `version` - The workflow version number
    ///
    /// # Returns
    /// * `Some(Arc<BoxedWorkflowFunction>)` if the workflow is found
    /// * `None` if no workflow with the given type and version exists
    pub fn get(&self, workflow_type: &str, version: u32) -> Option<Arc<BoxedWorkflowFunction>> {
        let key = (workflow_type.to_string(), version);
        self.workflows.get(&key).cloned()
    }

    /// Check if a workflow with the given type and version is registered
    pub fn contains(&self, workflow_type: &str, version: u32) -> bool {
        let key = (workflow_type.to_string(), version);
        self.workflows.contains_key(&key)
    }

    /// Get the number of registered workflows
    pub fn len(&self) -> usize {
        self.workflows.len()
    }

    /// Check if the registry is empty
    pub fn is_empty(&self) -> bool {
        self.workflows.is_empty()
    }

    /// List all registered workflow types and versions
    pub fn list_workflows(&self) -> Vec<(String, u32)> {
        self.workflows.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::any::Any;

    #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct TestInput {
        value: i32,
    }

    #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct TestOutput {
        result: i32,
    }

    struct SimpleWorkflow;

    impl WorkflowFunction<TestInput, TestOutput> for SimpleWorkflow {
        fn execute(
            &self,
            input: TestInput,
            _context: WorkflowContext,
        ) -> Pin<Box<dyn Future<Output = Result<TestOutput, WorkflowError>> + Send>> {
            Box::pin(async move {
                Ok(TestOutput {
                    result: input.value * 2,
                })
            })
        }
    }

    #[tokio::test]
    async fn test_workflow_registration() {
        let mut registry = WorkflowRegistry::new();

        // Test registration
        let result = registry.register("test_workflow", 1, SimpleWorkflow);
        assert!(result.is_ok());

        // Test duplicate registration fails
        let duplicate_result = registry.register("test_workflow", 1, SimpleWorkflow);
        assert!(duplicate_result.is_err());

        // Test different version succeeds
        let different_version_result = registry.register("test_workflow", 2, SimpleWorkflow);
        assert!(different_version_result.is_ok());
    }

    #[tokio::test]
    async fn test_workflow_lookup() {
        let mut registry = WorkflowRegistry::new();
        registry.register("test_workflow", 1, SimpleWorkflow).unwrap();

        // Test successful lookup
        let workflow = registry.get("test_workflow", 1);
        assert!(workflow.is_some());

        // Test failed lookup
        let missing_workflow = registry.get("missing_workflow", 1);
        assert!(missing_workflow.is_none());

        let wrong_version = registry.get("test_workflow", 999);
        assert!(wrong_version.is_none());
    }

    #[tokio::test]
    async fn test_workflow_execution() {
        let mut registry = WorkflowRegistry::new();
        registry.register("test_workflow", 1, SimpleWorkflow).unwrap();

        let workflow = registry.get("test_workflow", 1).unwrap();

        // Create test input and serialize it
        let input = TestInput { value: 21 };
        let input_bytes = serde_json::to_vec(&input).unwrap();
        let input_any: Box<dyn Any + Send> = Box::new(input_bytes);

        // Create mock context using WorkflowRuntime
        let workflow_runtime = crate::workflow::execution::WorkflowRuntime::new_single_node(1).await.unwrap();
        let context = WorkflowContext {
            workflow_id: "test_instance".to_string(),
            runtime: workflow_runtime.clone(),
        };

        // Execute workflow - returns serialized Result<TestOutput, WorkflowError>
        let result_bytes = workflow.execute(input_any, context).await.unwrap();

        // Deserialize the Result
        let output_result: Result<TestOutput, crate::workflow::execution::WorkflowError> =
            serde_json::from_slice(&result_bytes).unwrap();

        // Unwrap the Ok value
        let output = output_result.unwrap();

        assert_eq!(output.result, 42);
    }

    #[test]
    fn test_registry_utility_methods() {
        let mut registry = WorkflowRegistry::new();

        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);

        registry.register("workflow1", 1, SimpleWorkflow).unwrap();
        registry.register("workflow2", 1, SimpleWorkflow).unwrap();

        assert!(!registry.is_empty());
        assert_eq!(registry.len(), 2);
        assert!(registry.contains("workflow1", 1));
        assert!(registry.contains("workflow2", 1));
        assert!(!registry.contains("workflow3", 1));

        let workflows = registry.list_workflows();
        assert_eq!(workflows.len(), 2);
        assert!(workflows.contains(&("workflow1".to_string(), 1)));
        assert!(workflows.contains(&("workflow2".to_string(), 1)));
    }
}