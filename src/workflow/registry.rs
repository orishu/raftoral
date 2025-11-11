use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use crate::workflow::error::WorkflowError;
use crate::workflow::context::WorkflowContext;

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
        Fut: Future<Output = Result<O, WorkflowError>> + Send + 'static,
    {
        struct ClosureWorkflow<I, O, F, Fut>
        where
            F: Fn(I, WorkflowContext) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = Result<O, WorkflowError>> + Send + 'static,
        {
            closure: F,
            _phantom: std::marker::PhantomData<(I, O)>,
        }

        impl<I, O, F, Fut> WorkflowFunction<I, O> for ClosureWorkflow<I, O, F, Fut>
        where
            I: Send + Sync + for<'de> serde::Deserialize<'de> + 'static,
            O: Send + Sync + serde::Serialize + 'static,
            F: Fn(I, WorkflowContext) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = Result<O, WorkflowError>> + Send + 'static,
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

    /// Check if a workflow is registered
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
