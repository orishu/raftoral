use tonic::{transport::Server, Request, Response, Status};
use std::sync::Arc;
use tokio::sync::oneshot;
use crate::raft::generic::grpc_transport::GrpcClusterTransport;
use crate::raft::generic::transport::ClusterTransport;
use crate::raft::generic::cluster::RaftCluster;
use crate::workflow::WorkflowCommandExecutor;

// Include the generated protobuf code
pub mod raft_proto {
    tonic::include_proto!("raftoral");
}

use raft_proto::{
    raft_service_server::{RaftService, RaftServiceServer},
    RaftMessage, MessageResponse, WorkflowCommand as ProtoWorkflowCommand,
    WorkflowStartData as ProtoWorkflowStartData,
    WorkflowEndData as ProtoWorkflowEndData,
    CheckpointData as ProtoCheckpointData,
    OwnerChangeData as ProtoOwnerChangeData,
    OwnerChangeReason as ProtoOwnerChangeReason,
    DiscoveryRequest, DiscoveryResponse, RaftRole as ProtoRaftRole,
};

use crate::workflow::commands::{
    WorkflowCommand, WorkflowStartData, WorkflowEndData,
    CheckpointData, OwnerChangeData, OwnerChangeReason,
};

/// gRPC service implementation for Raft communication
pub struct RaftServiceImpl {
    transport: Arc<GrpcClusterTransport<WorkflowCommandExecutor>>,
    cluster: Arc<RaftCluster<WorkflowCommandExecutor>>,
    node_id: u64,
    address: String,
}

impl RaftServiceImpl {
    pub fn new(
        transport: Arc<GrpcClusterTransport<WorkflowCommandExecutor>>,
        cluster: Arc<RaftCluster<WorkflowCommandExecutor>>,
        node_id: u64,
        address: String,
    ) -> Self {
        Self { transport, cluster, node_id, address }
    }
}

#[tonic::async_trait]
impl RaftService for RaftServiceImpl {
    async fn send_message(
        &self,
        request: Request<RaftMessage>,
    ) -> Result<Response<MessageResponse>, Status> {
        let raft_msg = request.into_inner();

        // Convert proto WorkflowCommand to our internal type if present
        let workflow_command = if let Some(proto_cmd) = raft_msg.command {
            Some(convert_proto_to_workflow_command(proto_cmd)
                .map_err(|e| Status::invalid_argument(e.to_string()))?)
        } else {
            None
        };

        // Deserialize the Raft message
        let raft_message: raft::prelude::Message =
            protobuf::Message::parse_from_bytes(&raft_msg.raft_message)
                .map_err(|e| Status::invalid_argument(format!("Failed to parse Raft message: {}", e)))?;

        // Get the sender for this node
        let sender = self.transport.get_node_sender(self.node_id).await
            .ok_or_else(|| Status::not_found(format!("Node {} not found", self.node_id)))?;

        // Create the appropriate Message enum variant
        let message = if let Some(cmd) = workflow_command {
            // This is a Propose message with a workflow command
            crate::raft::generic::message::Message::Propose {
                id: 0, // ID will be set by the cluster
                callback: None,
                sync_callback: None,
                command: cmd,
            }
        } else {
            // This is a Raft protocol message
            crate::raft::generic::message::Message::Raft(raft_message)
        };

        // Send the message to the node's receiver
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
        // Determine Raft role (simplified: just check if leader)
        let role = if self.cluster.is_leader().await {
            ProtoRaftRole::Leader
        } else {
            ProtoRaftRole::Follower
        };

        // Get highest known node ID from transport
        let node_ids = self.transport.node_ids().await;
        let highest_known_node_id = node_ids.iter().max().copied().unwrap_or(self.node_id);

        Ok(Response::new(DiscoveryResponse {
            node_id: self.node_id,
            role: role as i32,
            highest_known_node_id,
            address: self.address.clone(),
        }))
    }
}

