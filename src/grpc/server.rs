use tonic::{transport::Server, Request, Response, Status};
use std::sync::Arc;
use tokio::sync::oneshot;
use crate::raft::generic::grpc_transport::GrpcClusterTransport;

// Include the generated protobuf code
pub mod raft_proto {
    tonic::include_proto!("raftoral");
}

use raft_proto::{
    raft_service_server::{RaftService, RaftServiceServer},
    GenericMessage, MessageResponse,
    DiscoveryRequest, DiscoveryResponse, RaftRole as ProtoRaftRole,
};

use crate::raft::generic::message::{Message, SerializableMessage, CommandExecutor};

/// gRPC service implementation for Raft communication
pub struct RaftServiceImpl<E: CommandExecutor> {
    transport: Arc<GrpcClusterTransport<E>>,
    node_id: u64,
    address: String,
}

impl<E: CommandExecutor> RaftServiceImpl<E> {
    pub fn new(
        transport: Arc<GrpcClusterTransport<E>>,
        node_id: u64,
        address: String,
    ) -> Self {
        Self { transport, node_id, address }
    }
}

#[tonic::async_trait]
impl<E: CommandExecutor> RaftService for RaftServiceImpl<E> {
    async fn send_message(
        &self,
        request: Request<GenericMessage>,
    ) -> Result<Response<MessageResponse>, Status> {
        let generic_msg = request.into_inner();

        // Deserialize to SerializableMessage first
        let serializable: SerializableMessage<E::Command> = serde_json::from_slice(&generic_msg.serialized_message)
            .map_err(|e| Status::invalid_argument(format!("Failed to deserialize: {}", e)))?;

        // Convert to Message (callbacks will be None)
        let message = Message::from_serializable(serializable)
            .map_err(|e| Status::invalid_argument(format!("Failed to convert: {}", e)))?;

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
        // Get highest known node ID from transport's internal nodes list
        let all_nodes = self.transport.get_all_nodes().await;
        let highest_known_node_id = all_nodes.iter().map(|n| n.node_id).max().unwrap_or(self.node_id);

        Ok(Response::new(DiscoveryResponse {
            node_id: self.node_id,
            role: ProtoRaftRole::Follower as i32, // Role discovery requires cluster reference - simplified for now
            highest_known_node_id,
            address: self.address.clone(),
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

/// Start a gRPC server for a Raft node with graceful shutdown
pub async fn start_grpc_server<E: CommandExecutor + 'static>(
    address: String,
    transport: Arc<GrpcClusterTransport<E>>,
    node_id: u64,
) -> Result<GrpcServerHandle, Box<dyn std::error::Error>> {
    start_grpc_server_with_config(address, transport, node_id, None).await
}

/// Start a gRPC server with custom server configuration
pub async fn start_grpc_server_with_config<E: CommandExecutor + 'static>(
    address: String,
    transport: Arc<GrpcClusterTransport<E>>,
    node_id: u64,
    server_config: Option<ServerConfigurator>,
) -> Result<GrpcServerHandle, Box<dyn std::error::Error>> {
    let addr = address.parse()?;
    let service = RaftServiceImpl::new(transport, node_id, address);

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        let mut server = Server::builder();

        // Apply custom configuration if provided
        if let Some(config_fn) = server_config {
            server = config_fn(server);
        }

        server
            .add_service(RaftServiceServer::new(service))
            .serve_with_shutdown(addr, async {
                shutdown_rx.await.ok();
            })
            .await
            .expect("gRPC server failed");
    });

    Ok(GrpcServerHandle { shutdown_tx })
}

