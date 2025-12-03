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
//!
//! This is a stub implementation for Phase 1. Full implementation will be in Phase 2.

use crate::management::SubClusterRuntime;
use crate::raft::generic::{RaftNode, RaftNodeConfig, Transport};
use crate::workflow::WorkflowStateMachine;  // Reuse same state machine!
use slog::Logger;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, oneshot};

/// Configuration for proxy runtime
///
/// In Phase 2, this will contain:
/// - Sidecar server connection settings
/// - App process discovery/routing configuration
/// - Timeout settings for app communication
pub struct ProxyConfig {
    // Phase 2 TODO: Add sidecar configuration fields
    // Example:
    // sidecar_address: String,
    // app_timeout: Duration,
    // etc.
}

impl ProxyConfig {
    /// Create a new proxy configuration
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self::new()
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
pub struct WorkflowProxyRuntime {
    node_id: u64,
    cluster_id: u32,
    // Phase 2 TODO: Add fields for sidecar communication
    // sidecar_client: Arc<SidecarClient>,
    // active_workflows: Arc<Mutex<HashMap<String, WorkflowHandle>>>,
}

impl SubClusterRuntime for WorkflowProxyRuntime {
    type StateMachine = WorkflowStateMachine;  // Same state machine as WorkflowRuntime!
    type SharedConfig = ProxyConfig;

    fn new_single_node(
        config: RaftNodeConfig,
        _transport: Arc<dyn Transport>,
        _mailbox_rx: mpsc::Receiver<crate::grpc::proto::GenericMessage>,
        _shared_config: Arc<Mutex<Self::SharedConfig>>,
        _logger: Logger,
    ) -> Result<
        (Self, Arc<Mutex<RaftNode<Self::StateMachine>>>),
        Box<dyn std::error::Error>,
    >
    where
        Self: Sized,
    {
        // Phase 2 TODO: Implement actual proxy runtime initialization
        // 1. Create RaftNode<WorkflowStateMachine> (same as WorkflowRuntime)
        //    - Uses WorkflowStateMachine to track workflow state in Raft
        // 2. Connect to sidecar server
        //    - Establish gRPC connection to sidecar
        // 3. Set up bidirectional communication
        //    - Subscribe to sidecar events (workflow results, checkpoint proposals)
        //    - Set up handlers to send execution requests to apps
        // 4. Hook up event handlers
        //    - On workflow execution event: send ExecuteWorkflowRequest to app via sidecar
        //    - On checkpoint event: broadcast CheckpointEvent to all apps via sidecar
        //    - On workflow result from app: apply to WorkflowStateMachine

        unimplemented!("WorkflowProxyRuntime::new_single_node will be implemented in Phase 2")
    }

    fn new_joining_node(
        config: RaftNodeConfig,
        _transport: Arc<dyn Transport>,
        _mailbox_rx: mpsc::Receiver<crate::grpc::proto::GenericMessage>,
        _initial_voters: Vec<u64>,
        _shared_config: Arc<Mutex<Self::SharedConfig>>,
        _logger: Logger,
    ) -> Result<
        (Self, Arc<Mutex<RaftNode<Self::StateMachine>>>),
        Box<dyn std::error::Error>,
    >
    where
        Self: Sized,
    {
        // Phase 2 TODO: Implement joining node initialization
        // 1. Create RaftNode<WorkflowStateMachine> joining existing cluster
        //    - Same state machine as single node, but joins existing Raft group
        // 2. Connect to sidecar server
        // 3. Set up bidirectional communication (same as new_single_node)
        // 4. Hook up event handlers (same as new_single_node)

        unimplemented!("WorkflowProxyRuntime::new_joining_node will be implemented in Phase 2")
    }

    async fn add_node(
        &self,
        _node_id: u64,
        _address: String,
    ) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        // Phase 2 TODO: Implement add_node
        // - Propose configuration change to Raft
        // - Wait for confirmation
        unimplemented!("WorkflowProxyRuntime::add_node will be implemented in Phase 2")
    }

    async fn remove_node(
        &self,
        _node_id: u64,
    ) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        // Phase 2 TODO: Implement remove_node
        // - Propose configuration change to Raft
        // - Wait for confirmation
        unimplemented!("WorkflowProxyRuntime::remove_node will be implemented in Phase 2")
    }

    fn node_id(&self) -> u64 {
        self.node_id
    }

    fn cluster_id(&self) -> u32 {
        self.cluster_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_runtime_creation() {
        // Basic smoke test for structure
        let runtime = WorkflowProxyRuntime {
            node_id: 1,
            cluster_id: 1,
        };
        assert_eq!(runtime.node_id(), 1);
        assert_eq!(runtime.cluster_id(), 1);
    }
}
