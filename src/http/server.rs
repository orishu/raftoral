//! HTTP Server for receiving Raft messages

use crate::grpc::proto::GenericMessage;
use crate::http::messages::{
    HealthResponse, JsonGenericMessage, JsonRunWorkflowRequest, JsonRunWorkflowResponse,
    PeerInfo, RegisterNodeRequest, RegisterNodeResponse,
};
use crate::management::ManagementRuntime;
use crate::raft::generic::{ClusterRouter, Transport};
use crate::workflow::{WorkflowRegistry, WorkflowRuntime};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use slog::{debug, error, info, Logger};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;

/// HTTP server for receiving Raft messages
#[derive(Clone)]
pub struct HttpServer {
    /// ClusterRouter for routing messages to the correct cluster
    cluster_router: Arc<ClusterRouter>,
    /// Transport for looking up peer addresses
    transport: Arc<dyn Transport>,
    /// Management runtime for cluster operations
    management_runtime: Arc<ManagementRuntime<WorkflowRuntime>>,
    /// Shared workflow registry
    workflow_registry: Arc<Mutex<WorkflowRegistry>>,
    /// This node's ID
    node_id: u64,
    /// This node's address
    node_address: String,
    /// Bind address
    address: SocketAddr,
    /// Logger
    logger: Logger,
}

impl HttpServer {
    pub fn new(
        node_id: u64,
        address: SocketAddr,
        node_address: String,
        cluster_router: Arc<ClusterRouter>,
        transport: Arc<dyn Transport>,
        management_runtime: Arc<ManagementRuntime<WorkflowRuntime>>,
        workflow_registry: Arc<Mutex<WorkflowRegistry>>,
        logger: Logger,
    ) -> Self {
        Self {
            cluster_router,
            transport,
            management_runtime,
            workflow_registry,
            node_id,
            node_address,
            address,
            logger,
        }
    }

    pub async fn start(self) -> Result<(), Box<dyn std::error::Error>> {
        info!(self.logger, "Starting HTTP server"; "address" => %self.address);

        let app = Router::new()
            // Raft endpoints
            .route("/raft/message", post(handle_raft_message))
            // Workflow endpoints
            .route("/workflow/execute", post(handle_workflow_execute))
            // Discovery endpoints
            .route("/discovery/register", post(handle_register_node))
            .route("/discovery/peers", get(handle_get_peers))
            // Health check
            .route("/health", get(handle_health))
            // Add CORS support for browser environments
            .layer(CorsLayer::permissive())
            .with_state(self.clone());

        info!(self.logger, "HTTP server routes configured");

        // Start server
        let listener = tokio::net::TcpListener::bind(self.address)
            .await
            .map_err(|e| {
                error!(self.logger, "Failed to bind HTTP server"; "error" => %e);
                e
            })?;

        info!(self.logger, "HTTP server listening"; "address" => %self.address);

        axum::serve(listener, app)
            .await
            .map_err(|e| {
                error!(self.logger, "HTTP server error"; "error" => %e);
                e
            })?;

        Ok(())
    }
}

/// Handle incoming Raft message
async fn handle_raft_message(
    State(server): State<HttpServer>,
    Json(json_msg): Json<JsonGenericMessage>,
) -> Response {
    // Convert JSON to protobuf GenericMessage
    let message: GenericMessage = json_msg.into();

    debug!(server.logger, "Received Raft message via HTTP";
        "cluster_id" => message.cluster_id
    );

    // Route the message to the appropriate cluster via ClusterRouter
    match server.cluster_router.route_message(message).await {
        Ok(_) => {
            debug!(server.logger, "Raft message processed successfully");
            StatusCode::OK.into_response()
        }
        Err(e) => {
            error!(server.logger, "Failed to process Raft message"; "error" => %e);
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Error: {}", e)).into_response()
        }
    }
}

