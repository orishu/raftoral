//! HTTP Server for receiving Raft messages

use crate::grpc::proto::GenericMessage;
use crate::http::messages::{
    HealthResponse, JsonGenericMessage, JsonRunWorkflowRequest, JsonRunWorkflowResponse,
    JsonWorkflowResultResponse, PeerInfo, RegisterNodeRequest, RegisterNodeResponse,
};
use crate::management::ManagementRuntime;
use crate::raft::generic::{ClusterRouter, Transport};
use crate::workflow::{WorkflowRegistry, WorkflowRuntime};
use axum::{
    extract::{Query, State},
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
            .route("/workflow/wait", get(handle_workflow_wait))
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
    info!(server.logger, "Workflow execute request";
        "name" => &req.name,
        "version" => req.version
    );

    // Generate workflow ID
    let workflow_id = uuid::Uuid::new_v4();

    // Get available execution clusters from management runtime
    let available_clusters = server.management_runtime.get_available_sub_clusters().await;

    if available_clusters.is_empty() {
        error!(server.logger, "No execution clusters available on this node");
        let response = JsonRunWorkflowResponse {
            workflow_id: String::new(),
            success: false,
            execution_cluster_id: None,
            result: None,
            error: Some("No execution clusters available on this node".to_string()),
        };
        return Json(response).into_response();
    }

    // Select a random execution cluster for load balancing
    use rand::Rng;
    let cluster_idx = rand::thread_rng().gen_range(0..available_clusters.len());
    let cluster_id = available_clusters[cluster_idx];

    debug!(server.logger, "Selected execution cluster";
        "cluster_id" => cluster_id,
        "available_clusters" => available_clusters.len()
    );

    // Get the workflow runtime for this cluster
    let workflow_runtime = match server
        .management_runtime
        .get_sub_cluster_runtime(&cluster_id)
        .await
    {
        Some(runtime) => runtime,
        None => {
            error!(server.logger, "Execution cluster not available";
                "cluster_id" => cluster_id
            );
            let response = JsonRunWorkflowResponse {
                workflow_id: String::new(),
                success: false,
                execution_cluster_id: None,
                result: None,
                error: Some(format!(
                    "Execution cluster {} not available on this node",
                    cluster_id
                )),
            };
            return Json(response).into_response();
        }
    };

    // Parse input JSON to serde_json::Value
    let input_value: serde_json::Value = match serde_json::from_str(&req.input) {
        Ok(value) => value,
        Err(e) => {
            error!(server.logger, "Invalid input JSON"; "error" => %e);
            let response = JsonRunWorkflowResponse {
                workflow_id: String::new(),
                success: false,
                execution_cluster_id: None,
                result: None,
                error: Some(format!("Invalid input JSON: {}", e)),
            };
            return Json(response).into_response();
        }
    };

    // Start the workflow asynchronously
    match workflow_runtime
        .start_workflow::<serde_json::Value, serde_json::Value>(
            workflow_id.to_string(),
            req.name.clone(),
            req.version,
            input_value,
        )
        .await
    {
        Ok(_) => {
            info!(server.logger, "Workflow started";
                "workflow_id" => %workflow_id,
                "cluster_id" => cluster_id
            );

            let response = JsonRunWorkflowResponse {
                workflow_id: workflow_id.to_string(),
                success: true,
                execution_cluster_id: Some(cluster_id.to_string()),
                result: None,
                error: None,
            };

            Json(response).into_response()
        }
        Err(e) => {
            error!(server.logger, "Failed to start workflow";
                "error" => %e,
                "workflow_type" => &req.name
            );

            let response = JsonRunWorkflowResponse {
                workflow_id: String::new(),
                success: false,
                execution_cluster_id: None,
                result: None,
                error: Some(e.to_string()),
            };

            Json(response).into_response()
        }
    }
}

