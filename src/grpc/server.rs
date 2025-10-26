use tonic::{transport::Server, Request, Response, Status};
use std::sync::Arc;
use tokio::sync::oneshot;
use crate::raft::generic::grpc_transport::GrpcClusterTransport;
use tonic_reflection::server::Builder as ReflectionBuilder;

// Include the generated protobuf code
pub mod raft_proto {
    tonic::include_proto!("raftoral");

    // File descriptor for gRPC reflection
    pub const FILE_DESCRIPTOR_SET: &[u8] = include_bytes!("../../target/descriptor.bin");
}

use raft_proto::{
    raft_service_server::{RaftService, RaftServiceServer},
    workflow_management_server::{WorkflowManagement, WorkflowManagementServer},
    GenericMessage, MessageResponse,
    DiscoveryRequest, DiscoveryResponse, RaftRole as ProtoRaftRole,
    RunWorkflowRequest, RunWorkflowResponse, RunWorkflowAsyncResponse, WaitForWorkflowRequest,
};

use crate::raft::generic::message::CommandExecutor;
use crate::raft::generic::cluster::RaftCluster;

/// gRPC service implementation for Raft communication
/// Now uses ClusterRouter for multi-cluster support
/// Phase 3: Transport is now type-parameter-free!
pub struct RaftServiceImpl<E: CommandExecutor> {
    #[allow(dead_code)]
    transport: Arc<GrpcClusterTransport>,
    cluster: Arc<RaftCluster<E>>,
    node_id: u64,
    address: String,
    /// Optional cluster router for multi-cluster routing (Phase 2)
    /// If None, falls back to single-cluster behavior (Phase 1)
    cluster_router: Option<Arc<crate::grpc::ClusterRouter>>,
}

impl<E: CommandExecutor> RaftServiceImpl<E> {
    pub fn new(
        transport: Arc<GrpcClusterTransport>,
        cluster: Arc<RaftCluster<E>>,
        node_id: u64,
        address: String,
    ) -> Self {
        Self { transport, cluster, node_id, address, cluster_router: None }
    }

    /// Create a new RaftServiceImpl with cluster routing support (Phase 2)
    pub fn with_cluster_router(
        transport: Arc<GrpcClusterTransport>,
        cluster: Arc<RaftCluster<E>>,
        node_id: u64,
        address: String,
        cluster_router: Arc<crate::grpc::ClusterRouter>,
    ) -> Self {
        Self { transport, cluster, node_id, address, cluster_router: Some(cluster_router) }
    }
}

#[tonic::async_trait]
impl<E: CommandExecutor + Default + 'static> RaftService for RaftServiceImpl<E> {
    async fn send_message(
        &self,
        request: Request<GenericMessage>,
    ) -> Result<Response<MessageResponse>, Status> {
        let proto_msg = request.into_inner();

        // Phase 2: Use cluster router if available, otherwise fallback to single-cluster
        if let Some(router) = &self.cluster_router {
            // Multi-cluster mode: route based on cluster_id
            router.route_message(proto_msg).await?;
        } else {
            // Single-cluster mode (backward compatibility)
            // Validate cluster_id is 0
            if proto_msg.cluster_id != 0 {
                return Err(Status::invalid_argument(
                    format!("Single-cluster mode: Only cluster_id=0 supported, got {}", proto_msg.cluster_id)
                ));
            }

            // Deserialize and send directly to cluster's local mailbox
            use crate::raft::generic::message::Message;
            let message = Message::<E::Command>::from_protobuf(proto_msg)
                .map_err(|e| Status::invalid_argument(format!("Failed to deserialize message: {}", e)))?;

            self.cluster.local_sender.send(message)
                .map_err(|e| Status::internal(format!("Failed to send message: {}", e)))?;
        }

        Ok(Response::new(MessageResponse {
            success: true,
            error: String::new(),
        }))
    }

    async fn discover(
        &self,
        _request: Request<DiscoveryRequest>,
    ) -> Result<Response<DiscoveryResponse>, Status> {
        // Get highest known node ID from Raft configuration (most accurate source)
        let node_ids = self.cluster.get_node_ids();
        let highest_known_node_id = node_ids.iter().copied().max().unwrap_or(self.node_id);

        // Get full configuration state (voters and learners)
        let conf_state = self.cluster.get_conf_state();
        let voters: Vec<u64> = conf_state.voters.into_iter().collect();
        let learners: Vec<u64> = conf_state.learners.into_iter().collect();

        Ok(Response::new(DiscoveryResponse {
            node_id: self.node_id,
            role: ProtoRaftRole::Follower as i32, // Role discovery requires cluster reference - simplified for now
            highest_known_node_id,
            address: self.address.clone(),
            voters,
            learners,
        }))
    }
}

