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
    RunWorkflowRequest, RunWorkflowResponse,
};

use crate::raft::generic::message::CommandExecutor;
use crate::raft::generic::cluster::RaftCluster;

/// gRPC service implementation for Raft communication
/// Now uses ClusterRouter for multi-cluster support
/// Phase 3: Transport is now type-parameter-free!
pub struct RaftServiceImpl<E: CommandExecutor> {
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
}

impl WorkflowManagementImpl {
    pub fn new(runtime: Arc<crate::workflow::WorkflowRuntime>) -> Self {
        Self { runtime }
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

/// Start a gRPC server with multi-cluster routing support (Phase 2)
pub async fn start_grpc_server_with_router(
    address: String,
    node_manager: Arc<crate::nodemanager::NodeManager>,
    cluster_router: Arc<crate::grpc::ClusterRouter>,
    node_id: u64,
) -> Result<GrpcServerHandle, Box<dyn std::error::Error>> {
    start_grpc_server_with_router_and_config(address, node_manager, cluster_router, node_id, None).await
}

/// Start a gRPC server with multi-cluster routing and custom server configuration (Phase 2)
pub async fn start_grpc_server_with_router_and_config(
    address: String,
    node_manager: Arc<crate::nodemanager::NodeManager>,
    cluster_router: Arc<crate::grpc::ClusterRouter>,
    node_id: u64,
    server_config: Option<ServerConfigurator>,
) -> Result<GrpcServerHandle, Box<dyn std::error::Error>> {
    let addr = address.parse()?;

    // Create dummy transport for the service (not used with router)
    // Phase 3: Transport is now type-parameter-free!
    let nodes = vec![crate::raft::generic::grpc_transport::NodeConfig {
        node_id,
        address: address.clone(),
    }];
    let transport = Arc::new(GrpcClusterTransport::new(nodes));

    let cluster = node_manager.workflow_cluster.clone();
    let raft_service = RaftServiceImpl::with_cluster_router(
        transport,
        cluster,
        node_id,
        address,
        cluster_router
    );
    let runtime = node_manager.workflow_runtime();
    let workflow_service = WorkflowManagementImpl::new(runtime);

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

/// Start a gRPC server for a Raft node with graceful shutdown
/// Phase 3: Transport is now type-parameter-free!
pub async fn start_grpc_server(
    address: String,
    transport: Arc<GrpcClusterTransport>,
    node_manager: Arc<crate::nodemanager::NodeManager>,
    node_id: u64,
) -> Result<GrpcServerHandle, Box<dyn std::error::Error>> {
    start_grpc_server_with_config(address, transport, node_manager, node_id, None).await
}

/// Start a gRPC server with custom server configuration
/// Phase 3: Transport is now type-parameter-free!
pub async fn start_grpc_server_with_config(
    address: String,
    transport: Arc<GrpcClusterTransport>,
    node_manager: Arc<crate::nodemanager::NodeManager>,
    node_id: u64,
    server_config: Option<ServerConfigurator>,
) -> Result<GrpcServerHandle, Box<dyn std::error::Error>> {
    let addr = address.parse()?;
    let cluster = node_manager.workflow_cluster.clone();
    let raft_service = RaftServiceImpl::new(transport, cluster, node_id, address);
    let runtime = node_manager.workflow_runtime();
    let workflow_service = WorkflowManagementImpl::new(runtime);

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

