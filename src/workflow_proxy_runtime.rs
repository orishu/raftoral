//! Workflow Proxy Runtime - Sidecar Mode
//!
//! This runtime acts as a proxy between the sidecar server and workflow execution clusters.
//! Instead of executing workflows directly (like WorkflowRuntime), it forwards execution
//! requests to application processes via the sidecar protocol.
//!
//! Key architectural points:
//! - Uses the SAME WorkflowStateMachine as WorkflowRuntime (tracks workflow state in Raft)
//! - Different execution model: delegates to external apps via sidecar instead of in-process
//! - Shared config contains sidecar communication settings instead of workflow registry

use crate::management::SubClusterRuntime;
use crate::raft::generic::{EventBus, ProposalRouter, RaftNode, RaftNodeConfig, Transport};
use crate::workflow::{WorkflowCommand, WorkflowEvent, WorkflowStateMachine};
use slog::{debug, error, info, Logger};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};

/// Request to execute a workflow on an external app
#[derive(Debug, Clone)]
pub struct ExecutionRequest {
    pub workflow_id: String,
    pub workflow_name: String,
    pub version: u32,
    pub input: Vec<u8>,
    pub is_owner: bool,
}

/// Result from workflow execution by external app
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub workflow_id: String,
    pub success: bool,
    pub result: Vec<u8>,
    pub error: String,
}

/// Configuration for proxy runtime
///
/// Contains communication channels to/from SidecarServer
pub struct ProxyConfig {
    /// Sender for execution requests (ProxyRuntime → SidecarServer)
    pub execution_request_tx: mpsc::Sender<ExecutionRequest>,
    /// Sender for execution results (SidecarServer → ProxyRuntime)
    pub execution_result_tx: mpsc::Sender<ExecutionResult>,
}

impl ProxyConfig {
    /// Create a new proxy configuration with communication channels
    pub fn new() -> (
        Self,
        mpsc::Receiver<ExecutionRequest>,
        mpsc::Receiver<ExecutionResult>,
    ) {
        let (exec_req_tx, exec_req_rx) = mpsc::channel::<ExecutionRequest>(100);
        let (exec_res_tx, exec_res_rx) = mpsc::channel::<ExecutionResult>(100);

        (
            Self {
                execution_request_tx: exec_req_tx,
                execution_result_tx: exec_res_tx,
            },
            exec_req_rx,
            exec_res_rx,
        )
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        let (config, _, _) = Self::new();
        config
    }
}

/// Workflow Proxy Runtime for sidecar mode
///
/// This runtime coordinates workflow execution across application processes
/// via the sidecar protocol, instead of executing workflows in-process.
///
/// Architecture:
/// - Same RaftNode<WorkflowStateMachine> as WorkflowRuntime
/// - Different execution layer: communicates with apps via sidecar gRPC
/// - When a workflow needs to execute, it sends ExecuteWorkflowRequest to app via sidecar
/// - When a checkpoint is proposed, it broadcasts CheckpointEvent to all apps via sidecar
#[derive(Clone)]
pub struct WorkflowProxyRuntime {
    /// Proposal router for submitting operations
    proposal_router: Arc<ProposalRouter<WorkflowStateMachine>>,

    /// Event bus for workflow event notifications
    event_bus: Arc<EventBus<WorkflowEvent>>,

    /// Channel to send execution requests to SidecarServer
    execution_request_tx: mpsc::Sender<ExecutionRequest>,

    /// Channel to send execution results to observer loop
    execution_result_tx: mpsc::Sender<ExecutionResult>,

    /// Node ID
    node_id: u64,

    /// Cluster ID
    cluster_id: u32,

    /// Logger
    logger: Logger,
}

impl WorkflowProxyRuntime {
    /// Create a new Workflow proxy runtime (single-node cluster)
    pub fn new(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc::proto::GenericMessage>,
        shared_config: Arc<Mutex<ProxyConfig>>,
        logger: Logger,
    ) -> Result<
        (Self, Arc<Mutex<RaftNode<WorkflowStateMachine>>>),
        Box<dyn std::error::Error>,
    > {
        let state_machine = WorkflowStateMachine::new();
        let event_bus = Arc::new(EventBus::new(100));

        info!(logger, "Creating Workflow proxy runtime (sidecar mode)";
            "node_id" => config.node_id,
            "cluster_id" => config.cluster_id
        );

        // Create RaftNode (same as WorkflowRuntime)
        let node = RaftNode::new_single_node(
            config.clone(),
            transport.clone(),
            mailbox_rx,
            state_machine,
            event_bus.clone(),
            logger.clone(),
        )?;

        let node_arc = Arc::new(Mutex::new(node));

        // Create ProposalRouter
        let proposal_router = Arc::new(ProposalRouter::new(
            node_arc.clone(),
            transport,
            config.cluster_id,
            config.node_id,
            logger.clone(),
        ));

        // Start leader tracker in background
        let router_clone = proposal_router.clone();
        tokio::spawn(async move {
            router_clone.run_leader_tracker().await;
        });

        // Get channels from shared config
        // We need to get result_rx before creating runtime, since we'll pass it to observer
        let (execution_request_tx, execution_result_tx, result_rx) = {
            // Create a temporary receiver channel for results
            let (result_tx, result_rx) = mpsc::channel::<ExecutionResult>(100);

            // Store sender in shared config so SidecarServer can send results
            let mut config_guard = shared_config.blocking_lock();
            let exec_req_tx = config_guard.execution_request_tx.clone();
            config_guard.execution_result_tx = result_tx.clone();
            drop(config_guard);

            (exec_req_tx, result_tx, result_rx)
        };

        let runtime = Arc::new(Self {
            proposal_router,
            event_bus: event_bus.clone(),
            execution_request_tx,
            execution_result_tx,
            node_id: config.node_id,
            cluster_id: config.cluster_id,
            logger: logger.clone(),
        });

        // Start workflow execution observer (listens for WorkflowStarted events)
        let runtime_clone = runtime.clone();
        tokio::spawn(async move {
            runtime_clone.run_workflow_observer(result_rx).await;
        });

        Ok(((*runtime).clone(), node_arc))
    }

