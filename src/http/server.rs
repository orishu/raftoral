//! HTTP Server for receiving Raft messages

use crate::grpc::proto::GenericMessage;
use crate::http::messages::{
    HealthResponse, JsonGenericMessage, JsonRunWorkflowRequest, JsonRunWorkflowResponse,
    PeerInfo, RegisterNodeRequest, RegisterNodeResponse,
};
use crate::raft::generic::Transport;
use crate::workflow::WorkflowRegistry;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use slog::{debug, error, info, Logger};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;

/// HTTP server state
#[derive(Clone)]
pub struct HttpServerState {
    transport: Arc<dyn Transport>,
    workflow_registry: Arc<Mutex<WorkflowRegistry>>,
    peer_registry: Arc<Mutex<HashMap<u64, String>>>,
    node_id: u64,
    logger: Logger,
}

/// HTTP server for receiving Raft messages
pub struct HttpServer {
    state: HttpServerState,
    address: SocketAddr,
    logger: Logger,
}

impl HttpServer {
    pub fn new(
        node_id: u64,
        address: SocketAddr,
        transport: Arc<dyn Transport>,
        workflow_registry: Arc<Mutex<WorkflowRegistry>>,
        logger: Logger,
    ) -> Self {
        let state = HttpServerState {
            transport,
            workflow_registry,
            peer_registry: Arc::new(Mutex::new(HashMap::new())),
            node_id,
            logger: logger.clone(),
        };

        Self {
            state,
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
            .with_state(self.state);

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
    State(state): State<HttpServerState>,
    Json(json_msg): Json<JsonGenericMessage>,
) -> Response {
    // Convert JSON to protobuf GenericMessage
    let message: GenericMessage = json_msg.into();

    debug!(state.logger, "Received Raft message via HTTP";
        "cluster_id" => message.cluster_id
    );

    // Forward to transport
    match state.transport.receive_message(message).await {
        Ok(_) => {
            debug!(state.logger, "Raft message processed successfully");
            StatusCode::OK.into_response()
        }
        Err(e) => {
            error!(state.logger, "Failed to process Raft message"; "error" => %e);
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Error: {}", e)).into_response()
        }
    }
}

/// Handle workflow execution request
async fn handle_workflow_execute(
    State(state): State<HttpServerState>,
    Json(req): Json<JsonRunWorkflowRequest>,
) -> Response {
    debug!(state.logger, "Workflow execute request";
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

/// Handle node registration for discovery
async fn handle_register_node(
    State(state): State<HttpServerState>,
    Json(req): Json<RegisterNodeRequest>,
) -> Response {
    debug!(state.logger, "Node registration request"; "address" => &req.address);

    // Assign node_id if not provided
    let node_id = req.node_id.unwrap_or_else(|| {
        // Generate next node_id (simple increment for now)
        let mut peers = state.peer_registry.blocking_lock();
        let next_id = peers.keys().max().map(|id| id + 1).unwrap_or(1);
        next_id
    });

    // Register peer
    {
        let mut peers = state.peer_registry.lock().await;
        peers.insert(node_id, req.address.clone());
    }

    // Get all peers
    let peers = state.peer_registry.lock().await;
    let peer_list: Vec<PeerInfo> = peers
        .iter()
        .map(|(id, addr)| PeerInfo {
            node_id: *id,
            address: addr.clone(),
        })
        .collect();

    let response = RegisterNodeResponse {
        node_id,
        peers: peer_list,
    };

    Json(response).into_response()
}

/// Get list of peers
async fn handle_get_peers(State(state): State<HttpServerState>) -> Response {
    let peers = state.peer_registry.lock().await;
    let peer_list: Vec<PeerInfo> = peers
        .iter()
        .map(|(id, addr)| PeerInfo {
            node_id: *id,
            address: addr.clone(),
        })
        .collect();

    Json(peer_list).into_response()
}

/// Health check endpoint
async fn handle_health(State(state): State<HttpServerState>) -> Response {
    let response = HealthResponse {
        status: "ok".to_string(),
        node_id: state.node_id,
        is_leader: false, // TODO: Get from Raft state
    };

    Json(response).into_response()
}
