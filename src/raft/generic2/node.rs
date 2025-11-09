//! Raft Node (Layer 3)
//!
//! Provides a clean interface to raft-rs with separation of concerns:
//! - Mailbox: receives peer Raft messages from lower layers (via ClusterRouter → Transport)
//! - Methods: upper layer commands (propose, campaign, add_node, remove_node)

use crate::grpc::server::raft_proto::{self, GenericMessage};
#[cfg(not(feature = "persistent-storage"))]
use crate::raft::generic::storage::MemStorageWithSnapshot;
#[cfg(feature = "persistent-storage")]
use crate::raft::generic2::rocksdb_storage::RocksDBStorage;
use crate::raft::generic2::errors::TransportError;
use crate::raft::generic2::{EventBus, StateMachine, Transport};
use bytes::Bytes;
use protobuf::Message as ProtobufMessage;
use raft::prelude::*;
use raft::StateRole;
use serde::{Deserialize, Serialize};
use slog::{debug, info, warn, Logger};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, broadcast, Mutex, oneshot};
use tokio::time;

/// Storage type selection based on feature flag
#[cfg(feature = "persistent-storage")]
type RaftStorage = RocksDBStorage;

#[cfg(not(feature = "persistent-storage"))]
type RaftStorage = MemStorageWithSnapshot;

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

    /// Number of committed entries between snapshots (0 = disable snapshots)
    pub snapshot_interval: u64,

    /// Timeout for detecting failed nodes (0 = disable automatic removal)
    ///
    /// If a follower doesn't make progress for this duration, it will be
    /// considered failed and an event will be emitted for removal.
    /// Only applies when this node is the leader.
    pub failure_detection_timeout: Duration,

    /// Path for persistent storage (None = in-memory storage)
    ///
    /// When set, RocksDB will be used for storage (requires persistent-storage feature).
    /// When None, in-memory storage will be used (data lost on restart).
    pub storage_path: Option<PathBuf>,
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
            snapshot_interval: 1000, // Create snapshot every 1000 committed entries
            failure_detection_timeout: Duration::from_secs(30), // 30 seconds default
            storage_path: None, // Default to in-memory storage
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
/// - State Machine: Applies committed commands, emits events
/// - Event Bus: Broadcasts events to subscribers
///
/// # Type Parameters
/// * `SM` - State Machine type implementing the StateMachine trait
pub struct RaftNode<SM: StateMachine> {
    /// Raft node ID
    node_id: u64,

    /// Cluster ID for message routing
    cluster_id: u32,

    /// raft-rs RawNode
    raw_node: RawNode<RaftStorage>,

    /// Transport for sending messages to peers
    transport: Arc<dyn Transport>,

    /// Mailbox for incoming peer Raft messages
    message_rx: mpsc::Receiver<GenericMessage>,

    /// State machine for applying commands
    state_machine: Arc<Mutex<SM>>,

    /// Event bus for broadcasting state machine events
    event_bus: Arc<EventBus<SM::Event>>,

    /// Current role
    current_role: Arc<Mutex<StateRole>>,

    /// Role change broadcast channel
    role_change_tx: broadcast::Sender<RoleChange>,

    /// Last committed index
    committed_index: Arc<Mutex<u64>>,

    /// Cached configuration state
    cached_conf_state: Arc<Mutex<ConfState>>,

    /// Proposal tracking: sync_id -> oneshot sender for completion notification
    pending_proposals: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<(), String>>>>>,

    /// Snapshot configuration
    snapshot_interval: u64,

    /// Last snapshot index
    last_snapshot_index: Arc<Mutex<u64>>,

    /// Failure detection timeout (0 = disabled)
    failure_detection_timeout: Duration,

    /// Follower progress tracking: node_id -> (matched_index, last_update_time)
    /// Used for detecting failed nodes that stop making progress
    follower_progress: Arc<Mutex<HashMap<u64, (u64, time::Instant)>>>,

    /// Logger
    logger: Logger,
}