    /// Create a new Workflow proxy runtime for a node joining an existing cluster
    pub fn new_joining_node(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc::proto::GenericMessage>,
        initial_voters: Vec<u64>,
        shared_config: Arc<Mutex<ProxyConfig>>,
        logger: Logger,
    ) -> Result<
        (Self, Arc<Mutex<RaftNode<WorkflowStateMachine>>>),
        Box<dyn std::error::Error>,
    > {
        let state_machine = WorkflowStateMachine::new();
        let event_bus = Arc::new(EventBus::new(100));

        info!(logger, "Creating Workflow proxy runtime (joining existing cluster)";
            "node_id" => config.node_id,
            "cluster_id" => config.cluster_id,
            "initial_voters" => ?initial_voters
        );

        // Create ConfState with all nodes as voters
        use raft::prelude::ConfState;
        let conf_state = ConfState::from((initial_voters, vec![]));

        // Create RaftNode for joining a multi-node cluster
        let node = RaftNode::new_with_conf_state(
            config.clone(),
            transport.clone(),
            mailbox_rx,
            state_machine,
            event_bus.clone(),
            conf_state,
            logger.clone(),
        )?;

        let node_arc = Arc::new(Mutex::new(node));

        // Create ProposalRouter
        let proposal_router = Arc::new(ProposalRouter::new(
            node_arc.clone(),
            transport,
            config.cluster_id,
            config.node_id,
            logger.clone(),
        ));

        // Start leader tracker in background
        let router_clone = proposal_router.clone();
        tokio::spawn(async move {
            router_clone.run_leader_tracker().await;
        });

        // Get channels from shared config
        let (execution_request_tx, execution_result_tx, result_rx) = {
            let (result_tx, result_rx) = mpsc::channel::<ExecutionResult>(100);

            let mut config_guard = shared_config.blocking_lock();
            let exec_req_tx = config_guard.execution_request_tx.clone();
            config_guard.execution_result_tx = result_tx.clone();
            drop(config_guard);

            (exec_req_tx, result_tx, result_rx)
        };

        let runtime = Arc::new(Self {
            proposal_router,
            event_bus: event_bus.clone(),
            execution_request_tx,
            execution_result_tx,
            node_id: config.node_id,
            cluster_id: config.cluster_id,
            logger: logger.clone(),
        });

        // Start workflow execution observer
        let runtime_clone = runtime.clone();
        tokio::spawn(async move {
            runtime_clone.run_workflow_observer(result_rx).await;
        });

        Ok(((*runtime).clone(), node_arc))
    }

