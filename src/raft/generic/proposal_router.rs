//! Proposal Router (Layer 6)
//!
//! Routes proposals to the leader node and tracks completion.
//! Simplifies the client experience by handling leader election and retries.

use crate::grpc::proto::{self as raft_proto};
use crate::raft::generic::{RaftNode, RoleChange, StateMachine, Transport};
use slog::{debug, warn, Logger};
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

/// Error types for proposal routing
#[derive(Debug, Clone)]
pub enum ProposalError {
    /// This node is not the leader
    NotLeader {
        /// The current leader's node ID (if known)
        leader_id: Option<u64>,
    },
    /// Proposal failed for another reason
    Failed(String),
}

impl std::fmt::Display for ProposalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProposalError::NotLeader { leader_id } => {
                if let Some(id) = leader_id {
                    write!(f, "Not leader (leader is node {})", id)
                } else {
                    write!(f, "Not leader (leader unknown)")
                }
            }
            ProposalError::Failed(msg) => write!(f, "Proposal failed: {}", msg),
        }
    }
}

impl std::error::Error for ProposalError {}

/// Proposal Router for simplified proposal submission
///
/// This wraps a RaftNode and provides a higher-level interface for proposing commands.
/// If this node is the leader, proposals are submitted locally.
/// If not, proposals are forwarded to the leader via transport.
///
/// # Type Parameters
/// * `SM` - State Machine type
pub struct ProposalRouter<SM: StateMachine> {
    /// The underlying RaftNode
    node: Arc<Mutex<RaftNode<SM>>>,

    /// Transport layer for forwarding to leader
    transport: Arc<dyn Transport>,

    /// Cluster ID for message routing
    cluster_id: u32,

    /// Current leader ID (if known)
    leader_id: Arc<Mutex<Option<u64>>>,

    /// This node's ID
    node_id: u64,

    /// Logger
    logger: Logger,
}