/// Helper function to create storage based on config and conf_state
#[cfg(feature = "persistent-storage")]
fn create_storage(
    config: &RaftNodeConfig,
    conf_state: ConfState,
) -> Result<RaftStorage, Box<dyn std::error::Error>> {
    match &config.storage_path {
        Some(storage_path) => {
            slog::info!(
                slog::Logger::root(slog::Discard, slog::o!()),
                "Creating RocksDB storage";
                "path" => ?storage_path,
                "node_id" => config.node_id,
                "cluster_id" => config.cluster_id
            );
            Ok(RocksDBStorage::open_or_create(storage_path, conf_state)?)
        }
        None => {
            Err("persistent-storage feature enabled but storage_path not provided".into())
        }
    }
}

#[cfg(not(feature = "persistent-storage"))]
fn create_storage(
    _config: &RaftNodeConfig,
    conf_state: ConfState,
) -> Result<RaftStorage, Box<dyn std::error::Error>> {
    // Always use in-memory storage when feature is disabled
    Ok(MemStorageWithSnapshot::new_with_conf_state(conf_state))
}

impl<SM: StateMachine> RaftNode<SM> {
    /// Create a new RaftNode with single-node bootstrap
    ///
    /// This initializes a single-node cluster that can accept proposals immediately.
    pub fn new_single_node(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        message_rx: mpsc::Receiver<GenericMessage>,
        state_machine: SM,
        event_bus: Arc<EventBus<SM::Event>>,
        logger: Logger,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let conf_state = ConfState::from((vec![config.node_id], vec![]));
        let storage = create_storage(&config, conf_state)?;

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
            state_machine: Arc::new(Mutex::new(state_machine)),
            event_bus,
            current_role: Arc::new(Mutex::new(StateRole::Follower)),
            role_change_tx,
            committed_index: Arc::new(Mutex::new(0)),
            cached_conf_state: Arc::new(Mutex::new(ConfState::from((vec![config.node_id], vec![])))),
            pending_proposals: Arc::new(Mutex::new(HashMap::new())),
            snapshot_interval: config.snapshot_interval,
            last_snapshot_index: Arc::new(Mutex::new(0)),
            failure_detection_timeout: config.failure_detection_timeout,
            follower_progress: Arc::new(Mutex::new(HashMap::new())),
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
        state_machine: SM,
        event_bus: Arc<EventBus<SM::Event>>,
        logger: Logger,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_with_conf_state(config, transport, message_rx, state_machine, event_bus, ConfState::default(), logger)
    }