/// gRPC service implementation for Workflow Management
pub struct WorkflowManagementImpl {
    runtime: Arc<crate::workflow::WorkflowRuntime>,
    node_manager: Option<Arc<crate::nodemanager::NodeManager>>,
    /// Request forwarder for proxying requests to other nodes
    forwarder: Arc<crate::grpc::RequestForwarder>,
}

impl WorkflowManagementImpl {
    pub fn new(runtime: Arc<crate::workflow::WorkflowRuntime>) -> Self {
        Self {
            runtime,
            node_manager: None,
            forwarder: Arc::new(crate::grpc::RequestForwarder::new()),
        }
    }

    pub fn with_node_manager(runtime: Arc<crate::workflow::WorkflowRuntime>, node_manager: Arc<crate::nodemanager::NodeManager>) -> Self {
        Self {
            runtime,
            node_manager: Some(node_manager),
            forwarder: Arc::new(crate::grpc::RequestForwarder::new()),
        }
    }
}

#[tonic::async_trait]
impl WorkflowManagement for WorkflowManagementImpl {
    async fn run_workflow_sync(
        &self,
        request: Request<RunWorkflowRequest>,
    ) -> Result<Response<RunWorkflowResponse>, Status> {
        let req = request.into_inner();

        // Parse input JSON to serde_json::Value
        let input_value: serde_json::Value = serde_json::from_str(&req.input_json)
            .map_err(|e| Status::invalid_argument(format!("Invalid input JSON: {}", e)))?;

        // Start the workflow with generic JSON input
        let workflow_run = self.runtime
            .start_workflow::<serde_json::Value, serde_json::Value>(
                &req.workflow_type,
                req.version,
                input_value,
            )
            .await
            .map_err(|e| Status::internal(format!("Failed to start workflow: {}", e)))?;

        // Wait for completion
        let result = workflow_run.wait_for_completion().await;

        match result {
            Ok(output_value) => {
                // Serialize result to JSON
                let result_json = serde_json::to_string(&output_value)
                    .map_err(|e| Status::internal(format!("Failed to serialize result: {}", e)))?;

                Ok(Response::new(RunWorkflowResponse {
                    success: true,
                    result_json,
                    error: String::new(),
                }))
            }
            Err(e) => {
                // Workflow failed - return error in payload, not gRPC status
                Ok(Response::new(RunWorkflowResponse {
                    success: false,
                    result_json: String::new(),
                    error: e.to_string(),
                }))
            }
        }
    }