/// Query parameters for workflow wait endpoint
#[derive(Debug, serde::Deserialize)]
struct WorkflowWaitQuery {
    workflow_id: String,
    execution_cluster_id: String,
    #[serde(default = "default_timeout")]
    timeout_seconds: u32,
}

fn default_timeout() -> u32 {
    60
}

/// Handle workflow wait/result request
async fn handle_workflow_wait(
    State(server): State<HttpServer>,
    Query(params): Query<WorkflowWaitQuery>,
) -> Response {
    info!(server.logger, "Workflow wait request";
        "workflow_id" => &params.workflow_id,
        "execution_cluster_id" => &params.execution_cluster_id,
        "timeout_seconds" => params.timeout_seconds
    );

    // Parse workflow ID (validation)
    if let Err(e) = uuid::Uuid::parse_str(&params.workflow_id) {
        error!(server.logger, "Invalid workflow_id"; "error" => %e);
        let response = JsonWorkflowResultResponse {
            success: false,
            result: None,
            error: Some(format!("Invalid workflow_id: {}", e)),
        };
        return (StatusCode::BAD_REQUEST, Json(response)).into_response();
    }

    // Parse execution cluster ID
    let execution_cluster_id = match params.execution_cluster_id.parse::<u32>() {
        Ok(id) => id,
        Err(e) => {
            error!(server.logger, "Invalid execution_cluster_id"; "error" => %e);
            let response = JsonWorkflowResultResponse {
                success: false,
                result: None,
                error: Some(format!("Invalid execution_cluster_id: {}", e)),
            };
            return (StatusCode::BAD_REQUEST, Json(response)).into_response();
        }
    };

    // Get the workflow runtime for this cluster
    let workflow_runtime = match server
        .management_runtime
        .get_sub_cluster_runtime(&execution_cluster_id)
        .await
    {
        Some(runtime) => runtime,
        None => {
            error!(server.logger, "Execution cluster not available";
                "cluster_id" => execution_cluster_id
            );
            let response = JsonWorkflowResultResponse {
                success: false,
                result: None,
                error: Some(format!(
                    "Execution cluster {} not available on this node",
                    execution_cluster_id
                )),
            };
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(response)).into_response();
        }
    };

    // Determine timeout (default 60 seconds if 0 or not specified)
    let timeout_seconds = if params.timeout_seconds == 0 {
        60
    } else {
        params.timeout_seconds
    };
    let poll_interval = std::time::Duration::from_millis(100); // Poll every 100ms
    let max_polls = (timeout_seconds as u64 * 1000) / poll_interval.as_millis() as u64;

    // Poll for workflow completion
    for poll_count in 0..max_polls {
        // Check if workflow result is available
        if let Some(result_bytes) = workflow_runtime.get_result(&params.workflow_id).await {
            // Convert result bytes to JSON string
            let result_json = match String::from_utf8(result_bytes) {
                Ok(json) => json,
                Err(e) => {
                    error!(server.logger, "Failed to decode workflow result"; "error" => %e);
                    let response = JsonWorkflowResultResponse {
                        success: false,
                        result: None,
                        error: Some(format!("Failed to decode workflow result: {}", e)),
                    };
                    return (StatusCode::INTERNAL_SERVER_ERROR, Json(response)).into_response();
                }
            };

            info!(server.logger, "Workflow completed";
                "workflow_id" => &params.workflow_id,
                "poll_count" => poll_count
            );

            let response = JsonWorkflowResultResponse {
                success: true,
                result: Some(result_json),
                error: None,
            };

            return Json(response).into_response();
        }

        // Still running or not yet completed, sleep and retry
        tokio::time::sleep(poll_interval).await;
    }

    // Timeout reached
    error!(server.logger, "Timeout waiting for workflow completion";
        "workflow_id" => &params.workflow_id,
        "timeout_seconds" => timeout_seconds
    );

    let response = JsonWorkflowResultResponse {
        success: false,
        result: None,
        error: Some(format!(
            "Timeout waiting for workflow completion ({}s)",
            timeout_seconds
        )),
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
