use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, broadcast};
use raft::{prelude::*, StateRole};
use super::storage::MemStorageWithSnapshot;
use slog::{Drain, Logger};
use protobuf::Message as PbMessage;
use crate::raft::generic::message::{Message, CommandExecutor};
use crate::raft::generic::cluster::RoleChange;

pub type SyncCallback = tokio::sync::oneshot::Sender<Result<(), Box<dyn std::error::Error + Send + Sync>>>;

/// Configuration for automatic snapshot creation and management
#[derive(Clone, Debug)]
pub struct SnapshotConfig {
    /// Create snapshot every N log entries
    pub snapshot_interval: u64,

    /// Maximum log entries before forcing snapshot
    pub max_log_size: u64,

    /// Compact logs older than latest snapshot
    pub auto_compact: bool,

    /// Keep at least N log entries before snapshot for safety
    pub entries_before_snapshot: u64,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            snapshot_interval: 1000,
            max_log_size: 10000,
            auto_compact: true,
            entries_before_snapshot: 100,
        }
    }
}

pub struct RaftNode<E: CommandExecutor> {
    raft_group: RawNode<MemStorageWithSnapshot>,
    #[allow(dead_code)] // Reserved for future use
    storage: Arc<RwLock<HashMap<String, Vec<u8>>>>,
    node_id: u64,
    logger: Logger,

    // For multi-node communication
    mailbox: mpsc::UnboundedReceiver<Message<E::Command>>,
    /// Transport interface for sending messages to peers
    transport: Arc<dyn crate::raft::generic::transport::TransportInteraction<Message<E::Command>>>,
    /// Cached Raft configuration (voter IDs) - shared with RaftCluster for direct access
    /// Updated whenever configuration changes, eliminating need for QueryConfig messages
    cached_config: Arc<RwLock<Vec<u64>>>,
    /// Cached full configuration state (voters and learners separately)
    cached_conf_state: Arc<RwLock<raft::prelude::ConfState>>,

    // Command executor for applying committed commands
    executor: Arc<E>,

    // Role change notifications
    role_change_tx: broadcast::Sender<RoleChange>,

    // Track current role to detect changes
    current_role: StateRole,

    // Track current committed index for checkpoint history
    committed_index: u64,

    // Snapshot configuration
    snapshot_config: SnapshotConfig,

    // Applied entries channel: send (log_index, context) when entries are applied
    // Used by RaftCluster for synchronous command tracking
    applied_tx: mpsc::UnboundedSender<(u64, Vec<u8>)>,
}

