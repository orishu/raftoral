//! Sidecar gRPC Server Implementation
//!
//! Provides bidirectional streaming gRPC server for app ↔ sidecar communication.

use super::proto::raftoral_sidecar_server::{RaftoralSidecar, RaftoralSidecarServer};
use super::proto::{AppMessage, ExecuteWorkflowRequest as ProtoExecuteWorkflowRequest, SidecarMessage};
use crate::workflow_proxy_runtime::{ExecutionRequest, ExecutionResult};
use slog::{debug, error, info, Logger};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio_stream::{wrappers::ReceiverStream, Stream, StreamExt};
use tonic::{transport::Server, Request, Response, Status};

type ResponseStream = Pin<Box<dyn Stream<Item = Result<SidecarMessage, Status>> + Send>>;

/// Sidecar server for handling app communication
pub struct SidecarServer {
    /// Server address (e.g., "127.0.0.1:9001")
    address: String,
    /// Logger
    logger: Logger,
    /// Active streams (app_id → sender channel)
    streams: Arc<Mutex<std::collections::HashMap<String, mpsc::Sender<SidecarMessage>>>>,
    /// Receiver for execution requests from WorkflowProxyRuntime
    execution_request_rx: Option<mpsc::Receiver<ExecutionRequest>>,
    /// Sender for execution results to WorkflowProxyRuntime
    execution_result_tx: Option<mpsc::Sender<ExecutionResult>>,
}

impl SidecarServer {
    /// Create a new sidecar server
    pub fn new(address: String, logger: Logger) -> Self {
        Self {
            address,
            logger,
            streams: Arc::new(Mutex::new(std::collections::HashMap::new())),
            execution_request_rx: None,
            execution_result_tx: None,
        }
    }

    /// Set the channels for communication with WorkflowProxyRuntime
    pub fn set_proxy_channels(
        &mut self,
        execution_request_rx: mpsc::Receiver<ExecutionRequest>,
        execution_result_tx: mpsc::Sender<ExecutionResult>,
    ) {
        self.execution_request_rx = Some(execution_request_rx);
        self.execution_result_tx = Some(execution_result_tx);
    }

    /// Start the sidecar server
    pub async fn start(mut self) -> Result<(), Box<dyn std::error::Error>> {
        let addr = self.address.parse()?;

        info!(self.logger, "Starting sidecar server"; "address" => %addr);

        let service = SidecarServiceImpl {
            logger: self.logger.clone(),
            streams: self.streams.clone(),
            execution_result_tx: self.execution_result_tx.clone(),
        };

        // Start execution request forwarder task if channels are set
        if let Some(mut exec_req_rx) = self.execution_request_rx.take() {
            let streams = self.streams.clone();
            let logger = self.logger.clone();
            tokio::spawn(async move {
                info!(logger, "Execution request forwarder started");
                while let Some(request) = exec_req_rx.recv().await {
                    Self::forward_execution_request(request, &streams, &logger).await;
                }
                info!(logger, "Execution request forwarder stopped");
            });
        }

        Server::builder()
            .add_service(RaftoralSidecarServer::new(service))
            .serve(addr)
            .await?;

        Ok(())
    }

    /// Forward an execution request to a connected app
    async fn forward_execution_request(
        request: ExecutionRequest,
        streams: &Arc<Mutex<std::collections::HashMap<String, mpsc::Sender<SidecarMessage>>>>,
        logger: &Logger,
    ) {
        // For now, broadcast to all connected apps
        // TODO: Implement smarter routing based on workflow name or app capabilities
        let streams_guard = streams.lock().await;

        if streams_guard.is_empty() {
            error!(logger, "No connected apps to execute workflow";
                "workflow_id" => &request.workflow_id,
                "workflow_name" => &request.workflow_name
            );
            return;
        }

        let message = SidecarMessage {
            message: Some(super::proto::sidecar_message::Message::ExecuteWorkflow(
                ProtoExecuteWorkflowRequest {
                    workflow_id: request.workflow_id.clone(),
                    workflow_name: request.workflow_name,
                    version: request.version,
                    input: request.input,
                    is_owner: request.is_owner,
                },
            )),
        };

        // Send to all connected apps (for now - TODO: smarter routing)
        for (stream_id, tx) in streams_guard.iter() {
            if let Err(e) = tx.send(message.clone()).await {
                error!(logger, "Failed to send execution request to app";
                    "stream_id" => stream_id,
                    "error" => %e
                );
            } else {
                debug!(logger, "Execution request sent to app";
                    "stream_id" => stream_id,
                    "workflow_id" => &request.workflow_id
                );
            }
        }
    }
}

