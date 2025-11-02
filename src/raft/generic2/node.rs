//! Raft Node (Layer 3)
//!
//! Provides a clean interface to raft-rs with separation of concerns:
//! - Mailbox: receives peer Raft messages from lower layers (via ClusterRouter → Transport)
//! - Methods: upper layer commands (propose, campaign, add_node, remove_node)

use crate::grpc::server::raft_proto::{self, GenericMessage};
use crate::raft::generic::storage::MemStorageWithSnapshot;
use crate::raft::generic2::errors::TransportError;
use crate::raft::generic2::Transport;
use bytes::Bytes;
use protobuf::Message as ProtobufMessage;
use raft::prelude::*;
use raft::StateRole;
use serde::{Deserialize, Serialize};
use slog::{info, warn, Logger};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, broadcast, Mutex, oneshot};
use tokio::time;

/// Configuration for RaftNode initialization
#[derive(Clone, Debug)]
pub struct RaftNodeConfig {
    /// This node's ID
    pub node_id: u64,

    /// Cluster ID for message routing
    pub cluster_id: u32,

    /// Election tick (ticks before starting election)
    pub election_tick: usize,

    /// Heartbeat tick (ticks between heartbeats)
    pub heartbeat_tick: usize,

    /// Enable check quorum
    pub check_quorum: bool,

    /// Enable pre-vote
    pub pre_vote: bool,

    /// Tick interval
    pub tick_interval: Duration,
}

impl Default for RaftNodeConfig {
    fn default() -> Self {
        Self {
            node_id: 1,
            cluster_id: 0,
            election_tick: 10,
            heartbeat_tick: 3,
            check_quorum: true,
            pre_vote: true,
            tick_interval: Duration::from_millis(100),
        }
    }
}

/// Node metadata for configuration changes
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeMetadata {
    pub address: String,
}

/// Role change notification
#[derive(Clone, Debug, PartialEq)]
pub enum RoleChange {
    BecameFollower,
    BecameCandidate,
    BecameLeader,
}

/// Raft Node with clean separation of mailbox and command methods
///
/// Architecture:
/// - Mailbox (message_rx): Receives peer Raft messages from ClusterRouter
/// - Methods (propose, campaign, etc.): Called by upper layers
/// - Transport: Sends outgoing messages to peers
pub struct RaftNode {
    /// Raft node ID
    node_id: u64,

    /// Cluster ID for message routing
    cluster_id: u32,

    /// raft-rs RawNode
    raw_node: RawNode<MemStorageWithSnapshot>,

    /// Transport for sending messages to peers
    transport: Arc<dyn Transport>,

    /// Mailbox for incoming peer Raft messages
    message_rx: mpsc::Receiver<GenericMessage>,

    /// Current role
    current_role: Arc<Mutex<StateRole>>,

    /// Role change broadcast channel
    role_change_tx: broadcast::Sender<RoleChange>,

    /// Last committed index
    committed_index: Arc<Mutex<u64>>,

    /// Cached configuration state
    cached_conf_state: Arc<Mutex<ConfState>>,

    /// Logger
    logger: Logger,
}

impl RaftNode {
    /// Create a new RaftNode with single-node bootstrap
    ///
    /// This initializes a single-node cluster that can accept proposals immediately.
    pub fn new_single_node(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        message_rx: mpsc::Receiver<GenericMessage>,
        logger: Logger,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let storage = MemStorageWithSnapshot::new_with_conf_state(ConfState::from((
            vec![config.node_id],
            vec![],
        )));

        let raft_config = Config {
            id: config.node_id,
            election_tick: config.election_tick,
            heartbeat_tick: config.heartbeat_tick,
            check_quorum: config.check_quorum,
            pre_vote: config.pre_vote,
            ..Default::default()
        };

        let raw_node = RawNode::new(&raft_config, storage, &logger)?;
        let (role_change_tx, _) = broadcast::channel(16);

        Ok(Self {
            node_id: config.node_id,
            cluster_id: config.cluster_id,
            raw_node,
            transport,
            message_rx,
            current_role: Arc::new(Mutex::new(StateRole::Follower)),
            role_change_tx,
            committed_index: Arc::new(Mutex::new(0)),
            cached_conf_state: Arc::new(Mutex::new(ConfState::from((vec![config.node_id], vec![])))),
            logger,
        })
    }