impl<E: CommandExecutor + 'static> RaftNode<E> {
    pub fn new(
        node_id: u64,
        mailbox: mpsc::UnboundedReceiver<Message<E::Command>>,
        transport: Arc<dyn crate::raft::generic::transport::TransportInteraction<Message<E::Command>>>,
        executor: Arc<E>,
        role_change_tx: broadcast::Sender<RoleChange>,
        cached_config: Arc<RwLock<Vec<u64>>>,
        cached_conf_state: Arc<RwLock<raft::prelude::ConfState>>,
        applied_tx: mpsc::UnboundedSender<(u64, Vec<u8>)>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let node_count = transport.list_nodes().len();
        let is_single_node = node_count == 1;

        let config = Config {
            id: node_id,
            election_tick: 10,
            heartbeat_tick: 3,
            applied: 0,
            max_size_per_msg: 1024 * 1024 * 1024,
            max_inflight_msgs: 256,
            check_quorum: !is_single_node, // Disable quorum check for single-node
            pre_vote: !is_single_node, // Disable pre-vote for single node
            ..Default::default()
        };

        let storage = MemStorageWithSnapshot::new();

        // Set up the initial cluster configuration
        if is_single_node {
            // Bootstrap node: Initialize with self in configuration
            let mut initial_state = ConfState::default();
            initial_state.voters.push(node_id);
            storage.wl().set_conf_state(initial_state.clone());
            *cached_config.write().unwrap() = vec![node_id];
            *cached_conf_state.write().unwrap() = initial_state;

            // Cache first entry info (will be set after first leader election creates index 1)
            // For now, leave as None - will be populated after the node becomes leader
        } else {
            // Joining node: Initialize with discovered voter configuration from transport
            // This solves the "empty configuration" problem that causes ConfChange apply failures
            let discovered_voters = transport.get_discovered_voters();

            if !discovered_voters.is_empty() {
                // We have discovered configuration - use it!
                let mut initial_state = ConfState::default();
                initial_state.voters = discovered_voters.clone();
                storage.wl().set_conf_state(initial_state.clone());
                *cached_config.write().unwrap() = discovered_voters.clone();
                *cached_conf_state.write().unwrap() = initial_state;
                slog::info!(slog::Logger::root(slog::Discard, slog::o!()),
                    "Initialized joining node with discovered voters";
                    "voters" => ?discovered_voters);
            } else{
                // Fallback: Use transport peer list for cached_config only
                // This maintains backward compatibility for tests that don't use discovery
                let peer_ids: Vec<u64> = transport.list_nodes().into_iter()
                    .filter(|&id| id != node_id)  // Don't include self
                    .collect();
                *cached_config.write().unwrap() = peer_ids;
                // cached_conf_state stays empty - old behavior for backward compatibility
            }
        }

        // Create logger for this node
        let decorator = slog_term::TermDecorator::new().build();
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        let logger = slog::Logger::root(drain, slog::o!("node_id" => node_id));

        let raft_group = RawNode::new(&config, storage, &logger)?;

        let node = RaftNode {
            raft_group,
            storage: Arc::new(RwLock::new(HashMap::new())),
            node_id,
            logger,
            mailbox,
            transport,
            cached_config,
            cached_conf_state,
            executor,
            role_change_tx,
            current_role: StateRole::Follower, // Start as follower
            committed_index: 0,
            snapshot_config: SnapshotConfig::default(),
            applied_tx,
        };

        Ok(node)
    }

    pub fn propose_command(&mut self, command: E::Command, sync_id: Option<u64>) -> Result<(), Box<dyn std::error::Error>> {
        // Serialize command directly (no wrapper needed - sync_id goes in context)
        let data = serde_json::to_vec(&command)?;

        // Put sync_id in context as 8 bytes for synchronous tracking
        let context = if let Some(id) = sync_id {
            id.to_le_bytes().to_vec()
        } else {
            vec![]
        };

        self.raft_group.propose(context, data)?;
        Ok(())
    }

    pub fn propose_conf_change_v2(&mut self, conf_change: ConfChangeV2, sync_id: Option<u64>) -> Result<(), Box<dyn std::error::Error>> {
        // Put sync_id in context as 8 bytes for synchronous tracking
        let context = if let Some(id) = sync_id {
            id.to_le_bytes().to_vec()
        } else {
            vec![]
        };

        // raft-rs 0.7.0 supports ConfChangeV2 through the ConfChangeI trait
        // Pass ConfChangeV2 directly - it will create EntryConfChangeV2 entries
        self.raft_group.propose_conf_change(context, conf_change)?;
        Ok(())
    }

    pub fn step(&mut self, msg: raft::prelude::Message) -> Result<(), Box<dyn std::error::Error>> {
        if let Err(e) = self.raft_group.step(msg.clone()) {
            slog::warn!(self.logger, "Failed to step message"; "from" => msg.from, "to" => msg.to,
                       "msg_type" => ?msg.get_msg_type(), "error" => %e);
            return Err(Box::new(e));
        }
        Ok(())
    }

    pub fn tick(&mut self) {
        self.raft_group.tick();
    }

    /// Process ready state - can be overridden in implementations
    pub fn on_ready(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.raft_group.has_ready() {
            return Ok(());
        }

        let store = self.raft_group.raft.raft_log.store.clone();
        let mut ready = self.raft_group.ready();

        // Send out messages to other nodes
        if !ready.messages().is_empty() {
            self.send_messages(ready.take_messages())?;
        }

        // Handle snapshots first
        if !ready.snapshot().is_empty() {
            let snapshot = ready.snapshot().clone();

            // Extract and restore executor state from snapshot data
            let snapshot_data = snapshot.get_data();
            if !snapshot_data.is_empty() {
                self.executor.restore_from_snapshot(snapshot_data)?;
                self.committed_index = snapshot.get_metadata().get_index();

                slog::info!(self.logger, "Restored state from snapshot";
                    "snapshot_index" => snapshot.get_metadata().get_index(),
                    "data_size" => snapshot_data.len()
                );
            }

            // Apply to Raft storage (using our custom method that preserves data)
            store.apply_snapshot_with_data(snapshot)?;
        }

        // Handle committed entries BEFORE persisting new entries
        let committed_entries = ready.committed_entries();
        if !committed_entries.is_empty() {
            self.handle_committed_entries(committed_entries)?;
        }

        // CRITICAL: Append entries to storage BEFORE calling advance()
        if !ready.entries().is_empty() {
            store.wl().append(ready.entries())?;
        }

        // Persist hard state changes
        if let Some(hs) = ready.hs() {
            store.wl().set_hardstate(hs.clone());
        }

        // Handle soft state changes (role changes)
        if let Some(ss) = ready.ss() {
            let new_role = ss.raft_state;
            if new_role != self.current_role {
                // Role changed - emit notification
                let role_change = match new_role {
                    StateRole::Leader => RoleChange::BecameLeader(self.node_id),
                    StateRole::Follower => RoleChange::BecameFollower(self.node_id),
                    StateRole::Candidate => RoleChange::BecameCandidate(self.node_id),
                    StateRole::PreCandidate => RoleChange::BecameFollower(self.node_id), // Treat as follower
                };
                let _ = self.role_change_tx.send(role_change);
                self.current_role = new_role;
            }
        }

        // Send persisted messages
        if !ready.persisted_messages().is_empty() {
            self.send_messages(ready.take_persisted_messages())?;
        }

        // NOW it's safe to advance - entries are persisted
        let mut light_rd = self.raft_group.advance(ready);

        // Handle light Ready state
        if let Some(commit) = light_rd.commit_index() {
            store.wl().mut_hard_state().set_commit(commit);
        }

        // Handle any additional messages from light ready
        if !light_rd.messages().is_empty() {
            self.send_messages(light_rd.take_messages())?;
        }

        // Handle additional committed entries from light ready
        let light_committed_entries = light_rd.take_committed_entries();
        if !light_committed_entries.is_empty() {
            self.handle_committed_entries(&light_committed_entries)?;
        }

        // Advance apply index
        self.raft_group.advance_apply();

        Ok(())
    }

    fn send_messages(&self, messages: Vec<raft::prelude::Message>) -> Result<(), Box<dyn std::error::Error>> {
        for msg in messages {
            let msg_to = msg.to;
            let raft_msg = Message::Raft(msg);
            if let Err(e) = self.transport.send_message_to_node(msg_to, raft_msg) {
                slog::warn!(self.logger, "Failed to send message"; "to" => msg_to, "error" => %e);
            }
        }
        Ok(())
    }

    fn handle_committed_entries(&mut self, entries: &[Entry]) -> Result<Option<u64>, Box<dyn std::error::Error>> {
        let mut last_applied_index = None;

        for entry in entries {
            if entry.data.is_empty() {
                // Empty entry, usually a leadership change
                last_applied_index = Some(entry.index);
                continue;
            }

            match entry.entry_type {
                EntryType::EntryNormal => {
                    // Deserialize command directly (no wrapper - sync_id is in context)
                    if let Ok(command) = serde_json::from_slice::<E::Command>(&entry.data) {
                        // Track committed index
                        self.committed_index = entry.index;

                        let result = self.apply_command(&command, entry.index);

                        // Send applied entry notification with context (for sync tracking)
                        // Context contains the sync_id as 8 bytes
                        let _ = self.applied_tx.send((entry.index, entry.context.to_vec()));

                        result?;
                        last_applied_index = Some(entry.index);
                    }
                },
                EntryType::EntryConfChange => {
                    slog::warn!(self.logger, "Legacy ConfChange not supported - use ConfChangeV2"; "index" => entry.index);
                    // We only support modern ConfChangeV2 for simplicity
                    last_applied_index = Some(entry.index);
                },
                EntryType::EntryConfChangeV2 => {
                    slog::info!(self.logger, "Processing conf change v2 entry"; "index" => entry.index);

                    if let Ok(conf_change_v2) = ConfChangeV2::parse_from_bytes(&entry.data) {
                        // Parse metadata to add peer to transport (all nodes need to know about peers)
                        if !conf_change_v2.context.is_empty() {
                            if let Ok(meta) = crate::raft::generic::transport::NodeMetadata::from_bytes(&conf_change_v2.context) {
                                let _ = self.transport.add_peer(meta.node_id, meta.address.clone());
                            }
                        }

                        // Apply the configuration change
                        self.apply_conf_change_v2(&conf_change_v2)?;
                    } else {
                        slog::error!(self.logger, "Failed to parse conf change v2 from entry data");
                    }

                    // Send applied entry notification with context (for sync tracking)
                    let _ = self.applied_tx.send((entry.index, entry.context.to_vec()));

                    last_applied_index = Some(entry.index);
                },
            }
        }

        Ok(last_applied_index)
    }

    fn apply_command(&mut self, command: &E::Command, log_index: u64) -> Result<(), Box<dyn std::error::Error>> {
        // Use the executor to apply the command with log index
        self.executor.apply_with_index(command, &self.logger, log_index)
    }


    fn apply_conf_change_v2(&mut self, conf_change: &ConfChangeV2) -> Result<(), Box<dyn std::error::Error>> {
        slog::info!(self.logger, "Applying ConfChangeV2"; "transition" => ?conf_change.transition);

        // Parse metadata from context if present
        let metadata = if !conf_change.context.is_empty() {
            match crate::raft::generic::transport::NodeMetadata::from_bytes(&conf_change.context) {
                Ok(meta) => Some(meta),
                Err(e) => {
                    slog::warn!(self.logger, "Failed to parse ConfChange metadata"; "error" => ?e);
                    None
                }
            }
        } else {
            None
        };

        // Process individual changes in the ConfChangeV2
        for change in conf_change.changes.iter() {
            let node_id = change.node_id;
            match change.change_type {
                ConfChangeType::AddNode => {
                    slog::info!(self.logger, "Adding node to cluster (v2)"; "node_id" => node_id);

                    // Add peer to transport when ConfChange is applied (if not already added)
                    // On the leader, this was already added before proposing (to enable bidirectional communication)
                    // On followers, this is where the peer is first added
                    if let Some(ref meta) = metadata {
                        if meta.node_id == node_id {
                            match self.transport.add_peer(meta.node_id, meta.address.clone()) {
                                Ok(()) => {
                                    slog::info!(self.logger, "Added peer to transport during ConfChange apply";
                                               "node_id" => node_id, "address" => &meta.address);
                                }
                                Err(e) => {
                                    // May already exist (if we're the leader who added it before proposing)
                                    slog::debug!(self.logger, "Peer may already be in transport";
                                                "node_id" => node_id, "address" => &meta.address, "error" => %e);
                                }
                            }
                        }
                    }
                },
                ConfChangeType::RemoveNode => {
                    slog::info!(self.logger, "Removing node from cluster (v2)"; "node_id" => node_id);
                    // Notify the executor so it can handle ownership reassignment
                    self.executor.on_node_removed(node_id, &self.logger);

                    // Update transport to remove peer
                    match self.transport.remove_peer(node_id) {
                        Ok(()) => {
                            slog::info!(self.logger, "Removed peer from transport"; "node_id" => node_id);
                        }
                        Err(e) => {
                            slog::error!(self.logger, "Failed to remove peer from transport";
                                        "node_id" => node_id, "error" => %e);
                        }
                    }
                },
                ConfChangeType::AddLearnerNode => {
                    // Learner support removed - use AddNode directly instead
                    slog::error!(self.logger, "AddLearnerNode not supported - use AddNode instead"; "node_id" => node_id);
                    return Err("AddLearnerNode not supported, use AddNode instead".into());
                },
            }
        }

        // Apply the configuration change and get the new configuration state
        let cs = self.raft_group.apply_conf_change(conf_change)?;
        slog::info!(self.logger, "ConfChange applied"; "new_conf" => ?cs);

        // Update cached configuration state
        *self.cached_conf_state.write().unwrap() = cs.clone();

        // Update cached configuration with voter node IDs
        let all_nodes: Vec<u64> = cs.voters.into_iter().collect();
        *self.cached_config.write().unwrap() = all_nodes.clone();

        slog::info!(self.logger, "Updated cached config"; "all_nodes" => ?all_nodes);

        Ok(())
    }

    pub fn is_leader(&self) -> bool {
        self.raft_group.raft.state == raft::StateRole::Leader
    }

    /// Get the current Raft configuration state (voters and learners)
    pub fn get_conf_state(&self) -> raft::prelude::ConfState {
        self.raft_group.raft.prs().conf().to_conf_state()
    }

    /// Get first log entry info for bootstrapping joining nodes
    /// Returns (index, term) or None if log is empty
    pub fn get_first_entry_info(&self) -> Option<(u64, u64)> {
        let first_index = self.raft_group.raft.raft_log.first_index();
        let last_index = self.raft_group.raft.raft_log.last_index();

        // Check if we have at least one entry
        if first_index <= last_index {
            // Get the term of the first entry
            if let Ok(term) = self.raft_group.raft.raft_log.term(first_index) {
                return Some((first_index, term));
            }
        }
        None
    }

    pub fn campaign(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.raft_group.campaign()?;
        Ok(())
    }


    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    pub fn get_raft_status(&self) -> String {
        format!("Node {}: {:?}, Term: {}, Leader: {}",
                self.node_id,
                self.raft_group.raft.state,
                self.raft_group.raft.term,
                self.raft_group.raft.leader_id)
    }


    // Main event loop for the node
    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut tick_timer = tokio::time::interval(Duration::from_millis(100));
        let mut now = Instant::now();

        loop {
            tokio::select! {
                // Handle incoming messages
                msg = self.mailbox.recv() => {
                    match msg {
                        Some(Message::Propose { command, sync_id, .. }) => {
                            // Propose the command with sync_id for synchronous tracking
                            match self.propose_command(command, sync_id) {
                                Ok(_) => {},
                                Err(e) => {
                                    slog::error!(self.logger, "Failed to propose command"; "error" => %e);
                                }
                            }
                        },
                        Some(Message::Raft(raft_msg)) => {
                            if let Err(e) = self.step(raft_msg) {
                                slog::error!(self.logger, "Failed to step raft message"; "error" => %e);
                            }
                        },
                        // ConfChangeV2 message variant removed - use AddNode/RemoveNode instead
                        Some(Message::Campaign) => {
                            match self.campaign() {
                                Ok(_) => {},
                                Err(e) => {
                                    slog::error!(self.logger, "Failed to start campaign"; "error" => %e);
                                }
                            }
                        },
                        Some(Message::AddNode { node_id, address, sync_id }) => {
                            slog::info!(self.logger, "Preparing to add node as voter"; "node_id" => node_id, "address" => &address);

                            // Add node directly as voter (simpler approach with proper initialization)
                            // With discovered configuration, new nodes can apply ConfChanges correctly
                            let mut conf_change = ConfChangeV2::default();
                            let mut change = ConfChangeSingle::default();
                            change.change_type = ConfChangeType::AddNode.into();
                            change.node_id = node_id;
                            conf_change.changes.push(change);

                            // Embed NodeMetadata in context field
                            use crate::raft::generic::transport::NodeMetadata;
                            let metadata = NodeMetadata {
                                node_id,
                                address: address.clone(),
                            };
                            match metadata.to_bytes() {
                                Ok(metadata_bytes) => {
                                    conf_change.context = metadata_bytes.into();
                                },
                                Err(e) => {
                                    slog::error!(self.logger, "Failed to serialize node metadata"; "error" => %e);
                                    continue;
                                }
                            }

                            match self.propose_conf_change_v2(conf_change, sync_id) {
                                Ok(_) => {
                                    slog::info!(self.logger, "Proposed adding node"; "node_id" => node_id, "address" => address);

                                    // CRITICAL: After proposing ConfChange, force replication to all peers
                                    // Progress might be paused, preventing automatic replication
                                    if self.is_leader() {
                                        let conf_state = self.raft_group.raft.prs().conf().to_conf_state();
                                        for peer_id in conf_state.voters {
                                            if peer_id != self.node_id {
                                                self.raft_group.raft.send_append(peer_id);
                                                slog::debug!(self.logger, "Triggered send_append to replicate ConfChange";
                                                           "peer_id" => peer_id);
                                            }
                                        }

                                        // Process ready state immediately to send the messages
                                        if let Err(e) = self.on_ready() {
                                            slog::warn!(self.logger, "Failed to process ready after send_append"; "error" => %e);
                                        }
                                    }
                                },
                                Err(e) => {
                                    slog::error!(self.logger, "Failed to propose add node"; "error" => %e, "node_id" => node_id);
                                }
                            }
                        },
                        Some(Message::RemoveNode { node_id, sync_id }) => {
                            // Create ConfChangeV2 for removing node
                            let mut conf_change = ConfChangeV2::default();
                            let mut change = ConfChangeSingle::default();
                            change.change_type = ConfChangeType::RemoveNode.into();
                            change.node_id = node_id;
                            conf_change.changes.push(change);

                            match self.propose_conf_change_v2(conf_change, sync_id) {
                                Ok(_) => {
                                    slog::info!(self.logger, "Proposed removing node"; "node_id" => node_id);
                                },
                                Err(e) => {
                                    slog::error!(self.logger, "Failed to propose remove node"; "error" => %e, "node_id" => node_id);
                                }
                            }
                        },
                        None => break,
                    }
                },

                // Periodic tick
                _ = tick_timer.tick() => {
                    let elapsed = now.elapsed();
                    if elapsed >= Duration::from_millis(100) {
                        self.tick();
                        now = Instant::now();
                    }
                }
            }

            // Process ready state after each iteration
            if let Err(e) = self.on_ready() {
                slog::error!(self.logger, "Failed to process ready state"; "error" => %e);
            }

            // Check if we should create a snapshot after processing ready state
            if let Err(e) = self.check_and_create_snapshot() {
                slog::error!(self.logger, "Failed to check/create snapshot"; "error" => %e);
            }
        }

        Ok(())
    }
}

