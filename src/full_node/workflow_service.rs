//! WorkflowManagement gRPC service implementation for FullNode
//!
//! Provides RunWorkflowAsync and WaitForWorkflowCompletion RPCs for workflow execution.

use crate::grpc::proto::{
    workflow_management_server::WorkflowManagement, RunWorkflowAsyncResponse,
    RunWorkflowRequest, RunWorkflowResponse, WaitForWorkflowRequest,
};
use crate::management::ManagementRuntime;
use crate::workflow::WorkflowRuntime;
use slog::{info, warn, Logger};
use std::sync::Arc;
use tonic::{Request, Response, Status};
use uuid::Uuid;

/// WorkflowManagement service implementation
#[derive(Clone)]
pub struct WorkflowManagementService {
    /// Management runtime for accessing workflow sub-clusters
    management_runtime: Arc<ManagementRuntime<WorkflowRuntime>>,

    /// Logger
    logger: Logger,
}

impl WorkflowManagementService {
    /// Create a new WorkflowManagementService
    pub fn new(
        management_runtime: Arc<ManagementRuntime<WorkflowRuntime>>,
        logger: Logger,
    ) -> Self {
        Self {
            management_runtime,
            logger,
        }
    }
}

#[tonic::async_trait]
impl WorkflowManagement for WorkflowManagementService {
    async fn run_workflow_async(
        &self,
        request: Request<RunWorkflowRequest>,
    ) -> Result<Response<RunWorkflowAsyncResponse>, Status> {
        let req = request.into_inner();

        info!(self.logger, "RunWorkflowAsync called";
            "workflow_type" => &req.workflow_type,
            "version" => req.version
        );

        // Generate workflow ID
        let workflow_id = Uuid::new_v4();

        // For now, use the default execution cluster (cluster_id=1)
        // Future: Implement load balancing across multiple execution clusters
        let cluster_id = 1u32;

        // Get the workflow runtime for this cluster
        let workflow_runtime = self
            .management_runtime
            .get_sub_cluster_runtime(&cluster_id)
            .await
            .ok_or_else(|| {
                Status::failed_precondition(format!(
                    "Execution cluster {} not available on this node",
                    cluster_id
                ))
            })?;

        // Parse input JSON to serde_json::Value
        let input_value: serde_json::Value = serde_json::from_str(&req.input_json)
            .map_err(|e| Status::invalid_argument(format!("Invalid input JSON: {}", e)))?;

        // Start the workflow asynchronously
        match workflow_runtime
            .start_workflow::<serde_json::Value, serde_json::Value>(
                workflow_id.to_string(),
                req.workflow_type.clone(),
                req.version,
                input_value,
            )
            .await
        {
            Ok(_) => {
                info!(self.logger, "Workflow started";
                    "workflow_id" => %workflow_id,
                    "cluster_id" => cluster_id
                );

                Ok(Response::new(RunWorkflowAsyncResponse {
                    success: true,
                    workflow_id: workflow_id.to_string(),
                    execution_cluster_id: cluster_id.to_string(),
                    error: String::new(),
                }))
            }
            Err(e) => {
                warn!(self.logger, "Failed to start workflow";
                    "error" => %e,
                    "workflow_type" => &req.workflow_type
                );

                Ok(Response::new(RunWorkflowAsyncResponse {
                    success: false,
                    workflow_id: String::new(),
                    execution_cluster_id: String::new(),
                    error: e.to_string(),
                }))
            }
        }
    }