    async fn run_workflow_async(
        &self,
        request: Request<RunWorkflowRequest>,
    ) -> Result<Response<RunWorkflowAsyncResponse>, Status> {
        let req = request.into_inner();

        // We need the node_manager to propose to the management cluster
        let node_manager = self.node_manager.as_ref()
            .ok_or_else(|| Status::failed_precondition("NodeManager not configured for async workflows"))?;

        // Generate a new workflow ID
        use uuid::Uuid;
        let workflow_id = Uuid::new_v4();

        // Choose an execution cluster using round-robin selection
        let (cluster_id, _selected_cluster) = node_manager.select_execution_cluster_round_robin();

        // Create the ScheduleWorkflowStart command
        use crate::nodemanager::*;
        let schedule_command = ManagementCommand::ScheduleWorkflowStart(ScheduleWorkflowData {
            workflow_id,
            cluster_id,
            workflow_type: req.workflow_type,
            version: req.version,
            input_json: req.input_json,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        });

        // Propose the ScheduleWorkflowStart command to the management cluster
        // This will be applied by all management nodes, but only the leader of the
        // execution cluster will actually propose the WorkflowStart command
        node_manager.propose_management_command(schedule_command).await
            .map_err(|e| Status::internal(format!("Failed to schedule workflow: {}", e)))?;

        Ok(Response::new(RunWorkflowAsyncResponse {
            success: true,
            workflow_id: workflow_id.to_string(),
            execution_cluster_id: cluster_id.to_string(),
            error: String::new(),
        }))
    }