    /// Create a new RaftNode for multi-node cluster
    ///
    /// Node starts as follower and must receive configuration from leader.
    pub fn new_multi_node(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        message_rx: mpsc::Receiver<GenericMessage>,
        logger: Logger,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let storage = MemStorageWithSnapshot::default();

        let raft_config = Config {
            id: config.node_id,
            election_tick: config.election_tick,
            heartbeat_tick: config.heartbeat_tick,
            check_quorum: config.check_quorum,
            pre_vote: config.pre_vote,
            ..Default::default()
        };

        let raw_node = RawNode::new(&raft_config, storage, &logger)?;
        let (role_change_tx, _) = broadcast::channel(16);

        Ok(Self {
            node_id: config.node_id,
            cluster_id: config.cluster_id,
            raw_node,
            transport,
            message_rx,
            current_role: Arc::new(Mutex::new(StateRole::Follower)),
            role_change_tx,
            committed_index: Arc::new(Mutex::new(0)),
            cached_conf_state: Arc::new(Mutex::new(ConfState::default())),
            logger,
        })
    }

    /// Subscribe to role change notifications
    pub fn subscribe_role_changes(&self) -> broadcast::Receiver<RoleChange> {
        self.role_change_tx.subscribe()
    }

    /// Get current role
    pub async fn role(&self) -> StateRole {
        *self.current_role.lock().await
    }

    /// Get last committed index
    pub async fn committed_index(&self) -> u64 {
        *self.committed_index.lock().await
    }

    /// Get current configuration state
    pub async fn conf_state(&self) -> ConfState {
        self.cached_conf_state.lock().await.clone()
    }

    /// Check if this node is the leader
    pub async fn is_leader(&self) -> bool {
        *self.current_role.lock().await == StateRole::Leader
    }

    /// Propose a command (upper layer method, not mailbox)
    ///
    /// Returns oneshot receiver that will be notified when proposal is committed.
    pub async fn propose(&mut self, data: Vec<u8>) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        let (tx, rx) = oneshot::channel();

        self.raw_node
            .propose(vec![], data)
            .map_err(|e| format!("Failed to propose: {:?}", e))?;

        // TODO: Track proposal with sync_id and complete tx when committed
        // For now, immediately succeed (will be enhanced in future)
        let _ = tx.send(Ok(()));