/// Internal service implementation
struct SidecarServiceImpl {
    logger: Logger,
    streams: Arc<Mutex<std::collections::HashMap<String, mpsc::Sender<SidecarMessage>>>>,
    execution_result_tx: Option<mpsc::Sender<ExecutionResult>>,
}

#[tonic::async_trait]
impl RaftoralSidecar for SidecarServiceImpl {
    type WorkflowStreamStream = ResponseStream;

    async fn workflow_stream(
        &self,
        request: Request<tonic::Streaming<AppMessage>>,
    ) -> Result<Response<Self::WorkflowStreamStream>, Status> {
        let remote_addr = request
            .remote_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        info!(self.logger, "New workflow stream connection";
            "remote_addr" => &remote_addr
        );

        let mut in_stream = request.into_inner();

        // Create channel for sending messages to app
        let (tx, rx) = mpsc::channel::<SidecarMessage>(100);

        // Generate unique stream ID (use remote_addr for now, will improve later)
        let stream_id = format!("stream_{}", uuid::Uuid::new_v4());

        // Store the sender for this stream
        {
            let mut streams = self.streams.lock().await;
            streams.insert(stream_id.clone(), tx.clone());
        }

        let logger = self.logger.clone();
        let streams = self.streams.clone();
        let stream_id_clone = stream_id.clone();
        let execution_result_tx = self.execution_result_tx.clone();

        // Spawn task to handle incoming messages from app
        tokio::spawn(async move {
            while let Some(result) = in_stream.next().await {
                match result {
                    Ok(app_msg) => {
                        debug!(logger, "Received message from app";
                            "stream_id" => &stream_id_clone
                        );

                        // Handle different message types
                        if let Some(msg) = app_msg.message {
                            match msg {
                                super::proto::app_message::Message::CheckpointProposal(proposal) => {
                                    debug!(logger, "Received checkpoint proposal";
                                        "workflow_id" => &proposal.workflow_id,
                                        "checkpoint_name" => &proposal.checkpoint_name
                                    );
                                    // TODO: Propose checkpoint to Raft
                                }
                                super::proto::app_message::Message::WorkflowResult(result) => {
                                    info!(logger, "Received workflow result";
                                        "workflow_id" => &result.workflow_id,
                                        "success" => result.success
                                    );

                                    // Forward result to WorkflowProxyRuntime
                                    if let Some(ref tx) = execution_result_tx {
                                        let execution_result = ExecutionResult {
                                            workflow_id: result.workflow_id.clone(),
                                            success: result.success,
                                            result: result.result,
                                            error: result.error,
                                        };

                                        if let Err(e) = tx.send(execution_result).await {
                                            error!(logger, "Failed to forward workflow result to proxy runtime";
                                                "workflow_id" => &result.workflow_id,
                                                "error" => %e
                                            );
                                        } else {
                                            debug!(logger, "Workflow result forwarded to proxy runtime";
                                                "workflow_id" => &result.workflow_id
                                            );
                                        }
                                    } else {
                                        error!(logger, "No execution_result_tx available to forward workflow result";
                                            "workflow_id" => &result.workflow_id
                                        );
                                    }
                                }
                                super::proto::app_message::Message::Heartbeat(hb) => {
                                    debug!(logger, "Received heartbeat";
                                        "timestamp" => hb.timestamp
                                    );

                                    // Send heartbeat response
                                    let response = SidecarMessage {
                                        message: Some(super::proto::sidecar_message::Message::HeartbeatResponse(
                                            super::proto::HeartbeatResponse {
                                                timestamp: hb.timestamp,
                                            }
                                        )),
                                    };

                                    if let Err(e) = tx.send(response).await {
                                        error!(logger, "Failed to send heartbeat response"; "error" => %e);
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!(logger, "Error receiving message from app";
                            "error" => %e
                        );
                        break;
                    }
                }
            }

            // Clean up when stream ends
            info!(logger, "App stream disconnected"; "stream_id" => &stream_id_clone);
            let mut streams = streams.lock().await;
            streams.remove(&stream_id_clone);
        });

        // Return the outgoing stream
        let out_stream = ReceiverStream::new(rx);
        Ok(Response::new(
            Box::pin(out_stream.map(Ok)) as Self::WorkflowStreamStream
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slog::{Drain, o};

    fn create_test_logger() -> Logger {
        let decorator = slog_term::PlainDecorator::new(std::io::sink());
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        Logger::root(drain, o!())
    }

    #[test]
    fn test_sidecar_server_creation() {
        let logger = create_test_logger();
        let server = SidecarServer::new("127.0.0.1:9001".to_string(), logger);
        assert_eq!(server.address, "127.0.0.1:9001");
    }
}
