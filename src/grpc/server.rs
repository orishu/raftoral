//! gRPC Server (Layer 0) - Receives Raft messages and routes to ClusterRouter
//!
//! This module provides a gRPC server that receives Raft messages via RaftService
//! and forwards them to the ClusterRouter for routing to the appropriate cluster.

use crate::grpc::proto::{
    raft_service_server::{RaftService, RaftServiceServer},
    AddNodeRequest, AddNodeResponse, GenericMessage, MessageResponse,
};
use crate::management::{ManagementRuntime, SubClusterRuntime};
use crate::raft::generic::{ClusterRouter, Transport};
use std::sync::Arc;
use tonic::{transport::Server, Request, Response, Status};

/// gRPC server implementation for generic architecture
///
/// This server receives GenericMessage via gRPC and routes them to
/// the appropriate cluster via ClusterRouter.
#[derive(Clone)]
pub struct GrpcServer<R>
where
    R: SubClusterRuntime,
{
    /// ClusterRouter for routing messages to the correct cluster
    cluster_router: Arc<ClusterRouter>,

    /// This node's ID
    node_id: u64,

    /// This node's address
    node_address: String,

    /// Transport for looking up peer addresses
    transport: Arc<dyn Transport>,

    /// Management runtime for cluster operations (e.g., adding nodes)
    management_runtime: Arc<ManagementRuntime<R>>,
}

impl<R> GrpcServer<R>
where
    R: SubClusterRuntime,
{
    /// Create a new GrpcServer
    ///
    /// # Arguments
    /// * `cluster_router` - The ClusterRouter for message routing
    /// * `node_id` - This node's ID
    /// * `node_address` - This node's network address
    /// * `transport` - Transport for looking up peer addresses
    /// * `management_runtime` - The ManagementRuntime for cluster operations
    pub fn new(
        cluster_router: Arc<ClusterRouter>,
        node_id: u64,
        node_address: String,
        transport: Arc<dyn Transport>,
        management_runtime: Arc<ManagementRuntime<R>>,
    ) -> Self {
        Self {
            cluster_router,
            node_id,
            node_address,
            transport,
            management_runtime,
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
impl<R> RaftService for GrpcServer<R>
where
    R: SubClusterRuntime,
{
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
        _request: Request<crate::grpc::proto::DiscoveryRequest>,
    ) -> Result<Response<crate::grpc::proto::DiscoveryResponse>, Status> {
        // Get leader information from management runtime
        // Check if we are the leader first (handles case where leader_id() returns None after bootstrap)
        let (leader_id, leader_address) = if self.management_runtime.is_leader().await {
            // We are the leader
            (self.node_id, self.node_address.clone())
        } else {
            // We're not the leader, try to find who is
            match self.management_runtime.leader_id().await {
                Some(lid) => {
                    // Look up leader address from transport peers
                    let addr = self.transport.get_peer_address(lid).await.unwrap_or_default();
                    (lid, addr)
                }
                None => (0, String::new()),
            }
        };

        // TODO: Track highest_known_node_id properly
        // For now, just return this node's ID as the highest known
        let highest_known = self.node_id;

        // Get voter information from management cluster
        let voters = self.management_runtime.get_management_voters().await;
        let current_voter_count = voters.len() as u64;
        let max_voters = 5u64; // Default from ManagementClusterConfig
        let should_join_as_voter = current_voter_count < max_voters;

        Ok(Response::new(
            crate::grpc::proto::DiscoveryResponse {
                node_id: self.node_id,
                highest_known_node_id: highest_known,
                address: self.node_address.clone(),
                management_leader_node_id: leader_id,
                management_leader_address: leader_address,
                should_join_as_voter,
                current_voter_count,
                max_voters,
            },
        ))
    }

    async fn add_node(
        &self,
        request: Request<AddNodeRequest>,
    ) -> Result<Response<AddNodeResponse>, Status> {
        let req = request.into_inner();

        // Call management runtime to add the node
        match self
            .management_runtime
            .add_node(req.node_id, req.address)
            .await
        {
            Ok(rx) => {
                // Wait for the operation to complete
                match rx.await {
                    Ok(Ok(_)) => Ok(Response::new(AddNodeResponse {
                        success: true,
                        error: String::new(),
                    })),
                    Ok(Err(e)) => Ok(Response::new(AddNodeResponse {
                        success: false,
                        error: format!("Failed to add node: {:?}", e),
                    })),
                    Err(e) => Ok(Response::new(AddNodeResponse {
                        success: false,
                        error: format!("Operation cancelled: {:?}", e),
                    })),
                }
            }
            Err(e) => Ok(Response::new(AddNodeResponse {
                success: false,
                error: format!("Failed to initiate add_node: {:?}", e),
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test removed - GrpcServer creation is tested via FullNode integration tests
    // Creating a standalone ManagementRuntime for unit testing is complex
}