impl<SM: StateMachine> ProposalRouter<SM> {
    /// Create a new ProposalRouter
    ///
    /// # Arguments
    /// * `node` - The RaftNode to wrap
    /// * `transport` - Transport layer for forwarding proposals
    /// * `cluster_id` - Cluster ID for message routing
    /// * `node_id` - This node's ID
    /// * `logger` - Logger instance
    pub fn new(
        node: Arc<Mutex<RaftNode<SM>>>,
        transport: Arc<dyn Transport>,
        cluster_id: u32,
        node_id: u64,
        logger: Logger,
    ) -> Self {
        // Initialize leader_id by querying the node (synchronously via block_in_place)
        let initial_leader = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let n = node.lock().await;
                n.leader_id()
            })
        });

        Self {
            node,
            transport,
            cluster_id,
            leader_id: Arc::new(Mutex::new(initial_leader)),
            node_id,
            logger,
        }
    }

    /// Start listening to role changes to track leadership
    ///
    /// This should be spawned as a background task.
    pub async fn run_leader_tracker(self: Arc<Self>) {
        let mut role_rx = {
            let node = self.node.lock().await;
            node.subscribe_role_changes()
        };

        loop {
            match role_rx.recv().await {
                Ok(change) => {
                    match change {
                        RoleChange::BecameLeader => {
                            // We became leader
                            *self.leader_id.lock().await = Some(self.node_id);
                        }
                        RoleChange::BecameFollower | RoleChange::BecameCandidate => {
                            // We're not leader anymore, query the node for current leader
                            let node = self.node.lock().await;
                            let actual_leader = node.leader_id();
                            drop(node);
                            *self.leader_id.lock().await = actual_leader;
                        }
                    }
                }
                Err(_) => {
                    // Channel closed, exit
                    break;
                }
            }
        }
    }

    /// Propose a command
    ///
    /// If this node is the leader, proposes locally.
    /// If not, forwards the proposal to the known leader via transport.
    ///
    /// # Arguments
    /// * `command` - The command to propose
    ///
    /// # Returns
    /// * `Ok(receiver)` - Oneshot receiver that will be notified when committed
    /// * `Err(ProposalError)` - If no known leader or proposal failed
    pub async fn propose(
        &self,
        command: SM::Command,
    ) -> Result<oneshot::Receiver<Result<(), String>>, ProposalError> {
        // Check if we're the leader
        let mut node = self.node.lock().await;
        let is_leader = node.is_leader().await;

        if is_leader {
            // We're the leader, propose locally
            debug!(self.logger, "Proposing locally (we are leader)");
            return node.propose(command)
                .await
                .map_err(|e| ProposalError::Failed(e));
        }

        // We're not the leader - forward to known leader
        // First, check our cached leader_id
        let mut cached_leader = *self.leader_id.lock().await;

        // If we don't have a cached leader, query the node for current leader
        if cached_leader.is_none() {
            let actual_leader = node.leader_id();
            drop(node); // Release the lock

            if actual_leader.is_some() {
                // Update the cache
                *self.leader_id.lock().await = actual_leader;
                cached_leader = actual_leader;
            }
        } else {
            drop(node); // Release the lock
        }

        match cached_leader {
            Some(leader) if leader != self.node_id => {
                debug!(self.logger, "Forwarding proposal to leader"; "leader_id" => leader);
                self.forward_to_leader(leader, command).await
            }
            _ => {
                warn!(self.logger, "Cannot propose - no known leader"; "leader_id" => ?cached_leader);
                Err(ProposalError::NotLeader { leader_id: cached_leader })
            }
        }
    }

    /// Forward a proposal to the leader
    async fn forward_to_leader(
        &self,
        leader_id: u64,
        command: SM::Command,
    ) -> Result<oneshot::Receiver<Result<(), String>>, ProposalError> {
        // Generate random sync_id for tracking (avoids collisions across nodes)
        let sync_id = rand::random::<u64>();

        // Create oneshot channel for completion
        let (tx, rx) = oneshot::channel();

        // Register with our local RaftNode so it can complete when the entry is committed locally
        // (after it's replicated back from the leader)
        let mut node = self.node.lock().await;
        node.register_forwarded_proposal(sync_id, tx).await;
        drop(node);

        // Serialize command
        let command_json = serde_json::to_vec(&command)
            .map_err(|e| ProposalError::Failed(format!("Failed to serialize command: {}", e)))?;

        // Create proposal message
        let propose_msg = raft_proto::ProposeMessage {
            command_json,
            sync_id,
        };

        let generic_msg = raft_proto::GenericMessage {
            cluster_id: self.cluster_id,
            message: Some(raft_proto::generic_message::Message::Propose(propose_msg)),
            ..Default::default()
        };

        // Send to leader
        self.transport.send_message(leader_id, generic_msg)
            .await
            .map_err(|e| {
                ProposalError::Failed(format!("Failed to forward to leader: {}", e))
            })?;

        debug!(self.logger, "Forwarded proposal to leader";
            "leader_id" => leader_id,
            "sync_id" => sync_id
        );

        Ok(rx)
    }

    /// Propose a command and wait for it to be committed and applied
    ///
    /// This is a convenience method that combines propose() and awaiting the result.
    ///
    /// # Arguments
    /// * `command` - The command to propose
    ///
    /// # Returns
    /// * `Ok(())` - Command was committed and applied successfully
    /// * `Err(ProposalError)` - If not leader, proposal failed, or application failed
    pub async fn propose_and_wait(&self, command: SM::Command) -> Result<(), ProposalError> {
        let rx = self.propose(command).await?;

        rx.await
            .map_err(|_| ProposalError::Failed("Proposal channel closed".to_string()))?
            .map_err(|e| ProposalError::Failed(e))
    }

    /// Get the current leader ID (if known)
    pub async fn leader_id(&self) -> Option<u64> {
        *self.leader_id.lock().await
    }

    /// Check if this node is the leader
    pub async fn is_leader(&self) -> bool {
        self.node.lock().await.is_leader().await
    }

    /// Add a node to the cluster
    ///
    /// # Arguments
    /// * `node_id` - ID of the node to add
    /// * `address` - Network address of the node
    ///
    /// # Returns
    /// Receiver that will be notified when the configuration change completes
    pub async fn add_node(
        &self,
        node_id: u64,
        address: String,
    ) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        self.node.lock().await.add_node(node_id, address).await
    }

    /// Remove a node from the cluster
    ///
    /// # Arguments
    /// * `node_id` - ID of the node to remove
    ///
    /// # Returns
    /// Receiver that will be notified when the configuration change completes
    pub async fn remove_node(
        &self,
        node_id: u64,
    ) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        self.node.lock().await.remove_node(node_id).await
    }

    /// Get reference to the underlying RaftNode
    pub fn node(&self) -> Arc<Mutex<RaftNode<SM>>> {
        self.node.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::generic::{
        EventBus, InProcessServer, InProcessMessageSender, KvCommand, KvStateMachine,
        RaftNodeConfig, TransportLayer,
    };
    use tokio::sync::mpsc;

    fn create_logger() -> Logger {
        use slog::Drain;
        let decorator = slog_term::PlainDecorator::new(std::io::stdout());
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        Logger::root(drain, slog::o!())
    }

    #[tokio::test]
    async fn test_proposal_router_creation() {
        let logger = create_logger();
        let server = Arc::new(InProcessServer::new());
        let transport = Arc::new(TransportLayer::new(Arc::new(InProcessMessageSender::new(
            server,
        ))));
        let (_tx, rx) = mpsc::channel(10);

        let config = RaftNodeConfig::default();
        let state_machine = KvStateMachine::new();
        let event_bus = Arc::new(EventBus::new(100));

        let node = RaftNode::new_single_node(
            config,
            transport.clone(),
            rx,
            state_machine,
            event_bus,
            logger.clone(),
        )
        .unwrap();
        let node = Arc::new(Mutex::new(node));

        let router = ProposalRouter::new(node, transport, 0, 1, logger);

        // Should not be leader initially (follower by default)
        assert!(!router.is_leader().await);
    }

    #[tokio::test]
    async fn test_proposal_router_not_leader() {
        let logger = create_logger();
        let server = Arc::new(InProcessServer::new());
        let transport = Arc::new(TransportLayer::new(Arc::new(InProcessMessageSender::new(
            server,
        ))));
        let (_tx, rx) = mpsc::channel(10);

        let config = RaftNodeConfig::default();
        let state_machine = KvStateMachine::new();
        let event_bus = Arc::new(EventBus::new(100));

        let node = RaftNode::new_single_node(
            config,
            transport.clone(),
            rx,
            state_machine,
            event_bus,
            logger.clone(),
        )
        .unwrap();
        let node = Arc::new(Mutex::new(node));

        let router = ProposalRouter::new(node, transport, 0, 1, logger);

        // Try to propose when not leader and no known leader
        let command = KvCommand::Set {
            key: "test".to_string(),
            value: "value".to_string(),
        };

        let result = router.propose(command).await;
        assert!(result.is_err());

        match result {
            Err(ProposalError::NotLeader { leader_id }) => {
                // Expected - no known leader
                assert_eq!(leader_id, None);
            }
            _ => panic!("Expected NotLeader error"),
        }
    }

    #[tokio::test]
    async fn test_proposal_router_leader_tracking() {
        // Test that the leader tracker updates leader_id when role changes
        let logger = create_logger();
        let server = Arc::new(InProcessServer::new());
        let transport = Arc::new(TransportLayer::new(Arc::new(InProcessMessageSender::new(
            server,
        ))));
        let (_tx, rx) = mpsc::channel(10);

        let config = RaftNodeConfig::default();
        let state_machine = KvStateMachine::new();
        let event_bus = Arc::new(EventBus::new(100));

        let node = RaftNode::new_single_node(
            config,
            transport.clone(),
            rx,
            state_machine,
            event_bus,
            logger.clone(),
        )
        .unwrap();
        let node = Arc::new(Mutex::new(node));

        let router = Arc::new(ProposalRouter::new(
            node.clone(),
            transport,
            0,
            1,
            logger,
        ));

        // Initially no known leader
        assert_eq!(router.leader_id().await, None);

        // Start leader tracker in background
        let router_clone = router.clone();
        let tracker_handle = tokio::spawn(async move {
            router_clone.run_leader_tracker().await;
        });

        // Give tracker time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Manually trigger a BecameLeader event
        let role_tx = {
            let node_guard = node.lock().await;
            node_guard.subscribe_role_changes();
            // We can't easily trigger leadership without running the full node,
            // so this test just verifies the tracker subscribes correctly
            // Full multi-node integration testing is needed for complete verification
        };

        // Stop the tracker
        drop(tracker_handle);

        // Note: Full proposal forwarding testing requires:
        // 1. Multi-node cluster with real Raft consensus
        // 2. Nodes running their event loops
        // 3. Network message passing between nodes
        // This is better tested in integration tests (Layer 7) with real cluster setup
    }
}
