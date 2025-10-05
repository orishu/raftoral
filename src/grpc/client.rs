use tonic::transport::Channel;
use crate::grpc::server::raft_proto::{
    raft_service_client::RaftServiceClient, RaftMessage,
};

/// gRPC client for sending Raft messages to peer nodes
pub struct RaftClient {
    client: RaftServiceClient<Channel>,
}

impl RaftClient {
    /// Create a new gRPC client connected to the specified address
    pub async fn connect(address: String) -> Result<Self, Box<dyn std::error::Error>> {
        let client = RaftServiceClient::connect(format!("http://{}", address)).await?;
        Ok(Self { client })
    }

    /// Send a Raft message to the peer node
    pub async fn send_raft_message(
        &mut self,
        raft_msg: RaftMessage,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let response = self.client.send_message(raft_msg).await?;
        let msg_response = response.into_inner();

        if !msg_response.success {
            return Err(msg_response.error.into());
        }

        Ok(())
    }
}