    async fn wait_for_workflow_completion(
        &self,
        request: Request<WaitForWorkflowRequest>,
    ) -> Result<Response<RunWorkflowResponse>, Status> {
        let req = request.into_inner();

        // We need the node_manager to access the management cluster
        let node_manager = self.node_manager.as_ref()
            .ok_or_else(|| Status::failed_precondition("NodeManager not configured"))?;

        // Parse workflow ID (validation only, not used in this implementation)
        use uuid::Uuid;
        let _workflow_id_uuid = Uuid::parse_str(&req.workflow_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid workflow_id: {}", e)))?;

        // Parse execution cluster ID and route to correct cluster
        let execution_cluster_id = if !req.execution_cluster_id.is_empty() {
            Uuid::parse_str(&req.execution_cluster_id)
                .map_err(|e| Status::invalid_argument(format!("Invalid execution_cluster_id: {}", e)))?
        } else {
            // If no cluster_id provided, use default cluster (for backward compatibility)
            return Err(Status::invalid_argument("execution_cluster_id is required"));
        };

        // Determine timeout (default 60 seconds if 0 or not specified)
        let timeout_seconds = if req.timeout_seconds == 0 { 60 } else { req.timeout_seconds };
        let poll_interval = std::time::Duration::from_millis(100); // Poll every 100ms
        let max_polls = (timeout_seconds as u64 * 1000) / poll_interval.as_millis() as u64;

        // Try to get the execution cluster executor locally
        let workflow_executor = match node_manager.get_execution_cluster_executor(&execution_cluster_id) {
            Some(executor) => executor,
            None => {
                // This node doesn't have the execution cluster
                // Automatically forward the request to a node that does

                // Query management state to find nodes that have this cluster
                let management_executor = node_manager.management_executor();
                let cluster_info = management_executor.get_cluster_info(&execution_cluster_id)
                    .ok_or_else(|| Status::not_found(format!("Execution cluster {} not found in management state", execution_cluster_id)))?;

                if cluster_info.node_ids.is_empty() {
                    return Err(Status::not_found(format!("No nodes available for execution cluster {}", execution_cluster_id)));
                }

                // Get addresses for nodes that have this cluster
                let node_addresses = node_manager.get_node_addresses(&cluster_info.node_ids);

                if node_addresses.is_empty() {
                    return Err(Status::internal(format!(
                        "Found nodes {:?} for cluster {} but no addresses available",
                        cluster_info.node_ids, execution_cluster_id
                    )));
                }

                // Forward request to one of the nodes that has this cluster
                log::info!(
                    "Forwarding wait_for_workflow_completion request to nodes with cluster {}: {:?}",
                    execution_cluster_id, node_addresses
                );

                return self.forwarder.forward_to_any(
                    &node_addresses,
                    |addr| {
                        let forwarder = self.forwarder.clone();
                        let req_clone = WaitForWorkflowRequest {
                            workflow_id: req.workflow_id.clone(),
                            execution_cluster_id: req.execution_cluster_id.clone(),
                            timeout_seconds: req.timeout_seconds,
                        };
                        async move {
                            forwarder.forward_wait_for_workflow_completion(&addr, req_clone).await
                        }
                    }
                ).await;
            }
        };

        // Poll for workflow completion
        for _ in 0..max_polls {
            // Check if workflow result is available in execution cluster state
            let result_opt = workflow_executor.get_result(&req.workflow_id);

            if let Some(result_bytes) = result_opt {
                // Convert result bytes to JSON string
                let result_json = String::from_utf8(result_bytes)
                    .map_err(|e| Status::internal(format!("Failed to decode workflow result: {}", e)))?;

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
        Ok(Response::new(RunWorkflowResponse {
            success: false,
            result_json: String::new(),
            error: format!("Timeout waiting for workflow completion ({}s)", timeout_seconds),
        }))
    }
}

/// Type alias for server configuration function
pub type ServerConfigurator = Box<dyn Fn(Server) -> Server + Send + Sync>;

/// gRPC server handle with graceful shutdown support
pub struct GrpcServerHandle {
    shutdown_tx: oneshot::Sender<()>,
}

impl GrpcServerHandle {
    /// Trigger graceful shutdown of the server
    pub fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
    }
}

/// Start a gRPC server with multi-cluster routing support
pub async fn start_grpc_server(
    address: String,
    node_manager: Arc<crate::nodemanager::NodeManager>,
    node_id: u64,
) -> Result<GrpcServerHandle, Box<dyn std::error::Error>> {
    start_grpc_server_with_config(address, node_manager, node_id, None).await
}

/// Start a gRPC server with multi-cluster routing and custom server configuration
pub async fn start_grpc_server_with_config(
    address: String,
    node_manager: Arc<crate::nodemanager::NodeManager>,
    node_id: u64,
    server_config: Option<ServerConfigurator>,
) -> Result<GrpcServerHandle, Box<dyn std::error::Error>> {
    let addr = address.parse()?;

    // Get ClusterRouter from NodeManager
    let cluster_router = node_manager.cluster_router();

    // Create dummy transport for the service (not used with router)
    // Phase 3: Transport is now type-parameter-free!
    let nodes = vec![crate::raft::generic::grpc_transport::NodeConfig {
        node_id,
        address: address.clone(),
    }];
    let transport = Arc::new(GrpcClusterTransport::new(nodes));

    // Use the first available execution cluster for the RaftService
    // (This is primarily for backward compatibility with single-cluster tests)
    let initial_cluster_id = uuid::Uuid::from_u128(1); // INITIAL_EXECUTION_CLUSTER_ID
    let cluster = node_manager.get_execution_cluster(&initial_cluster_id)
        .expect("Initial execution cluster should exist");
    let raft_service = RaftServiceImpl::with_cluster_router(
        transport,
        cluster,
        node_id,
        address,
        cluster_router
    );
    let runtime = node_manager.workflow_runtime();
    let workflow_service = WorkflowManagementImpl::with_node_manager(runtime, node_manager.clone());

    // Create reflection service
    let reflection_service = ReflectionBuilder::configure()
        .register_encoded_file_descriptor_set(raft_proto::FILE_DESCRIPTOR_SET)
        .build_v1()?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        let mut server = Server::builder();

        // Apply custom configuration if provided
        if let Some(config_fn) = server_config {
            server = config_fn(server);
        }

        server
            .add_service(RaftServiceServer::new(raft_service))
            .add_service(WorkflowManagementServer::new(workflow_service))
            .add_service(reflection_service)
            .serve_with_shutdown(addr, async {
                shutdown_rx.await.ok();
            })
            .await
            .expect("gRPC server failed");
    });

    Ok(GrpcServerHandle { shutdown_tx })
}

