//! gRPC Server (Layer 0) - Receives Raft messages and routes to ClusterRouter
//!
//! This module provides a gRPC server that receives Raft messages via RaftService
//! and forwards them to the ClusterRouter for routing to the appropriate cluster.

use crate::grpc::server::raft_proto::{
    raft_service_server::{RaftService, RaftServiceServer},
    AddNodeRequest, AddNodeResponse, GenericMessage, MessageResponse,
};
use crate::management::ManagementRuntime;
use crate::raft::generic2::ClusterRouter;
use crate::workflow2::WorkflowRuntime;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::{transport::Server, Request, Response, Status};

/// Cached leader information for discovery endpoint
#[derive(Clone, Debug)]
struct LeaderInfo {
    /// Current leader node ID (0 if unknown)
    leader_id: u64,
    /// Current leader address (empty if unknown)
    leader_address: String,
}

/// gRPC server implementation for generic2 architecture
///
/// This server receives GenericMessage via gRPC and routes them to
/// the appropriate cluster via ClusterRouter.
#[derive(Clone)]
pub struct GrpcServer {
    /// ClusterRouter for routing messages to the correct cluster
    cluster_router: Arc<ClusterRouter>,

    /// This node's ID
    node_id: u64,

    /// This node's address
    node_address: String,

    /// Cached management cluster leader information
    /// Updated by background task listening to role changes
    management_leader: Arc<Mutex<LeaderInfo>>,

    /// Map of node_id -> address for all known nodes
    /// Used to resolve leader_id to leader_address
    peer_addresses: Arc<Mutex<HashMap<u64, String>>>,

    /// Management runtime for cluster operations (e.g., adding nodes)
    management_runtime: Arc<ManagementRuntime<WorkflowRuntime>>,
}

impl GrpcServer {
    /// Create a new GrpcServer
    ///
    /// # Arguments
    /// * `cluster_router` - The ClusterRouter for message routing
    /// * `node_id` - This node's ID
    /// * `node_address` - This node's network address
    /// * `management_runtime` - The ManagementRuntime for cluster operations
    pub fn new(
        cluster_router: Arc<ClusterRouter>,
        node_id: u64,
        node_address: String,
        management_runtime: Arc<ManagementRuntime<WorkflowRuntime>>,
    ) -> Self {
        Self {
            cluster_router,
            node_id,
            node_address,
            management_leader: Arc::new(Mutex::new(LeaderInfo {
                leader_id: 0,
                leader_address: String::new(),
            })),
            peer_addresses: Arc::new(Mutex::new(HashMap::new())),
            management_runtime,
        }
    }

    /// Start background task to track management cluster leadership
    ///
    /// # Arguments
    /// * `management_node` - The management cluster RaftNode
    /// * `transport` - Transport to query peer addresses
    pub fn start_leader_tracker<SM>(
        self: &Arc<Self>,
        management_node: Arc<Mutex<crate::raft::generic2::RaftNode<SM>>>,
        transport: Arc<dyn crate::raft::generic2::Transport>,
    ) where
        SM: crate::raft::generic2::StateMachine + 'static,
    {
        let server = Arc::clone(self);

        tokio::spawn(async move {
            // Subscribe to role changes
            let mut role_rx = {
                let node = management_node.lock().await;
                node.subscribe_role_changes()
            };

            loop {
                // Check current leader
                let leader_id = {
                    let node = management_node.lock().await;
                    node.leader_id()
                };

                // Update cached leader info
                if let Some(lid) = leader_id {
                    let leader_address = if lid == server.node_id {
                        // We are the leader
                        server.node_address.clone()
                    } else {
                        // Look up leader address from transport peers
                        transport.get_peer_address(lid).await.unwrap_or_default()
                    };

                    let mut leader = server.management_leader.lock().await;
                    leader.leader_id = lid;
                    leader.leader_address = leader_address;
                }

                // Wait for role change
                match role_rx.recv().await {
                    Ok(_) => {
                        // Role changed, loop will check leader again
                        continue;
                    }
                    Err(_) => {
                        // Channel closed, exit
                        break;
                    }
                }
            }
        });
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
        // Get cached leader information
        let leader = self.management_leader.lock().await.clone();

        // TODO: Track highest_known_node_id properly
        // For now, just return this node's ID as the highest known
        let highest_known = self.node_id;

        Ok(Response::new(
            crate::grpc::server::raft_proto::DiscoveryResponse {
                node_id: self.node_id,
                highest_known_node_id: highest_known,
                address: self.node_address.clone(),
                management_leader_node_id: leader.leader_id,
                management_leader_address: leader.leader_address,
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