// Generic snapshot methods - available for all executors
impl<E: CommandExecutor> RaftNode<E> {

    /// Create a snapshot using the executor's snapshot implementation
    pub fn create_snapshot(&self) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let snapshot_data = self.executor.create_snapshot(self.committed_index)?;

        slog::info!(self.logger, "Created snapshot";
            "snapshot_index" => self.committed_index,
            "data_size" => snapshot_data.len()
        );

        Ok(snapshot_data)
    }

    /// Apply a snapshot to restore state using the executor's restore implementation
    pub fn apply_snapshot(&mut self, snapshot_data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        slog::info!(self.logger, "Applying snapshot";
            "data_size" => snapshot_data.len()
        );

        // Restore state via executor
        self.executor.restore_from_snapshot(snapshot_data)?;

        Ok(())
    }

    /// Check if a snapshot should be created based on log size
    /// Delegates to executor for custom logic
    fn should_create_snapshot(&self) -> Result<bool, Box<dyn std::error::Error>> {
        let store = self.raft_group.raft.raft_log.store.clone();
        let first_index = store.first_index()?;
        let last_applied = self.committed_index;

        // Calculate log size
        let log_size = if last_applied >= first_index {
            last_applied - first_index + 1
        } else {
            0
        };

        // Delegate to executor for custom logic
        let should_snapshot = self.executor.should_create_snapshot(log_size, self.snapshot_config.snapshot_interval);

        if should_snapshot {
            slog::debug!(self.logger, "Snapshot check triggered";
                "log_size" => log_size,
                "first_index" => first_index,
                "last_applied" => last_applied,
                "threshold" => self.snapshot_config.snapshot_interval
            );
        }

        Ok(should_snapshot)
    }

    /// Trigger automatic snapshot creation using executor's implementation
    fn trigger_snapshot_creation(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let last_applied = self.committed_index;

        // Get snapshot data from executor
        let snapshot_data = self.executor.create_snapshot(last_applied)?;
        let data_size = snapshot_data.len(); // Save size before moving

        // Get raft metadata
        let store = self.raft_group.raft.raft_log.store.clone();
        let term = store.term(last_applied)?;
        let conf_state = self.raft_group.raft.prs().conf().to_conf_state();

        // Create raft snapshot
        let mut raft_snapshot = Snapshot::default();
        raft_snapshot.set_data(snapshot_data.into());

        let mut snapshot_metadata = SnapshotMetadata::default();
        snapshot_metadata.set_index(last_applied);
        snapshot_metadata.set_term(term);
        snapshot_metadata.set_conf_state(conf_state);
        raft_snapshot.set_metadata(snapshot_metadata);

        // Apply to storage (using our custom method that preserves data)
        store.apply_snapshot_with_data(raft_snapshot)?;

        slog::info!(self.logger, "Created automatic snapshot";
            "snapshot_index" => last_applied,
            "data_size" => data_size
        );

        // Compact logs if configured
        if self.snapshot_config.auto_compact {
            self.compact_log(last_applied)?;
        }

        Ok(())
    }

    /// Compact log up to the given index
    fn compact_log(&mut self, up_to_index: u64) -> Result<(), Box<dyn std::error::Error>> {
        // Keep some entries before snapshot for safety
        let compact_index = up_to_index.saturating_sub(self.snapshot_config.entries_before_snapshot);

        if compact_index > 0 {
            let store = self.raft_group.raft.raft_log.store.clone();
            store.wl().compact(compact_index)?;

            slog::info!(self.logger, "Compacted log";
                "compact_index" => compact_index,
                "entries_removed" => compact_index
            );
        }

        Ok(())
    }

    /// Check and create snapshot if needed (generic implementation)
    pub fn check_and_create_snapshot(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Check if snapshot is needed
        if self.should_create_snapshot()? {
            self.trigger_snapshot_creation()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_config_default() {
        let config = SnapshotConfig::default();
        assert_eq!(config.snapshot_interval, 1000);
        assert_eq!(config.max_log_size, 10000);
        assert_eq!(config.auto_compact, true);
        assert_eq!(config.entries_before_snapshot, 100);
    }

    #[test]
    fn test_snapshot_config_custom() {
        let config = SnapshotConfig {
            snapshot_interval: 500,
            max_log_size: 5000,
            auto_compact: false,
            entries_before_snapshot: 50,
        };

        assert_eq!(config.snapshot_interval, 500);
        assert_eq!(config.max_log_size, 5000);
        assert_eq!(config.auto_compact, false);
        assert_eq!(config.entries_before_snapshot, 50);
    }
}