    /// Observer that listens for WorkflowStarted events and forwards them to apps
    async fn run_workflow_observer(
        self: Arc<Self>,
        mut result_rx: mpsc::Receiver<ExecutionResult>,
    ) {
        let mut event_rx = self.event_bus.subscribe();

        info!(self.logger, "Workflow proxy observer started");

        loop {
            tokio::select! {
                // Handle workflow events from state machine
                event_result = event_rx.recv() => {
                    match event_result {
                        Ok(event) => {
                            match event {
                                WorkflowEvent::WorkflowStarted {
                                    workflow_id,
                                    workflow_type,
                                    version,
                                    owner_node_id,
                                } => {
                                    let is_owner = owner_node_id == self.node_id;
                                    info!(self.logger, "Forwarding workflow to app via sidecar";
                                        "workflow_id" => &workflow_id,
                                        "workflow_type" => &workflow_type,
                                        "version" => version,
                                        "is_owner" => is_owner
                                    );

                                    let runtime_clone = self.clone();
                                    tokio::spawn(async move {
                                        runtime_clone
                                            .forward_workflow_to_app(workflow_id, workflow_type, version, is_owner)
                                            .await;
                                    });
                                }
                                _ => {
                                    // Ignore other events
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            info!(self.logger, "Workflow proxy observer stopped (event channel closed)");
                            break;
                        }
                    }
                }

                // Handle execution results from apps (via SidecarServer)
                result = result_rx.recv() => {
                    match result {
                        Some(execution_result) => {
                            info!(self.logger, "Received workflow result from app";
                                "workflow_id" => &execution_result.workflow_id,
                                "success" => execution_result.success
                            );

                            let runtime_clone = self.clone();
                            tokio::spawn(async move {
                                runtime_clone.handle_workflow_result(execution_result).await;
                            });
                        }
                        None => {
                            error!(self.logger, "Execution result channel closed");
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Forward workflow execution to app via SidecarServer
    async fn forward_workflow_to_app(
        self: Arc<Self>,
        workflow_id: String,
        workflow_name: String,
        version: u32,
        is_owner: bool,
    ) {
        // Get workflow input from state machine
        let input_bytes = match self.get_input(&workflow_id).await {
            Some(bytes) => bytes,
            None => {
                error!(self.logger, "Workflow input not found in state machine";
                    "workflow_id" => &workflow_id
                );
                return;
            }
        };

        // Create execution request
        let request = ExecutionRequest {
            workflow_id: workflow_id.clone(),
            workflow_name,
            version,
            input: input_bytes,
            is_owner,
        };

        // Send to SidecarServer (which will forward to connected app)
        if let Err(e) = self.execution_request_tx.send(request).await {
            error!(self.logger, "Failed to send execution request to sidecar";
                "workflow_id" => &workflow_id,
                "error" => %e
            );
        } else {
            debug!(self.logger, "Execution request sent to sidecar";
                "workflow_id" => &workflow_id
            );
        }
    }

    /// Handle workflow result from app
    async fn handle_workflow_result(self: Arc<Self>, result: ExecutionResult) {
        // Propose WorkflowEnd command to Raft
        // Even for errors, we use WorkflowEnd with error encoded in result bytes
        let result_bytes = if result.success {
            result.result
        } else {
            // Serialize error into result bytes
            result.error.into_bytes()
        };

        let command = WorkflowCommand::WorkflowEnd {
            workflow_id: result.workflow_id.clone(),
            result: result_bytes,
        };

        match self.proposal_router.propose(command).await {
            Ok(_rx) => {
                debug!(self.logger, "WorkflowEnd proposed";
                    "workflow_id" => &result.workflow_id,
                    "success" => result.success
                );
            }
            Err(e) => {
                error!(self.logger, "Failed to propose WorkflowEnd";
                    "workflow_id" => &result.workflow_id,
                    "error" => %e
                );
            }
        }
    }

    /// Get workflow input from state machine (helper method)
    async fn get_input(&self, workflow_id: &str) -> Option<Vec<u8>> {
        // TODO: Implement state machine query for workflow input
        // For now, return empty input
        Some(Vec::new())
    }

    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    pub fn cluster_id(&self) -> u32 {
        self.cluster_id
    }

    pub async fn add_node(
        &self,
        node_id: u64,
        _address: String,
    ) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        // TODO: Implement add_node via configuration change
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(Err(format!(
            "add_node not yet implemented for node {}",
            node_id
        )));
        Ok(rx)
    }

    pub async fn remove_node(
        &self,
        node_id: u64,
    ) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        // TODO: Implement remove_node via configuration change
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(Err(format!(
            "remove_node not yet implemented for node {}",
            node_id
        )));
        Ok(rx)
    }
}

impl SubClusterRuntime for WorkflowProxyRuntime {
    type StateMachine = WorkflowStateMachine;
    type SharedConfig = ProxyConfig;

    fn new_single_node(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc::proto::GenericMessage>,
        shared_config: Arc<Mutex<Self::SharedConfig>>,
        logger: Logger,
    ) -> Result<
        (Self, Arc<Mutex<RaftNode<Self::StateMachine>>>),
        Box<dyn std::error::Error>,
    >
    where
        Self: Sized,
    {
        WorkflowProxyRuntime::new(config, transport, mailbox_rx, shared_config, logger)
    }

    fn new_joining_node(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc::proto::GenericMessage>,
        initial_voters: Vec<u64>,
        shared_config: Arc<Mutex<Self::SharedConfig>>,
        logger: Logger,
    ) -> Result<
        (Self, Arc<Mutex<RaftNode<Self::StateMachine>>>),
        Box<dyn std::error::Error>,
    >
    where
        Self: Sized,
    {
        WorkflowProxyRuntime::new_joining_node(
            config,
            transport,
            mailbox_rx,
            initial_voters,
            shared_config,
            logger,
        )
    }

    async fn add_node(
        &self,
        node_id: u64,
        address: String,
    ) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        self.add_node(node_id, address).await
    }

    async fn remove_node(
        &self,
        node_id: u64,
    ) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        self.remove_node(node_id).await
    }

    fn node_id(&self) -> u64 {
        self.node_id()
    }

    fn cluster_id(&self) -> u32 {
        self.cluster_id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_config_creation() {
        let (config, _req_rx, _res_tx) = ProxyConfig::new();
        // Verify channels are created
        assert!(config.execution_request_tx.capacity() > 0);
    }
}
