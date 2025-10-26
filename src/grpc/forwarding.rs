//! Request forwarding utilities for proxying RPC requests to other nodes.
//!
//! This module provides functionality to forward gRPC requests to nodes that
//! can actually handle them (e.g., nodes that have a specific execution cluster).

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Response, Status};

use crate::grpc::server::raft_proto::workflow_management_client::WorkflowManagementClient;
use crate::grpc::server::raft_proto::{WaitForWorkflowRequest, RunWorkflowResponse};

/// Manages connections to peer nodes for request forwarding
pub struct RequestForwarder {
    /// Cached gRPC clients by node address
    /// Using RwLock for concurrent reads, occasional writes
    clients: Arc<RwLock<HashMap<String, WorkflowManagementClient<Channel>>>>,
}

impl RequestForwarder {
    /// Create a new request forwarder
    pub fn new() -> Self {
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get or create a WorkflowManagement client for the given address
    async fn get_or_create_client(
        &self,
        address: &str,
    ) -> Result<WorkflowManagementClient<Channel>, Status> {
        // First try to get existing client (fast path with read lock)
        {
            let clients = self.clients.read().await;
            if let Some(client) = clients.get(address) {
                return Ok(client.clone());
            }
        }

        // Client doesn't exist, create it (slow path with write lock)
        let mut clients = self.clients.write().await;

        // Double-check in case another task created it while we waited for write lock
        if let Some(client) = clients.get(address) {
            return Ok(client.clone());
        }

        // Create new client
        let endpoint = Endpoint::from_shared(format!("http://{}", address))
            .map_err(|e| Status::internal(format!("Invalid endpoint: {}", e)))?;

        let channel = endpoint
            .connect()
            .await
            .map_err(|e| Status::unavailable(format!("Failed to connect to {}: {}", address, e)))?;

        let client = WorkflowManagementClient::new(channel);
        clients.insert(address.to_string(), client.clone());

        Ok(client)
    }

    /// Forward a WaitForWorkflowCompletion request to another node
    ///
    /// # Arguments
    /// * `target_address` - Address of the node to forward to (e.g., "192.168.1.10:5001")
    /// * `request` - The original request to forward
    ///
    /// # Returns
    /// The response from the target node, or an error if forwarding fails
    pub async fn forward_wait_for_workflow_completion(
        &self,
        target_address: &str,
        request: WaitForWorkflowRequest,
    ) -> Result<Response<RunWorkflowResponse>, Status> {
        let mut client = self.get_or_create_client(target_address).await?;

        // Forward the request
        client
            .wait_for_workflow_completion(Request::new(request))
            .await
    }

    /// Forward to any available node from a list
    ///
    /// Tries each node in order until one succeeds or all fail.
    /// This provides resilience if some nodes are down.
    ///
    /// # Arguments
    /// * `node_addresses` - List of node addresses to try
    /// * `forward_fn` - Async function that forwards to a specific address
    ///
    /// # Returns
    /// The response from the first successful node, or an error if all fail
    pub async fn forward_to_any<F, Fut, T>(
        &self,
        node_addresses: &[String],
        forward_fn: F,
    ) -> Result<Response<T>, Status>
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = Result<Response<T>, Status>>,
    {
        if node_addresses.is_empty() {
            return Err(Status::not_found("No nodes available for forwarding"));
        }

        let mut last_error = None;

        for address in node_addresses {
            match forward_fn(address.clone()).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    log::debug!("Failed to forward to {}: {}", address, e);
                    last_error = Some(e);
                    // Continue to next node
                }
            }
        }

        // All nodes failed
        Err(last_error.unwrap_or_else(|| Status::unavailable("All forwarding attempts failed")))
    }

    /// Clear cached clients (useful for testing or handling network changes)
    pub async fn clear_cache(&self) {
        self.clients.write().await.clear();
    }

    /// Get the number of cached clients
    pub async fn cached_client_count(&self) -> usize {
        self.clients.read().await.len()
    }
}

impl Default for RequestForwarder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_forwarder_creation() {
        let forwarder = RequestForwarder::new();
        assert_eq!(forwarder.cached_client_count().await, 0);
    }

    #[tokio::test]
    async fn test_clear_cache() {
        let forwarder = RequestForwarder::new();
        forwarder.clear_cache().await;
        assert_eq!(forwarder.cached_client_count().await, 0);
    }
}