    /// Create a new RaftNode with explicit ConfState (supports learners)
    ///
    /// This constructor allows creating a node with an explicit configuration
    /// that includes both voters and learners. This is used when a node needs
    /// to join as a learner in the management cluster.
    ///
    /// # Arguments
    /// * `config` - Node configuration (ID, cluster ID, etc.)
    /// * `transport` - Transport layer for network communication
    /// * `message_rx` - Receiver for incoming Raft messages
    /// * `state_machine` - State machine implementation
    /// * `event_bus` - Event bus for state changes
    /// * `conf_state` - Explicit configuration state with voters and learners
    /// * `logger` - Logger instance
    ///
    /// # Example
    /// ```no_run
    /// # use raftoral::raft::generic2::*;
    /// # use raft::prelude::ConfState;
    /// # use std::sync::Arc;
    /// # use tokio::sync::mpsc;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let conf_state = ConfState {
    ///     voters: vec![1, 2, 3],       // Existing voters
    ///     learners: vec![4],            // This node joins as learner
    ///     ..Default::default()
    /// };
    ///
    /// // Node 4 joins as a learner
    /// let (runtime, node) = ManagementRuntime::new_joining_learner(
    ///     config, transport, mailbox_rx, conf_state, registry, logger
    /// )?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn new_with_conf_state(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        message_rx: mpsc::Receiver<GenericMessage>,
        state_machine: SM,
        event_bus: Arc<EventBus<SM::Event>>,
        conf_state: ConfState,
        logger: Logger,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Use the provided conf_state for storage initialization
        let storage = create_storage(&config, conf_state.clone())?;

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
            state_machine: Arc::new(Mutex::new(state_machine)),
            event_bus,
            current_role: Arc::new(Mutex::new(StateRole::Follower)),
            role_change_tx,
            committed_index: Arc::new(Mutex::new(0)),
            cached_conf_state: Arc::new(Mutex::new(conf_state)),
            pending_proposals: Arc::new(Mutex::new(HashMap::new())),
            snapshot_interval: config.snapshot_interval,
            last_snapshot_index: Arc::new(Mutex::new(0)),
            failure_detection_timeout: config.failure_detection_timeout,
            follower_progress: Arc::new(Mutex::new(HashMap::new())),
            logger,
        })
    }

    /// Subscribe to role change notifications
    pub fn subscribe_role_changes(&self) -> broadcast::Receiver<RoleChange> {
        self.role_change_tx.subscribe()
    }

    /// Subscribe to state machine events
    pub fn subscribe_events(&self) -> broadcast::Receiver<SM::Event> {
        self.event_bus.subscribe()
    }

    /// Get reference to the event bus
    pub fn event_bus(&self) -> Arc<EventBus<SM::Event>> {
        self.event_bus.clone()
    }

    /// Get reference to the state machine
    pub fn state_machine(&self) -> Arc<Mutex<SM>> {
        self.state_machine.clone()
    }

    /// Get this node's ID
    pub fn node_id(&self) -> u64 {
        self.node_id
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

    /// Get the current leader ID (if known)
    ///
    /// Returns the node ID of the current leader, or None if unknown.
    /// Note: This is best-effort - the leader may change at any time.
    pub fn leader_id(&self) -> Option<u64> {
        let leader = self.raw_node.raft.leader_id;
        if leader == 0 || leader == raft::INVALID_ID {
            None
        } else {
            Some(leader)
        }
    }

    /// Propose a command (upper layer method, not mailbox)
    ///
    /// Serializes the command and submits it to Raft.
    /// Returns oneshot receiver that will be notified when proposal is committed and applied.
    pub async fn propose(&mut self, command: SM::Command) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        let (tx, rx) = oneshot::channel();

        // Generate random sync_id for tracking (avoids collisions across nodes)
        let sync_id = rand::random::<u64>();

        // Store the completion channel
        self.pending_proposals.lock().await.insert(sync_id, tx);

        // Serialize the command
        let data = serde_json::to_vec(&command)
            .map_err(|e| format!("Failed to serialize command: {}", e))?;

        // Put sync_id in context for tracking (8 bytes, little endian)
        let context = sync_id.to_le_bytes().to_vec();

        self.raw_node
            .propose(context, data)
            .map_err(|e| {
                // Remove from tracking on error
                let pending = self.pending_proposals.clone();
                tokio::spawn(async move {
                    pending.lock().await.remove(&sync_id);
                });
                format!("Failed to propose: {:?}", e)
            })?;

        debug!(self.logger, "Proposed command"; "sync_id" => sync_id);

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

        // Generate random sync_id for tracking (avoids collisions across nodes)
        let sync_id = rand::random::<u64>();

        // Store the completion channel
        self.pending_proposals.lock().await.insert(sync_id, tx);

        // Create node metadata (prepend sync_id for tracking)
        let metadata = NodeMetadata { address: address.clone() };
        let metadata_bytes = serde_json::to_vec(&metadata)
            .map_err(|e| format!("Failed to serialize metadata: {}", e))?;

        // Prepend sync_id to metadata (8 bytes LE + metadata)
        let mut context_bytes = sync_id.to_le_bytes().to_vec();
        context_bytes.extend_from_slice(&metadata_bytes);

        // Propose configuration change
        let mut cc = ConfChange::default();
        cc.set_change_type(ConfChangeType::AddNode);
        cc.node_id = node_id;
        cc.context = Bytes::from(context_bytes);

        self.raw_node
            .propose_conf_change(vec![], cc)
            .map_err(|e| {
                // Remove from tracking on error
                let pending = self.pending_proposals.clone();
                tokio::spawn(async move {
                    pending.lock().await.remove(&sync_id);
                });
                format!("Failed to propose conf change: {:?}", e)
            })?;

        // Add peer to transport
        self.transport.add_peer(node_id, address).await;

        debug!(self.logger, "Proposed add_node"; "sync_id" => sync_id, "node_id" => node_id);

        Ok(rx)
    }

    /// Remove a node from the cluster (upper layer method, not mailbox)
    pub async fn remove_node(&mut self, node_id: u64) -> Result<oneshot::Receiver<Result<(), String>>, String> {
        let (tx, rx) = oneshot::channel();

        // Generate random sync_id for tracking (avoids collisions across nodes)
        let sync_id = rand::random::<u64>();

        // Store the completion channel
        self.pending_proposals.lock().await.insert(sync_id, tx);

        // Put sync_id in context
        let context_bytes = sync_id.to_le_bytes().to_vec();

        // Propose configuration change
        let mut cc = ConfChange::default();
        cc.set_change_type(ConfChangeType::RemoveNode);
        cc.node_id = node_id;
        cc.context = Bytes::from(context_bytes);

        self.raw_node
            .propose_conf_change(vec![], cc)
            .map_err(|e| {
                // Remove from tracking on error
                let pending = self.pending_proposals.clone();
                tokio::spawn(async move {
                    pending.lock().await.remove(&sync_id);
                });
                format!("Failed to propose conf change: {:?}", e)
            })?;

        debug!(self.logger, "Proposed remove_node"; "sync_id" => sync_id, "node_id" => node_id);

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

    /// Run the Raft node from an Arc<Mutex<>> (helper for tests/async contexts)
    ///
    /// This is a helper that allows running a node that's wrapped in Arc<Mutex<>>
    /// without having to move it out first.
    pub async fn run_from_arc(node_arc: Arc<Mutex<RaftNode<SM>>>) -> Result<(), Box<dyn std::error::Error>> {
        let mut ticker = time::interval(Duration::from_millis(100));

        loop {
            // Lock for short duration to check for messages and tick
            let should_shutdown = {
                let mut node = node_arc.lock().await;

                // Tick the node
                node.raw_node.tick();

                // Try to receive messages (non-blocking)
                let mut found_message = true;
                while found_message {
                    match node.message_rx.try_recv() {
                        Ok(generic_msg) => {
                            if let Err(e) = node.handle_raft_message(generic_msg).await {
                                return Err(e);
                            }
                        }
                        Err(mpsc::error::TryRecvError::Empty) => {
                            found_message = false;
                        }
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            info!(node.logger, "Mailbox closed, shutting down");
                            return Ok(());
                        }
                    }
                }

                // Process ready
                if let Err(e) = node.on_ready().await {
                    return Err(e);
                }

                false
            };

            if should_shutdown {
                break;
            }

            // Wait for next tick
            ticker.tick().await;
        }

        Ok(())
    }

    /// Handle incoming message from mailbox
    ///
    /// This handles both:
    /// - Peer Raft messages (for consensus)
    /// - Forwarded proposals (from non-leader nodes)
    async fn handle_raft_message(&mut self, generic_msg: raft_proto::GenericMessage) -> Result<(), Box<dyn std::error::Error>> {
        // Extract the message from the oneof
        let message = generic_msg.message
            .ok_or("GenericMessage missing message field")?;

        match message {
            raft_proto::generic_message::Message::RaftMessage(bytes) => {
                // Deserialize raft::prelude::Message from protobuf bytes
                let raft_msg = raft::prelude::Message::parse_from_bytes(&bytes)?;

                // Step the message
                self.raw_node.step(raft_msg)?;
            }
            raft_proto::generic_message::Message::Propose(propose_msg) => {
                // Forwarded proposal from another node
                // Extract sync_id (for tracking the completion back to originator)
                let sync_id = if propose_msg.sync_id != 0 {
                    Some(propose_msg.sync_id)
                } else {
                    None
                };

                debug!(self.logger, "Received forwarded proposal";
                    "sync_id" => sync_id,
                    "data_len" => propose_msg.command_json.len()
                );

                // Propose locally (we should be the leader if we received this)
                if let Some(id) = sync_id {
                    // Put sync_id in context for tracking
                    let context = id.to_le_bytes().to_vec();
                    self.raw_node.propose(context, propose_msg.command_json)?;
                } else {
                    self.raw_node.propose(vec![], propose_msg.command_json)?;
                }

                // Note: The proposal completion will happen through normal on_ready() processing
                // The sync_id will be extracted from the context and the result sent back
            }
            _ => {
                // Other message types (campaign, add_node, etc.) should come via method calls
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

        // 1. Handle snapshot restoration
        if !ready.snapshot().is_empty() {
            let snapshot = ready.snapshot().clone();
            info!(self.logger, "Restoring from snapshot";
                "index" => snapshot.get_metadata().index,
                "term" => snapshot.get_metadata().term
            );

            // Restore state machine from snapshot data
            let mut sm = self.state_machine.lock().await;
            if let Err(e) = sm.restore(snapshot.get_data()) {
                warn!(self.logger, "Failed to restore state machine from snapshot"; "error" => %e);
            } else {
                info!(self.logger, "State machine restored from snapshot");
            }
            drop(sm);

            // Update cached conf state
            *self.cached_conf_state.lock().await = snapshot.get_metadata().get_conf_state().clone();

            // Update last snapshot index
            *self.last_snapshot_index.lock().await = snapshot.get_metadata().index;

            // Apply snapshot to storage
            let store = self.raw_node.raft.raft_log.store.clone();
            if let Err(e) = store.apply_snapshot_with_data(snapshot) {
                warn!(self.logger, "Failed to apply snapshot to storage"; "error" => ?e);
            }
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
                        // Extract sync_id from context (if present)
                        let sync_id = if entry.context.len() == 8 {
                            Some(u64::from_le_bytes([
                                entry.context[0],
                                entry.context[1],
                                entry.context[2],
                                entry.context[3],
                                entry.context[4],
                                entry.context[5],
                                entry.context[6],
                                entry.context[7],
                            ]))
                        } else {
                            None
                        };

                        // Deserialize and apply to state machine
                        let apply_result = match serde_json::from_slice::<SM::Command>(&entry.data) {
                            Ok(command) => {
                                debug!(self.logger, "Applying command";
                                    "index" => entry.index,
                                    "sync_id" => sync_id,
                                    "data_len" => entry.data.len()
                                );

                                // Apply to state machine
                                let mut sm = self.state_machine.lock().await;
                                match sm.apply(&command) {
                                    Ok(events) => {
                                        // Publish events to event bus
                                        if !events.is_empty() {
                                            let count = self.event_bus.publish_batch(events);
                                            debug!(self.logger, "Published events";
                                                "index" => entry.index,
                                                "subscriber_count" => count
                                            );
                                        }
                                        Ok(())
                                    }
                                    Err(e) => {
                                        warn!(self.logger, "State machine apply failed";
                                            "index" => entry.index,
                                            "error" => %e
                                        );
                                        Err(format!("Apply failed: {}", e))
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(self.logger, "Failed to deserialize command";
                                    "index" => entry.index,
                                    "error" => %e
                                );
                                Err(format!("Deserialization failed: {}", e))
                            }
                        };

                        // Complete pending proposal if tracked
                        if let Some(id) = sync_id {
                            if let Some(tx) = self.pending_proposals.lock().await.remove(&id) {
                                let _ = tx.send(apply_result);
                                debug!(self.logger, "Completed proposal"; "sync_id" => id, "index" => entry.index);
                            }
                        }

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
            #[cfg(feature = "persistent-storage")]
            {
                store.update_commit(commit)?;
            }
            #[cfg(not(feature = "persistent-storage"))]
            {
                store.wl().mut_hard_state().set_commit(commit);
            }
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
                        // Extract sync_id from context (if present)
                        let sync_id = if entry.context.len() == 8 {
                            Some(u64::from_le_bytes([
                                entry.context[0],
                                entry.context[1],
                                entry.context[2],
                                entry.context[3],
                                entry.context[4],
                                entry.context[5],
                                entry.context[6],
                                entry.context[7],
                            ]))
                        } else {
                            None
                        };

                        // Deserialize and apply to state machine
                        let apply_result = match serde_json::from_slice::<SM::Command>(&entry.data) {
                            Ok(command) => {
                                debug!(self.logger, "Applying command (light ready)";
                                    "index" => entry.index,
                                    "sync_id" => sync_id
                                );

                                // Apply to state machine
                                let mut sm = self.state_machine.lock().await;
                                match sm.apply(&command) {
                                    Ok(events) => {
                                        if !events.is_empty() {
                                            self.event_bus.publish_batch(events);
                                        }
                                        Ok(())
                                    }
                                    Err(e) => {
                                        warn!(self.logger, "State machine apply failed (light ready)";
                                            "index" => entry.index,
                                            "error" => %e
                                        );
                                        Err(format!("Apply failed: {}", e))
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(self.logger, "Failed to deserialize command (light ready)";
                                    "index" => entry.index,
                                    "error" => %e
                                );
                                Err(format!("Deserialization failed: {}", e))
                            }
                        };

                        // Complete pending proposal if tracked
                        if let Some(id) = sync_id {
                            if let Some(tx) = self.pending_proposals.lock().await.remove(&id) {
                                let _ = tx.send(apply_result);
                                debug!(self.logger, "Completed proposal (light ready)"; "sync_id" => id, "index" => entry.index);
                            }
                        }

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

        // 13. Create periodic snapshots if needed
        self.maybe_create_snapshot().await?;

        // 14. Check follower progress for failure detection
        self.check_follower_progress().await?;

        Ok(())
    }

    /// Create a snapshot if the configured interval has been reached
    async fn maybe_create_snapshot(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Skip if snapshots are disabled
        if self.snapshot_interval == 0 {
            return Ok(());
        }

        let current_committed = *self.committed_index.lock().await;
        let last_snapshot = *self.last_snapshot_index.lock().await;

        // Check if we've committed enough entries since last snapshot
        if current_committed - last_snapshot < self.snapshot_interval {
            return Ok(());
        }

        info!(self.logger, "Creating snapshot";
            "committed_index" => current_committed,
            "last_snapshot_index" => last_snapshot
        );

        // Get state machine snapshot
        let sm = self.state_machine.lock().await;
        let snapshot_data = match sm.snapshot() {
            Ok(data) => data,
            Err(e) => {
                warn!(self.logger, "Failed to create state machine snapshot"; "error" => %e);
                return Ok(()); // Don't fail the whole operation
            }
        };
        drop(sm);

        // Get current conf state
        let conf_state = self.cached_conf_state.lock().await.clone();

        // Create Raft snapshot
        let store = self.raw_node.raft.raft_log.store.clone();
        let mut snapshot = Snapshot::default();

        let meta = snapshot.mut_metadata();
        meta.index = current_committed;
        meta.term = store.rl().hard_state().term;
        meta.set_conf_state(conf_state);

        snapshot.set_data(bytes::Bytes::from(snapshot_data));

        // Apply snapshot to storage
        store.apply_snapshot_with_data(snapshot)?;

        // Update last snapshot index
        *self.last_snapshot_index.lock().await = current_committed;

        info!(self.logger, "Snapshot created successfully"; "index" => current_committed);

        Ok(())
    }

    /// Apply a configuration change
    async fn apply_conf_change(&mut self, entry: &Entry) -> Result<(), Box<dyn std::error::Error>> {
        let cc: ConfChange = protobuf::Message::parse_from_bytes(&entry.data)?;

        // Extract sync_id from context (first 8 bytes if present)
        let (sync_id, metadata_start) = if cc.context.len() >= 8 {
            let id = u64::from_le_bytes([
                cc.context[0],
                cc.context[1],
                cc.context[2],
                cc.context[3],
                cc.context[4],
                cc.context[5],
                cc.context[6],
                cc.context[7],
            ]);
            (Some(id), 8)
        } else {
            (None, 0)
        };

        let cs = self.raw_node.apply_conf_change(&cc)?;

        // Update cached conf state
        *self.cached_conf_state.lock().await = cs;

        // Handle add/remove on transport layer
        let conf_change_result = match cc.get_change_type() {
            ConfChangeType::AddNode => {
                let node_id = cc.node_id;

                // Deserialize metadata (after sync_id bytes)
                if cc.context.len() > metadata_start {
                    match serde_json::from_slice::<NodeMetadata>(&cc.context[metadata_start..]) {
                        Ok(metadata) => {
                            info!(self.logger, "Adding peer to transport";
                                "node_id" => node_id,
                                "address" => &metadata.address,
                                "sync_id" => sync_id
                            );
                            self.transport.add_peer(node_id, metadata.address).await;
                            Ok(())
                        }
                        Err(e) => Err(format!("Failed to deserialize node metadata: {}", e)),
                    }
                } else {
                    Ok(()) // No metadata, just tracking
                }
            }
            ConfChangeType::RemoveNode => {
                let node_id = cc.node_id;
                info!(self.logger, "Removing peer from transport";
                    "node_id" => node_id,
                    "sync_id" => sync_id
                );
                self.transport.remove_peer(node_id).await;
                Ok(())
            }
            _ => Ok(()),
        };

        // Complete pending proposal if tracked
        if let Some(id) = sync_id {
            if let Some(tx) = self.pending_proposals.lock().await.remove(&id) {
                let _ = tx.send(conf_change_result);
                debug!(self.logger, "Completed conf change proposal"; "sync_id" => id);
            }
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

    /// Check follower progress and detect failed nodes
    ///
    /// This is called periodically when this node is the leader. It tracks the
    /// matched index for each follower and emits events when followers stop
    /// making progress for longer than the configured timeout.
    async fn check_follower_progress(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Skip if failure detection is disabled
        if self.failure_detection_timeout.is_zero() {
            return Ok(());
        }

        // Only check when we're the leader
        if !self.is_leader().await {
            return Ok(());
        }

        let now = time::Instant::now();
        let mut progress_map = self.follower_progress.lock().await;
        let mut failed_nodes = Vec::new();

        // Check each peer's progress
        let prs = self.raw_node.raft.prs();
        for (&peer_id, progress) in prs.iter() {
            // Skip ourselves
            if peer_id == self.node_id {
                continue;
            }

            let matched = progress.matched;

            // Check if this peer made progress
            if let Some((last_matched, last_update)) = progress_map.get(&peer_id) {
                if matched > *last_matched {
                    // Progress made, update tracking
                    progress_map.insert(peer_id, (matched, now));
                } else {
                    // No progress, check if timeout exceeded
                    let elapsed = now.duration_since(*last_update);
                    if elapsed > self.failure_detection_timeout {
                        warn!(self.logger, "Follower failed to make progress";
                            "node_id" => peer_id,
                            "matched" => matched,
                            "timeout" => ?self.failure_detection_timeout,
                            "elapsed" => ?elapsed
                        );
                        failed_nodes.push(peer_id);
                    }
                }
            } else {
                // First time seeing this peer, track it
                progress_map.insert(peer_id, (matched, now));
            }
        }

        drop(progress_map);

        // Handle failed nodes
        for node_id in failed_nodes {
            // Call state machine to get failure events
            let mut sm = self.state_machine.lock().await;
            let events = sm.on_follower_failed(node_id);
            drop(sm);

            // Publish events
            if !events.is_empty() {
                let count = self.event_bus.publish_batch(events);
                info!(self.logger, "Published follower failure events";
                    "node_id" => node_id,
                    "subscriber_count" => count
                );
            }

            // Remove from progress tracking (we only emit event once)
            self.follower_progress.lock().await.remove(&node_id);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::generic2::{InProcessServer, InProcessMessageSender, KvCommand, KvStateMachine, TransportLayer};
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

        let state_machine = KvStateMachine::new();
        let event_bus = Arc::new(EventBus::new(100));

        let node = RaftNode::new_single_node(config, transport, rx, state_machine, event_bus, logger).unwrap();
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

        let state_machine = KvStateMachine::new();
        let event_bus = Arc::new(EventBus::new(100));

        let node = RaftNode::new_multi_node(config, transport, rx, state_machine, event_bus, logger).unwrap();
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
        let state_machine = KvStateMachine::new();
        let event_bus = Arc::new(EventBus::new(100));

        let node = RaftNode::new_single_node(config, transport, rx, state_machine, event_bus, logger).unwrap();

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
        let state_machine = KvStateMachine::new();
        let event_bus = Arc::new(EventBus::new(100));

        let node = RaftNode::new_single_node(config, transport, rx, state_machine, event_bus, logger).unwrap();

        let mut role_rx = node.subscribe_role_changes();

        // Should be able to subscribe
        assert!(role_rx.try_recv().is_err()); // No changes yet
    }

    #[tokio::test]
    async fn test_raft_node_subscribe_events() {
        let logger = create_logger();
        let server = Arc::new(InProcessServer::new());
        let transport = Arc::new(TransportLayer::new(Arc::new(InProcessMessageSender::new(server))));
        let (_tx, rx) = mpsc::channel(10);

        let config = RaftNodeConfig::default();
        let state_machine = KvStateMachine::new();
        let event_bus = Arc::new(EventBus::new(100));

        let node = RaftNode::new_single_node(config, transport, rx, state_machine, event_bus, logger).unwrap();

        let mut event_rx = node.subscribe_events();

        // Should be able to subscribe
        assert!(event_rx.try_recv().is_err()); // No events yet
    }

    #[tokio::test]
    async fn test_raft_node_snapshot_restore() {
        let logger = create_logger();
        let server = Arc::new(InProcessServer::new());
        let transport = Arc::new(TransportLayer::new(Arc::new(InProcessMessageSender::new(server))));
        let (_tx, rx) = mpsc::channel(10);

        let config = RaftNodeConfig::default();
        let mut state_machine = KvStateMachine::new();

        // Populate state machine with data
        state_machine.apply(&KvCommand::Set {
            key: "key1".to_string(),
            value: "value1".to_string(),
        }).unwrap();
        state_machine.apply(&KvCommand::Set {
            key: "key2".to_string(),
            value: "value2".to_string(),
        }).unwrap();

        // Create snapshot from state machine
        let snapshot_data = state_machine.snapshot().unwrap();

        // Create new state machine and restore
        let mut restored_sm = KvStateMachine::new();
        restored_sm.restore(&snapshot_data).unwrap();

        // Verify restored state
        assert_eq!(restored_sm.get("key1"), Some(&"value1".to_string()));
        assert_eq!(restored_sm.get("key2"), Some(&"value2".to_string()));

        // Verify it works with node
        let event_bus = Arc::new(EventBus::new(100));
        let node = RaftNode::new_single_node(config, transport, rx, restored_sm, event_bus, logger).unwrap();

        // State machine should have the restored data
        let sm = node.state_machine();
        assert_eq!(sm.lock().await.get("key1"), Some(&"value1".to_string()));
        assert_eq!(sm.lock().await.get("key2"), Some(&"value2".to_string()));
    }
}