        Ok(rx)
    }

    /// Campaign to become leader (upper layer method, not mailbox)
    pub async fn campaign(&mut self) -> Result<(), String> {
        self.raw_node
            .campaign()
            .map_err(|e| format!("Failed to campaign: {:?}", e))
    }

    /// Add a node to the cluster (upper layer method, not mailbox)
    pub async fn add_node(&mut self, node_id: u64, address: String) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        let (tx, rx) = oneshot::channel();

        // Create node metadata
        let metadata = NodeMetadata { address: address.clone() };
        let metadata_bytes = serde_json::to_vec(&metadata)
            .map_err(|e| format!("Failed to serialize metadata: {}", e))?;

        // Propose configuration change
        let mut cc = ConfChange::default();
        cc.set_change_type(ConfChangeType::AddNode);
        cc.node_id = node_id;
        cc.context = Bytes::from(metadata_bytes);

        self.raw_node
            .propose_conf_change(vec![], cc)
            .map_err(|e| format!("Failed to propose conf change: {:?}", e))?;

        // Add peer to transport
        self.transport.add_peer(node_id, address).await;

        // TODO: Track conf change and complete tx when applied
        let _ = tx.send(Ok(()));

        Ok(rx)
    }

    /// Remove a node from the cluster (upper layer method, not mailbox)
    pub async fn remove_node(&mut self, node_id: u64) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        let (tx, rx) = oneshot::channel();

        // Propose configuration change
        let mut cc = ConfChange::default();
        cc.set_change_type(ConfChangeType::RemoveNode);
        cc.node_id = node_id;

        self.raw_node
            .propose_conf_change(vec![], cc)
            .map_err(|e| format!("Failed to propose conf change: {:?}", e))?;

        // TODO: Track conf change and complete tx when applied
        let _ = tx.send(Ok(()));

        Ok(rx)
    }

    /// Run the Raft node event loop
    ///
    /// This processes:
    /// - Periodic ticks
    /// - Incoming peer Raft messages from mailbox
    /// - Ready notifications from raft-rs
    pub async fn run(mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut ticker = time::interval(Duration::from_millis(100));

        loop {
            tokio::select! {
                // Periodic tick
                _ = ticker.tick() => {
                    self.raw_node.tick();
                }

                // Incoming peer Raft message from mailbox (lower layer)
                msg = self.message_rx.recv() => {
                    match msg {
                        Some(generic_msg) => {
                            self.handle_raft_message(generic_msg).await?;
                        }
                        None => {
                            info!(self.logger, "Mailbox closed, shutting down");
                            break;
                        }
                    }
                }
            }

            // Process ready
            self.on_ready().await?;
        }

        Ok(())
    }

    /// Handle incoming peer Raft message
    async fn handle_raft_message(&mut self, generic_msg: raft_proto::GenericMessage) -> Result<(), Box<dyn std::error::Error>> {
        // Extract the raft_message from the oneof
        let message = generic_msg.message
            .ok_or("GenericMessage missing message field")?;

        match message {
            raft_proto::generic_message::Message::RaftMessage(bytes) => {
                // Deserialize raft::prelude::Message from protobuf bytes
                let raft_msg = raft::prelude::Message::parse_from_bytes(&bytes)?;

                // Step the message
                self.raw_node.step(raft_msg)?;
            }
            _ => {
                // In generic2, the mailbox should only receive Raft messages
                // Commands (propose, campaign, etc.) come via method calls
                warn!(self.logger, "Unexpected message type in mailbox, ignoring");
            }
        }

        Ok(())
    }

    /// Process Ready from raft-rs
    ///
    /// This handles:
    /// - Snapshot restoration (unimplemented for now)
    /// - Committed entries (TODO: apply to state machine)
    /// - Entry persistence
    /// - Hard state persistence
    /// - Role changes
    /// - Outgoing messages to peers
    async fn on_ready(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.raw_node.has_ready() {
            return Ok(());
        }

        let store = self.raw_node.raft.raft_log.store.clone();
        let mut ready = self.raw_node.ready();

        // 1. Handle snapshot (unimplemented)
        if !ready.snapshot().is_empty() {
            warn!(self.logger, "Snapshot support not yet implemented");
            // TODO: Restore state machine from snapshot
            // TODO: Update cached_conf_state
        }

        // 2. Handle committed entries BEFORE persisting new entries
        let committed_entries = ready.committed_entries();
        if !committed_entries.is_empty() {
            for entry in committed_entries {
                if entry.data.is_empty() {
                    // Empty entry from leader election
                    continue;
                }

                match entry.get_entry_type() {
                    EntryType::EntryNormal => {
                        // TODO: Apply to state machine
                        // For now, just update committed index
                        *self.committed_index.lock().await = entry.index;
                    }
                    EntryType::EntryConfChange | EntryType::EntryConfChangeV2 => {
                        // Apply configuration change
                        self.apply_conf_change(entry).await?;
                    }
                }
            }
        }

        // 3. Persist entries
        if !ready.entries().is_empty() {
            store.wl().append(ready.entries())?;
        }

        // 4. Persist hard state
        if let Some(hs) = ready.hs() {
            store.wl().set_hardstate(hs.clone());
        }

        // 5. Handle role changes
        if let Some(ss) = ready.ss() {
            let new_role = ss.raft_state;
            let mut current_role = self.current_role.lock().await;

            if new_role != *current_role {
                let change = match new_role {
                    StateRole::Follower => RoleChange::BecameFollower,
                    StateRole::Candidate => RoleChange::BecameCandidate,
                    StateRole::Leader => RoleChange::BecameLeader,
                    _ => return Ok(()), // PreCandidate doesn't need notification
                };

                info!(self.logger, "Role changed";
                    "old_role" => format!("{:?}", current_role),
                    "new_role" => format!("{:?}", new_role)
                );

                *current_role = new_role;
                let _ = self.role_change_tx.send(change);
            }
        }

        // 6. Send messages to peers
        if !ready.messages().is_empty() {
            for msg in ready.take_messages() {
                self.send_raft_message(msg).await?;
            }
        }

        // 7. Send persisted messages
        if !ready.persisted_messages().is_empty() {
            for msg in ready.take_persisted_messages() {
                self.send_raft_message(msg).await?;
            }
        }

        // 8. Advance the ready
        let mut light_rd = self.raw_node.advance(ready);

        // 9. Handle commit index from light ready
        if let Some(commit) = light_rd.commit_index() {
            store.wl().mut_hard_state().set_commit(commit);
        }

        // 10. Send messages from light ready
        if !light_rd.messages().is_empty() {
            for msg in light_rd.take_messages() {
                self.send_raft_message(msg).await?;
            }
        }

        // 11. Apply committed entries from light ready
        let light_committed = light_rd.committed_entries();
        if !light_committed.is_empty() {
            for entry in light_committed {
                if entry.data.is_empty() {
                    continue;
                }

                match entry.get_entry_type() {
                    EntryType::EntryNormal => {
                        // TODO: Apply to state machine
                        *self.committed_index.lock().await = entry.index;
                    }
                    EntryType::EntryConfChange | EntryType::EntryConfChangeV2 => {
                        self.apply_conf_change(entry).await?;
                    }
                }
            }
        }

        // 12. Advance light ready
        self.raw_node.advance_apply();

        Ok(())
    }

    /// Apply a configuration change
    async fn apply_conf_change(&mut self, entry: &Entry) -> Result<(), Box<dyn std::error::Error>> {
        let cc: ConfChange = protobuf::Message::parse_from_bytes(&entry.data)?;
        let cs = self.raw_node.apply_conf_change(&cc)?;

        // Update cached conf state
        *self.cached_conf_state.lock().await = cs;

        // Handle add/remove on transport layer
        match cc.get_change_type() {
            ConfChangeType::AddNode => {
                let node_id = cc.node_id;

                // Deserialize metadata
                if !cc.context.is_empty() {
                    if let Ok(metadata) = serde_json::from_slice::<NodeMetadata>(&cc.context) {
                        info!(self.logger, "Adding peer to transport"; "node_id" => node_id, "address" => &metadata.address);
                        self.transport.add_peer(node_id, metadata.address).await;
                    }
                }
            }
            ConfChangeType::RemoveNode => {
                let node_id = cc.node_id;
                info!(self.logger, "Removing peer from transport"; "node_id" => node_id);
                self.transport.remove_peer(node_id).await;
            }
            _ => {}
        }

        Ok(())
    }

    /// Send a Raft message to a peer via transport
    async fn send_raft_message(&self, msg: raft::prelude::Message) -> Result<(), TransportError> {
        let target_node = msg.to;

        // Serialize raft::prelude::Message to protobuf bytes
        let bytes = msg.write_to_bytes().map_err(|e| {
            TransportError::SerializationError {
                reason: format!("Failed to serialize message: {}", e),
            }
        })?;

        // Create GenericMessage with raft_message variant
        let generic_msg = raft_proto::GenericMessage {
            cluster_id: self.cluster_id,
            message: Some(raft_proto::generic_message::Message::RaftMessage(bytes)),
            ..Default::default()
        };

        self.transport.send_message(target_node, generic_msg).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::generic2::{InProcessServer, InProcessMessageSender, TransportLayer};
    use tokio::sync::mpsc;

    fn create_logger() -> Logger {
        use slog::Drain;
        let decorator = slog_term::PlainDecorator::new(std::io::stdout());
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        Logger::root(drain, slog::o!())
    }

    #[tokio::test]
    async fn test_raft_node_creation_single() {
        let logger = create_logger();
        let server = Arc::new(InProcessServer::new());
        let transport = Arc::new(TransportLayer::new(Arc::new(InProcessMessageSender::new(server))));
        let (_tx, rx) = mpsc::channel(10);

        let config = RaftNodeConfig {
            node_id: 1,
            cluster_id: 0,
            ..Default::default()
        };

        let node = RaftNode::new_single_node(config, transport, rx, logger).unwrap();
        assert_eq!(node.node_id, 1);
        assert_eq!(node.cluster_id, 0);
    }

    #[tokio::test]
    async fn test_raft_node_creation_multi() {
        let logger = create_logger();
        let server = Arc::new(InProcessServer::new());
        let transport = Arc::new(TransportLayer::new(Arc::new(InProcessMessageSender::new(server))));
        let (_tx, rx) = mpsc::channel(10);

        let config = RaftNodeConfig {
            node_id: 2,
            cluster_id: 1,
            ..Default::default()
        };

        let node = RaftNode::new_multi_node(config, transport, rx, logger).unwrap();
        assert_eq!(node.node_id, 2);
        assert_eq!(node.cluster_id, 1);
    }

    #[tokio::test]
    async fn test_raft_node_role_tracking() {
        let logger = create_logger();
        let server = Arc::new(InProcessServer::new());
        let transport = Arc::new(TransportLayer::new(Arc::new(InProcessMessageSender::new(server))));
        let (_tx, rx) = mpsc::channel(10);

        let config = RaftNodeConfig::default();
        let node = RaftNode::new_single_node(config, transport, rx, logger).unwrap();

        // Should start as follower
        assert_eq!(node.role().await, StateRole::Follower);
        assert!(!node.is_leader().await);
    }

    #[tokio::test]
    async fn test_raft_node_subscribe_role_changes() {
        let logger = create_logger();
        let server = Arc::new(InProcessServer::new());
        let transport = Arc::new(TransportLayer::new(Arc::new(InProcessMessageSender::new(server))));
        let (_tx, rx) = mpsc::channel(10);

        let config = RaftNodeConfig::default();
        let node = RaftNode::new_single_node(config, transport, rx, logger).unwrap();

        let mut role_rx = node.subscribe_role_changes();

        // Should be able to subscribe
        assert!(role_rx.try_recv().is_err()); // No changes yet
    }
}
