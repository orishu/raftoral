//! gRPC Server (Layer 0) - Receives Raft messages and routes to ClusterRouter
//!
//! This module provides a gRPC server that receives Raft messages via RaftService
//! and forwards them to the ClusterRouter for routing to the appropriate cluster.

use crate::grpc::server::raft_proto::{
    raft_service_server::{RaftService, RaftServiceServer},
    GenericMessage, MessageResponse,
};
use crate::raft::generic2::ClusterRouter;
use std::sync::Arc;
use tonic::{transport::Server, Request, Response, Status};

/// gRPC server implementation for generic2 architecture
///
/// This server receives GenericMessage via gRPC and routes them to
/// the appropriate cluster via ClusterRouter.
pub struct GrpcServer {
    /// ClusterRouter for routing messages to the correct cluster
    cluster_router: Arc<ClusterRouter>,

    /// Optional node ID for logging/debugging
    node_id: Option<u64>,
}

impl GrpcServer {
    /// Create a new GrpcServer
    ///
    /// # Arguments
    /// * `cluster_router` - The ClusterRouter for message routing
    pub fn new(cluster_router: Arc<ClusterRouter>) -> Self {
        Self {
            cluster_router,
            node_id: None,
        }
    }

    /// Create a new GrpcServer with node ID
    pub fn new_with_node_id(cluster_router: Arc<ClusterRouter>, node_id: u64) -> Self {
        Self {
            cluster_router,
            node_id: Some(node_id),
        }
    }

    /// Start the gRPC server on the given address
    ///
    /// # Arguments
    /// * `address` - Address to bind to (e.g., "0.0.0.0:5001")
    ///
    /// # Returns
    /// Server handle that can be awaited
    pub async fn serve(
        self,
        address: String,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let addr = address.parse()?;
        let service = RaftServiceServer::new(self);

        Server::builder()
            .add_service(service)
            .serve(addr)
            .await?;

        Ok(())
    }
}

#[tonic::async_trait]
impl RaftService for GrpcServer {
    async fn send_message(
        &self,
        request: Request<GenericMessage>,
    ) -> Result<Response<MessageResponse>, Status> {
        let message = request.into_inner();

        // Route the message to the appropriate cluster via ClusterRouter
        match self.cluster_router.route_message(message).await {
            Ok(_) => Ok(Response::new(MessageResponse {
                success: true,
                error: String::new(),
            })),
            Err(e) => {
                // Return success=false with error message
                Ok(Response::new(MessageResponse {
                    success: false,
                    error: format!("{:?}", e),
                }))
            }
        }
    }

    async fn discover(
        &self,
        _request: Request<crate::grpc::server::raft_proto::DiscoveryRequest>,
    ) -> Result<Response<crate::grpc::server::raft_proto::DiscoveryResponse>, Status> {
        // Basic implementation - can be enhanced later
        Ok(Response::new(
            crate::grpc::server::raft_proto::DiscoveryResponse {
                node_id: self.node_id.unwrap_or(0),
                highest_known_node_id: self.node_id.unwrap_or(0),
                address: String::new(),
                management_leader_node_id: 0,
                management_leader_address: String::new(),
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grpc_server_creation() {
        let router = Arc::new(ClusterRouter::new());
        let server = GrpcServer::new(router);
        assert!(server.node_id.is_none());
    }

    #[test]
    fn test_grpc_server_creation_with_node_id() {
        let router = Arc::new(ClusterRouter::new());
        let server = GrpcServer::new_with_node_id(router, 42);
        assert_eq!(server.node_id, Some(42));
    }
}
