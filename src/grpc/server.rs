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

use crate::raft::generic::message::{Message, CommandExecutor};
use crate::raft::generic::cluster::RaftCluster;

/// gRPC service implementation for Raft communication
pub struct RaftServiceImpl<C, E>
where
    C: Clone + std::fmt::Debug + serde::Serialize + for<'de> serde::Deserialize<'de> + Send + Sync + 'static,
    E: CommandExecutor<Command = C>,
{
    transport: Arc<GrpcClusterTransport<Message<C>>>,
    cluster: Arc<RaftCluster<E>>,
    node_id: u64,
    address: String,
}

impl<C, E> RaftServiceImpl<C, E>
where
    C: Clone + std::fmt::Debug + serde::Serialize + for<'de> serde::Deserialize<'de> + Send + Sync + 'static,
    E: CommandExecutor<Command = C>,
{
    pub fn new(
        transport: Arc<GrpcClusterTransport<Message<C>>>,
        cluster: Arc<RaftCluster<E>>,
        node_id: u64,
        address: String,
    ) -> Self {
        Self { transport, cluster, node_id, address }
    }
}

#[tonic::async_trait]
impl<C, E> RaftService for RaftServiceImpl<C, E>
where
    C: Clone + std::fmt::Debug + serde::Serialize + for<'de> serde::Deserialize<'de> + Send + Sync + 'static,
    E: CommandExecutor<Command = C>,
{
    async fn send_message(
        &self,
        request: Request<GenericMessage>,
    ) -> Result<Response<MessageResponse>, Status> {
        let generic_msg = request.into_inner();

        // Deserialize SerializableMessage which contains the command
        let serializable_msg: crate::raft::generic::message::SerializableMessage<C> =
            serde_json::from_slice(&generic_msg.serialized_message)
                .map_err(|e| Status::invalid_argument(format!("Failed to deserialize: {}", e)))?;

        // Convert to Message<C> (with callback: None)
        let message = Message::from_serializable(serializable_msg)
            .map_err(|e| Status::internal(format!("Failed to convert message: {}", e)))?;

        // Get the sender for this node
        let sender = self.transport.get_node_sender(self.node_id).await
            .ok_or_else(|| Status::not_found(format!("Node {} not found", self.node_id)))?;

        // Send the message to the node's local mailbox
        sender.send(message)
            .map_err(|e| Status::internal(format!("Failed to send message: {}", e)))?;

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

/// Start a gRPC server for a Raft node with graceful shutdown
pub async fn start_grpc_server(
    address: String,
    transport: Arc<GrpcClusterTransport<Message<crate::workflow::WorkflowCommand>>>,
    node_manager: Arc<crate::nodemanager::NodeManager>,
    node_id: u64,
) -> Result<GrpcServerHandle, Box<dyn std::error::Error>> {
    start_grpc_server_with_config(address, transport, node_manager, node_id, None).await
}

/// Start a gRPC server with custom server configuration
pub async fn start_grpc_server_with_config(
    address: String,
    transport: Arc<GrpcClusterTransport<Message<crate::workflow::WorkflowCommand>>>,
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

