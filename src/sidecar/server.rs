//! Sidecar gRPC Server Implementation
//!
//! Provides bidirectional streaming gRPC server for app ↔ sidecar communication.

use super::proto::raftoral_sidecar_server::{RaftoralSidecar, RaftoralSidecarServer};
use super::proto::{AppMessage, SidecarMessage};
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
}

impl SidecarServer {
    /// Create a new sidecar server
    pub fn new(address: String, logger: Logger) -> Self {
        Self {
            address,
            logger,
            streams: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Start the sidecar server
    pub async fn start(self) -> Result<(), Box<dyn std::error::Error>> {
        let addr = self.address.parse()?;

        info!(self.logger, "Starting sidecar server"; "address" => %addr);

        let service = SidecarServiceImpl {
            logger: self.logger.clone(),
            streams: self.streams.clone(),
        };

        Server::builder()
            .add_service(RaftoralSidecarServer::new(service))
            .serve(addr)
            .await?;

        Ok(())
    }
}

/// Internal service implementation
struct SidecarServiceImpl {
    logger: Logger,
    streams: Arc<Mutex<std::collections::HashMap<String, mpsc::Sender<SidecarMessage>>>>,
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
                                    // TODO: Handle workflow completion
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