/// Handle workflow execution request
async fn handle_workflow_execute(
    State(server): State<HttpServer>,
    Json(req): Json<JsonRunWorkflowRequest>,
) -> Response {
    debug!(server.logger, "Workflow execute request";
        "name" => &req.name,
        "version" => req.version
    );

    // TODO: Integrate with workflow execution
    // For now, return a placeholder response
    let response = JsonRunWorkflowResponse {
        workflow_id: format!("wf-{}-{}", req.name, uuid::Uuid::new_v4()),
        success: true,
        result: None,
        error: None,
    };

    Json(response).into_response()
}

/// Handle node registration - adds node to management cluster
async fn handle_register_node(
    State(server): State<HttpServer>,
    Json(req): Json<RegisterNodeRequest>,
) -> Response {
    let node_id = match req.node_id {
        Some(id) => id,
        None => {
            error!(server.logger, "Add node request missing node_id");
            return (
                StatusCode::BAD_REQUEST,
                "node_id is required".to_string(),
            )
                .into_response();
        }
    };

    // For HTTP transport, store address with http:// prefix so Raft messages work
    let full_address = format!("http://{}", req.address);

    debug!(server.logger, "Add node request";
        "node_id" => node_id,
        "address" => &full_address
    );

    // Call management runtime to add the node (will be added to transport via ConfChange)
    match server
        .management_runtime
        .add_node(node_id, full_address.clone())
        .await
    {
        Ok(rx) => {
            // Wait for the operation to complete
            match rx.await {
                Ok(Ok(_)) => {
                    debug!(server.logger, "Node added successfully"; "node_id" => node_id);

                    // Return success with empty peer list (will be populated via discovery)
                    let response = RegisterNodeResponse {
                        node_id,
                        peers: vec![],
                    };
                    Json(response).into_response()
                }
                Ok(Err(e)) => {
                    error!(server.logger, "Failed to add node"; "error" => ?e);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Failed to add node: {:?}", e),
                    )
                        .into_response()
                }
                Err(e) => {
                    error!(server.logger, "Add node operation cancelled"; "error" => ?e);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Operation cancelled: {:?}", e),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            error!(server.logger, "Failed to initiate add_node"; "error" => ?e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to initiate add_node: {:?}", e),
            )
                .into_response()
        }
    }
}

/// Discovery endpoint - returns information for joining nodes
async fn handle_get_peers(State(server): State<HttpServer>) -> Response {
    debug!(server.logger, "Discovery request received");

    // Get leader information from management runtime
    // Check if we are the leader first (handles case where leader_id() returns None after bootstrap)
    let (leader_id, leader_address) = if server.management_runtime.is_leader().await {
        // We are the leader
        (server.node_id, server.node_address.clone())
    } else {
        // We're not the leader, try to find who is
        match server.management_runtime.leader_id().await {
            Some(lid) => {
                // Look up leader address from transport peers
                let addr = server.transport.get_peer_address(lid).await.unwrap_or_default();
                // Strip http:// prefix if present (HTTP transport stores full URLs)
                let clean_addr = addr.strip_prefix("http://").unwrap_or(&addr).to_string();
                (lid, clean_addr)
            }
            None => {
                debug!(server.logger, "No leader ID available");
                (0, String::new())
            },
        }
    };

    // Get voter information from management cluster
    let voters = server.management_runtime.get_management_voters().await;
    let current_voter_count = voters.len() as u64;
    let max_voters = 5u64; // Default from ManagementClusterConfig
    let should_join_as_voter = current_voter_count < max_voters;

    // Build discovery response as JSON
    let discovery_info = serde_json::json!({
        "node_id": server.node_id,
        "highest_known_node_id": server.node_id, // TODO: Track properly
        "address": server.node_address,
        "management_leader_node_id": leader_id,
        "management_leader_address": leader_address,
        "should_join_as_voter": should_join_as_voter,
        "current_voter_count": current_voter_count,
        "max_voters": max_voters,
    });

    Json(discovery_info).into_response()
}

/// Health check endpoint
async fn handle_health(State(server): State<HttpServer>) -> Response {
    let is_leader = server.management_runtime.is_leader().await;

    let response = HealthResponse {
        status: "ok".to_string(),
        node_id: server.node_id,
        is_leader,
    };

    Json(response).into_response()
}
