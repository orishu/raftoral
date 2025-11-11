//! gRPC Client (Layer 0) - Implements MessageSender for generic2 architecture
//!
//! This module provides a gRPC client that implements the MessageSender trait,
//! enabling the Transport layer to send Raft messages over gRPC.

use crate::grpc2::proto::{
    raft_service_client::RaftServiceClient, GenericMessage, MessageResponse,
};
use crate::raft::generic2::errors::TransportError;
use crate::raft::generic2::transport::MessageSender;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::Request;

/// gRPC client that implements MessageSender
///
/// This maintains a pool of gRPC client connections to peer nodes.
pub struct GrpcMessageSender {
    /// Connection pool: address → RaftServiceClient
    clients: Arc<Mutex<HashMap<String, RaftServiceClient<tonic::transport::Channel>>>>,
}

impl GrpcMessageSender {
    /// Create a new GrpcMessageSender
    pub fn new() -> Self {
        Self {
            clients: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get or create a client connection to the given address
    async fn get_client(
        &self,
        address: &str,
    ) -> Result<RaftServiceClient<tonic::transport::Channel>, TransportError> {
        let mut clients = self.clients.lock().await;

        // Return existing client if available
        if let Some(client) = clients.get(address) {
            return Ok(client.clone());
        }

        // Create new client connection
        let endpoint = format!("http://{}", address);
        let channel = tonic::transport::Channel::from_shared(endpoint.clone())
            .map_err(|e| TransportError::Other(
                format!("Invalid endpoint {}: {}", address, e)
            ))?
            .connect()
            .await
            .map_err(|e| TransportError::Other(
                format!("Connection failed to {}: {}", address, e)
            ))?;

        let client = RaftServiceClient::new(channel);
        clients.insert(address.to_string(), client.clone());

        Ok(client)
    }
}

impl Default for GrpcMessageSender {
    fn default() -> Self {
        Self::new()
    }
}

#[tonic::async_trait]
impl MessageSender for GrpcMessageSender {
    async fn send(&self, address: &str, message: GenericMessage) -> Result<(), TransportError> {
        // Get or create client connection
        let mut client = self.get_client(address).await?;

        // Send the message via gRPC
        let request = Request::new(message);
        let response: Result<tonic::Response<MessageResponse>, tonic::Status> =
            client.send_message(request).await;

        match response {
            Ok(resp) => {
                let msg_response = resp.into_inner();
                if msg_response.success {
                    Ok(())
                } else {
                    Err(TransportError::SendFailed {
                        node_id: 0, // We don't have node_id in this context
                        reason: msg_response.error,
                    })
                }
            }
            Err(status) => Err(TransportError::SendFailed {
                node_id: 0,
                reason: format!("gRPC error: {}", status),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grpc_message_sender_creation() {
        let sender = GrpcMessageSender::new();
        assert_eq!(sender.clients.try_lock().unwrap().len(), 0);
    }

    #[test]
    fn test_grpc_message_sender_default() {
        let sender = GrpcMessageSender::default();
        assert_eq!(sender.clients.try_lock().unwrap().len(), 0);
    }
}