/// Convert protobuf WorkflowCommand to internal WorkflowCommand
fn convert_proto_to_workflow_command(proto: ProtoWorkflowCommand) -> Result<WorkflowCommand, Box<dyn std::error::Error>> {
    let command = proto.command.ok_or("Missing command in WorkflowCommand")?;

    Ok(match command {
        raft_proto::workflow_command::Command::WorkflowStart(start) => {
            WorkflowCommand::WorkflowStart(WorkflowStartData {
                workflow_id: start.workflow_id,
                workflow_type: start.workflow_type,
                version: start.version,
                input: start.input,
                owner_node_id: start.owner_node_id,
            })
        }
        raft_proto::workflow_command::Command::WorkflowEnd(end) => {
            WorkflowCommand::WorkflowEnd(WorkflowEndData {
                workflow_id: end.workflow_id,
                result: end.result,
            })
        }
        raft_proto::workflow_command::Command::Checkpoint(checkpoint) => {
            WorkflowCommand::SetCheckpoint(CheckpointData {
                workflow_id: checkpoint.workflow_id,
                key: checkpoint.key,
                value: checkpoint.value,
            })
        }
        raft_proto::workflow_command::Command::OwnerChange(owner_change) => {
            let reason = match ProtoOwnerChangeReason::try_from(owner_change.reason) {
                Ok(ProtoOwnerChangeReason::NodeFailure) => OwnerChangeReason::NodeFailure,
                Ok(ProtoOwnerChangeReason::LoadBalancing) => OwnerChangeReason::LoadBalancing,
                Err(_) => return Err("Invalid OwnerChangeReason".into()),
            };

            WorkflowCommand::OwnerChange(OwnerChangeData {
                workflow_id: owner_change.workflow_id,
                old_owner_node_id: owner_change.old_owner_node_id,
                new_owner_node_id: owner_change.new_owner_node_id,
                reason,
            })
        }
    })
}

/// Convert internal WorkflowCommand to protobuf WorkflowCommand
pub fn convert_workflow_command_to_proto(cmd: &WorkflowCommand) -> ProtoWorkflowCommand {
    let command = match cmd {
        WorkflowCommand::WorkflowStart(start) => {
            raft_proto::workflow_command::Command::WorkflowStart(ProtoWorkflowStartData {
                workflow_id: start.workflow_id.clone(),
                workflow_type: start.workflow_type.clone(),
                version: start.version,
                input: start.input.clone(),
                owner_node_id: start.owner_node_id,
            })
        }
        WorkflowCommand::WorkflowEnd(end) => {
            raft_proto::workflow_command::Command::WorkflowEnd(ProtoWorkflowEndData {
                workflow_id: end.workflow_id.clone(),
                result: end.result.clone(),
            })
        }
        WorkflowCommand::SetCheckpoint(checkpoint) => {
            raft_proto::workflow_command::Command::Checkpoint(ProtoCheckpointData {
                workflow_id: checkpoint.workflow_id.clone(),
                key: checkpoint.key.clone(),
                value: checkpoint.value.clone(),
            })
        }
        WorkflowCommand::OwnerChange(owner_change) => {
            let reason = match owner_change.reason {
                OwnerChangeReason::NodeFailure => ProtoOwnerChangeReason::NodeFailure,
                OwnerChangeReason::LoadBalancing => ProtoOwnerChangeReason::LoadBalancing,
            };

            raft_proto::workflow_command::Command::OwnerChange(ProtoOwnerChangeData {
                workflow_id: owner_change.workflow_id.clone(),
                old_owner_node_id: owner_change.old_owner_node_id,
                new_owner_node_id: owner_change.new_owner_node_id,
                reason: reason as i32,
            })
        }
    };

    ProtoWorkflowCommand {
        command: Some(command),
    }
}

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
    transport: Arc<GrpcClusterTransport<WorkflowCommandExecutor>>,
    cluster: Arc<RaftCluster<WorkflowCommandExecutor>>,
    node_id: u64,
) -> Result<GrpcServerHandle, Box<dyn std::error::Error>> {
    let addr = address.parse()?;
    let service = RaftServiceImpl::new(transport, cluster, node_id, address);

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        Server::builder()
            .add_service(RaftServiceServer::new(service))
            .serve_with_shutdown(addr, async {
                shutdown_rx.await.ok();
            })
            .await
            .expect("gRPC server failed");
    });

    Ok(GrpcServerHandle { shutdown_tx })
}

/// Legacy function for backward compatibility (blocks until shutdown)
pub async fn start_grpc_server_blocking(
    address: String,
    transport: Arc<GrpcClusterTransport<WorkflowCommandExecutor>>,
    cluster: Arc<RaftCluster<WorkflowCommandExecutor>>,
    node_id: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr = address.parse()?;
    let service = RaftServiceImpl::new(transport, cluster, node_id, address);

    Server::builder()
        .add_service(RaftServiceServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}
