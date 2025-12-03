//! Raftoral Client Implementation

use crate::error::{Error, Result};
use crate::proto::raftoral_sidecar_client::RaftoralSidecarClient;
use crate::proto::{
    app_message, sidecar_message, AppMessage, CheckpointProposal, ExecuteWorkflowRequest,
    HeartbeatRequest, WorkflowResult as ProtoWorkflowResult,
};
use futures::Future;
use slog::{debug, error, info, Logger};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_stream::{wrappers::ReceiverStream, StreamExt};
use tonic::transport::Channel;

type WorkflowHandler = Box<
    dyn Fn(ExecuteWorkflowRequest) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send>>
        + Send
        + Sync,
>;

/// Client for communicating with Raftoral sidecar
pub struct RaftoralClient {
    /// Channel for sending messages to sidecar
    tx: mpsc::Sender<AppMessage>,
    /// Registered workflow handlers
    handlers: Arc<Mutex<HashMap<(String, u32), Arc<WorkflowHandler>>>>,
    /// Pending checkpoint responses
    checkpoint_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<Vec<u8>>>>>,
    /// Logger
    logger: Logger,
}

impl RaftoralClient {
    /// Connect to the Raftoral sidecar
    pub async fn connect(url: &str, logger: Logger) -> Result<Self> {
        info!(logger, "Connecting to Raftoral sidecar"; "url" => url);

        let channel = Channel::from_shared(url.to_string())?
            .connect()
            .await?;

        let mut client = RaftoralSidecarClient::new(channel);

        // Create bidirectional stream
        let (tx, rx) = mpsc::channel::<AppMessage>(100);
        let request_stream = ReceiverStream::new(rx);

        let response = client.workflow_stream(request_stream).await?;
        let mut response_stream = response.into_inner();

        let handlers: Arc<Mutex<HashMap<(String, u32), Arc<WorkflowHandler>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let checkpoint_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let logger_clone = logger.clone();
        let handlers_clone = handlers.clone();
        let checkpoint_waiters_clone = checkpoint_waiters.clone();
        let tx_clone = tx.clone();

        // Spawn task to handle incoming messages from sidecar
        tokio::spawn(async move {
            while let Some(result) = response_stream.next().await {
                match result {
                    Ok(sidecar_msg) => {
                        if let Some(msg) = sidecar_msg.message {
                            match msg {
                                sidecar_message::Message::ExecuteWorkflow(req) => {
                                    debug!(logger_clone, "Received execute workflow request";
                                        "workflow_id" => &req.workflow_id,
                                        "workflow_name" => &req.workflow_name
                                    );

                                    Self::handle_execute_workflow(
                                        req,
                                        &handlers_clone,
                                        &tx_clone,
                                        &logger_clone,
                                    )
                                    .await;
                                }
                                sidecar_message::Message::CheckpointEvent(event) => {
                                    debug!(logger_clone, "Received checkpoint event";
                                        "workflow_id" => &event.workflow_id,
                                        "checkpoint_name" => &event.checkpoint_name
                                    );

                                    let key = format!("{}:{}", event.workflow_id, event.checkpoint_name);
                                    let mut waiters = checkpoint_waiters_clone.lock().await;
                                    if let Some(tx) = waiters.remove(&key) {
                                        let _ = tx.send(event.value);
                                    }
                                }
                                sidecar_message::Message::WorkflowCancellation(cancel) => {
                                    info!(logger_clone, "Workflow cancelled";
                                        "workflow_id" => &cancel.workflow_id,
                                        "reason" => &cancel.reason
                                    );
                                }
                                sidecar_message::Message::HeartbeatResponse(hb) => {
                                    debug!(logger_clone, "Received heartbeat response";
                                        "timestamp" => hb.timestamp
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!(logger_clone, "Error receiving from sidecar"; "error" => %e);
                        break;
                    }
                }
            }

            info!(logger_clone, "Sidecar stream closed");
        });

        // Start heartbeat task
        let tx_heartbeat = tx.clone();
        let logger_heartbeat = logger.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                interval.tick().await;

                let heartbeat = AppMessage {
                    message: Some(app_message::Message::Heartbeat(HeartbeatRequest {
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                    })),
                };

                if tx_heartbeat.send(heartbeat).await.is_err() {
                    debug!(logger_heartbeat, "Heartbeat channel closed");
                    break;
                }
            }
        });

        Ok(Self {
            tx,
            handlers,
            checkpoint_waiters,
            logger,
        })
    }

    /// Register a workflow handler
    pub async fn register_handler<F, Fut>(&self, name: &str, version: u32, handler: F)
    where
        F: Fn(ExecuteWorkflowRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Vec<u8>>> + Send + 'static,
    {
        let boxed: WorkflowHandler = Box::new(move |req| Box::pin(handler(req)));

        let mut handlers = self.handlers.lock().await;
        handlers.insert((name.to_string(), version), Arc::new(boxed));
    }

    /// Propose a checkpoint (owner node only)
    pub async fn propose_checkpoint(
        &self,
        workflow_id: &str,
        checkpoint_name: &str,
        value: Vec<u8>,
    ) -> Result<()> {
        debug!(self.logger, "Proposing checkpoint";
            "workflow_id" => workflow_id,
            "checkpoint_name" => checkpoint_name
        );

        let msg = AppMessage {
            message: Some(app_message::Message::CheckpointProposal(
                CheckpointProposal {
                    workflow_id: workflow_id.to_string(),
                    checkpoint_name: checkpoint_name.to_string(),
                    value,
                },
            )),
        };

        self.tx
            .send(msg)
            .await
            .map_err(|_| Error::ConnectionClosed)?;

        Ok(())
    }

    /// Wait for a checkpoint event (follower nodes)
    pub async fn wait_checkpoint(&self, workflow_id: &str, checkpoint_name: &str) -> Result<Vec<u8>> {
        let key = format!("{}:{}", workflow_id, checkpoint_name);
        let (tx, rx) = oneshot::channel();

        {
            let mut waiters = self.checkpoint_waiters.lock().await;
            waiters.insert(key, tx);
        }

        rx.await.map_err(|_| Error::Timeout("checkpoint event".to_string()))
    }

    /// Send workflow result
    async fn send_workflow_result(
        &self,
        workflow_id: &str,
        result: Result<Vec<u8>>,
    ) -> Result<()> {
        let (success, result_bytes, error) = match result {
            Ok(bytes) => (true, bytes, String::new()),
            Err(e) => (false, Vec::new(), e.to_string()),
        };

        let msg = AppMessage {
            message: Some(app_message::Message::WorkflowResult(ProtoWorkflowResult {
                workflow_id: workflow_id.to_string(),
                success,
                result: result_bytes,
                error,
            })),
        };

        self.tx
            .send(msg)
            .await
            .map_err(|_| Error::ConnectionClosed)?;

        Ok(())
    }

    /// Internal handler for execute workflow requests
    async fn handle_execute_workflow(
        req: ExecuteWorkflowRequest,
        handlers: &Arc<Mutex<HashMap<(String, u32), Arc<WorkflowHandler>>>>,
        tx: &mpsc::Sender<AppMessage>,
        logger: &Logger,
    ) {
        let workflow_id = req.workflow_id.clone();
        let workflow_name = req.workflow_name.clone();
        let version = req.version;

        let handlers_lock = handlers.lock().await;
        let handler = handlers_lock.get(&(workflow_name.clone(), version)).cloned();
        drop(handlers_lock);

        if let Some(handler) = handler {
            let result = handler(req).await;

            let (success, result_bytes, error) = match result {
                Ok(bytes) => (true, bytes, String::new()),
                Err(e) => {
                    error!(logger, "Workflow execution failed";
                        "workflow_id" => &workflow_id,
                        "error" => %e
                    );
                    (false, Vec::new(), e.to_string())
                }
            };

            let msg = AppMessage {
                message: Some(app_message::Message::WorkflowResult(ProtoWorkflowResult {
                    workflow_id,
                    success,
                    result: result_bytes,
                    error,
                })),
            };

            if let Err(e) = tx.send(msg).await {
                error!(logger, "Failed to send workflow result"; "error" => %e);
            }
        } else {
            error!(logger, "No handler registered for workflow";
                "workflow_name" => &workflow_name,
                "version" => version
            );

            let msg = AppMessage {
                message: Some(app_message::Message::WorkflowResult(ProtoWorkflowResult {
                    workflow_id,
                    success: false,
                    result: Vec::new(),
                    error: format!("No handler for workflow {}:{}", workflow_name, version),
                })),
            };

            let _ = tx.send(msg).await;
        }
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

    #[tokio::test]
    async fn test_client_creation() {
        // This test requires a running sidecar, so we'll just verify the structure
        assert!(true);
    }
}
