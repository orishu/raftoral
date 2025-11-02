//! Proposal Router (Layer 6)
//!
//! Routes proposals to the leader node and tracks completion.
//! Simplifies the client experience by handling leader election and retries.

use crate::raft::generic2::{RaftNode, RoleChange, StateMachine};
use slog::{warn, Logger};
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
/// It checks leadership before proposing and returns appropriate errors if not leader.
///
/// # Type Parameters
/// * `SM` - State Machine type
pub struct ProposalRouter<SM: StateMachine> {
    /// The underlying RaftNode
    node: Arc<Mutex<RaftNode<SM>>>,

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
    /// * `node_id` - This node's ID
    /// * `logger` - Logger instance
    pub fn new(node: Arc<Mutex<RaftNode<SM>>>, node_id: u64, logger: Logger) -> Self {
        Self {
            node,
            leader_id: Arc::new(Mutex::new(None)),
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
                            // We're not leader anymore
                            *self.leader_id.lock().await = None;
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
    /// Checks if this node is the leader before proposing.
    /// Returns an error if not leader.
    ///
    /// # Arguments
    /// * `command` - The command to propose
    ///
    /// # Returns
    /// * `Ok(receiver)` - Oneshot receiver that will be notified when committed
    /// * `Err(ProposalError)` - If not leader or proposal failed
    pub async fn propose(
        &self,
        command: SM::Command,
    ) -> Result<oneshot::Receiver<Result<(), String>>, ProposalError> {
        // Check if we're the leader
        let mut node = self.node.lock().await;
        let is_leader = node.is_leader().await;

        if !is_leader {
            let leader_id = *self.leader_id.lock().await;
            warn!(self.logger, "Proposal rejected - not leader"; "leader_id" => ?leader_id);
            return Err(ProposalError::NotLeader { leader_id });
        }

        // We're the leader, propose
        node.propose(command)
            .await
            .map_err(|e| ProposalError::Failed(e))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::generic2::{
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

        let node = RaftNode::new_single_node(config, transport, rx, state_machine, event_bus, logger.clone())
            .unwrap();
        let node = Arc::new(Mutex::new(node));

        let router = ProposalRouter::new(node, 1, logger);

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

        let node = RaftNode::new_single_node(config, transport, rx, state_machine, event_bus, logger.clone())
            .unwrap();
        let node = Arc::new(Mutex::new(node));

        let router = ProposalRouter::new(node, 1, logger);

        // Try to propose when not leader
        let command = KvCommand::Set {
            key: "test".to_string(),
            value: "value".to_string(),
        };

        let result = router.propose(command).await;
        assert!(result.is_err());

        match result {
            Err(ProposalError::NotLeader { .. }) => {
                // Expected
            }
            _ => panic!("Expected NotLeader error"),
        }
    }
}