    async fn run_workflow_sync(
        &self,
        request: Request<RunWorkflowRequest>,
    ) -> Result<Response<RunWorkflowResponse>, Status> {
        let req = request.into_inner();

        info!(self.logger, "RunWorkflowSync called";
            "workflow_type" => &req.workflow_type,
            "version" => req.version
        );

        // Generate workflow ID
        let workflow_id = Uuid::new_v4();

        // For now, use the default execution cluster (cluster_id=1)
        let cluster_id = 1u32;

        // Get the workflow runtime for this cluster
        let workflow_runtime = self
            .management_runtime
            .get_sub_cluster_runtime(&cluster_id)
            .await
            .ok_or_else(|| {
                Status::failed_precondition(format!(
                    "Execution cluster {} not available on this node",
                    cluster_id
                ))
            })?;

        // Parse input JSON to serde_json::Value
        let input_value: serde_json::Value = serde_json::from_str(&req.input_json)
            .map_err(|e| Status::invalid_argument(format!("Invalid input JSON: {}", e)))?;

        // Start the workflow and wait for completion
        match workflow_runtime
            .start_workflow::<serde_json::Value, serde_json::Value>(
                workflow_id.to_string(),
                req.workflow_type.clone(),
                req.version,
                input_value,
            )
            .await
        {
            Ok(workflow_run) => {
                // Wait for completion
                match workflow_run.wait_for_completion().await {
                    Ok(output_value) => {
                        // Serialize result to JSON
                        let result_json = serde_json::to_string(&output_value).map_err(|e| {
                            Status::internal(format!("Failed to serialize result: {}", e))
                        })?;

                        info!(self.logger, "Workflow completed successfully";
                            "workflow_id" => %workflow_id
                        );

                        Ok(Response::new(RunWorkflowResponse {
                            success: true,
                            result_json,
                            error: String::new(),
                        }))
                    }
                    Err(e) => {
                        warn!(self.logger, "Workflow execution failed";
                            "workflow_id" => %workflow_id,
                            "error" => %e
                        );

                        Ok(Response::new(RunWorkflowResponse {
                            success: false,
                            result_json: String::new(),
                            error: e.to_string(),
                        }))
                    }
                }
            }
            Err(e) => {
                warn!(self.logger, "Failed to start workflow";
                    "error" => %e,
                    "workflow_type" => &req.workflow_type
                );

                Ok(Response::new(RunWorkflowResponse {
                    success: false,
                    result_json: String::new(),
                    error: e.to_string(),
                }))
            }
        }
    }

    async fn wait_for_workflow_completion(
        &self,
        request: Request<WaitForWorkflowRequest>,
    ) -> Result<Response<RunWorkflowResponse>, Status> {
        let req = request.into_inner();

        info!(self.logger, "WaitForWorkflowCompletion called";
            "workflow_id" => &req.workflow_id,
            "execution_cluster_id" => &req.execution_cluster_id
        );

        // Parse workflow ID (validation)
        let _workflow_id_uuid = Uuid::parse_str(&req.workflow_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid workflow_id: {}", e)))?;

        // Parse execution cluster ID
        let execution_cluster_id = req
            .execution_cluster_id
            .parse::<u32>()
            .map_err(|e| {
                Status::invalid_argument(format!("Invalid execution_cluster_id: {}", e))
            })?;

        // Get the workflow runtime for this cluster
        let workflow_runtime = self
            .management_runtime
            .get_sub_cluster_runtime(&execution_cluster_id)
            .await
            .ok_or_else(|| {
                Status::failed_precondition(format!(
                    "Execution cluster {} not available on this node",
                    execution_cluster_id
                ))
            })?;

        // Determine timeout (default 60 seconds if 0 or not specified)
        let timeout_seconds = if req.timeout_seconds == 0 {
            60
        } else {
            req.timeout_seconds
        };
        let poll_interval = std::time::Duration::from_millis(100); // Poll every 100ms
        let max_polls = (timeout_seconds as u64 * 1000) / poll_interval.as_millis() as u64;

        // Poll for workflow completion
        for poll_count in 0..max_polls {
            // Check if workflow result is available
            if let Some(result_bytes) = workflow_runtime.get_result(&req.workflow_id).await {
                // Convert result bytes to JSON string
                let result_json = String::from_utf8(result_bytes).map_err(|e| {
                    Status::internal(format!("Failed to decode workflow result: {}", e))
                })?;

                info!(self.logger, "Workflow completed";
                    "workflow_id" => &req.workflow_id,
                    "poll_count" => poll_count
                );

                return Ok(Response::new(RunWorkflowResponse {
                    success: true,
                    result_json,
                    error: String::new(),
                }));
            }

            // Still running or not yet completed, sleep and retry
            tokio::time::sleep(poll_interval).await;
        }

        // Timeout reached
        warn!(self.logger, "Timeout waiting for workflow completion";
            "workflow_id" => &req.workflow_id,
            "timeout_seconds" => timeout_seconds
        );

        Ok(Response::new(RunWorkflowResponse {
            success: false,
            result_json: String::new(),
            error: format!(
                "Timeout waiting for workflow completion ({}s)",
                timeout_seconds
            ),
        }))
    }
}
