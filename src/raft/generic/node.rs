use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, mpsc};
use raft::{prelude::*, storage::MemStorage};
use slog::{Drain, Logger};
use protobuf::Message as PbMessage;
use crate::raft::generic::message::{Message, CommandExecutor, CommandWrapper};

pub type SyncCallback = tokio::sync::oneshot::Sender<Result<(), Box<dyn std::error::Error + Send + Sync>>>;

pub struct RaftNode<E: CommandExecutor> {
    raft_group: RawNode<MemStorage>,
    #[allow(dead_code)] // Reserved for future use
    storage: Arc<RwLock<HashMap<String, Vec<u8>>>>,
    node_id: u64,
    logger: Logger,

    // For multi-node communication
    mailbox: mpsc::UnboundedReceiver<Message<E::Command>>,
    peers: HashMap<u64, mpsc::UnboundedSender<Message<E::Command>>>,

    // Sync command tracking (command_id -> completion callback)
    sync_commands: HashMap<u64, SyncCallback>,
    next_command_id: AtomicU64,

    // Command executor for applying committed commands
    executor: Arc<E>,
}

impl<E: CommandExecutor + 'static> RaftNode<E> {
    pub fn new_single_node(
        node_id: u64,
        mailbox: mpsc::UnboundedReceiver<Message<E::Command>>,
        peers: HashMap<u64, mpsc::UnboundedSender<Message<E::Command>>>,
        executor: Arc<E>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let config = Config {
            id: node_id,
            election_tick: 10,
            heartbeat_tick: 3,
            applied: 0,
            max_size_per_msg: 1024 * 1024 * 1024,
            max_inflight_msgs: 256,
            check_quorum: false, // Important: disable for single node
            pre_vote: false, // Disable pre-vote for single node
            ..Default::default()
        };

        let storage = MemStorage::new();

        // For single-node cluster, initialize with ourselves as voter
        let mut initial_state = ConfState::default();
        initial_state.voters.push(node_id);
        storage.wl().set_conf_state(initial_state);

        // Create logger for this node
        let decorator = slog_term::TermDecorator::new().build();
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        let logger = slog::Logger::root(drain, slog::o!("node_id" => node_id));

        let raft_group = RawNode::new(&config, storage, &logger)?;

        Ok(RaftNode {
            raft_group,
            storage: Arc::new(RwLock::new(HashMap::new())),
            node_id,
            logger,
            mailbox,
            peers,
            sync_commands: HashMap::new(),
            next_command_id: AtomicU64::new(1),
            executor,
        })
    }

    pub fn new(
        node_id: u64,
        mailbox: mpsc::UnboundedReceiver<Message<E::Command>>,
        peers: HashMap<u64, mpsc::UnboundedSender<Message<E::Command>>>,
        executor: Arc<E>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let config = Config {
            id: node_id,
            election_tick: 10,
            heartbeat_tick: 3,
            applied: 0,
            max_size_per_msg: 1024 * 1024 * 1024,
            max_inflight_msgs: 256,
            check_quorum: peers.len() > 0, // Disable quorum check for single-node
            ..Default::default()
        };

        let storage = MemStorage::new();

        // If this is a single-node cluster, set up the initial configuration
        if peers.is_empty() {
            let mut initial_state = ConfState::default();
            initial_state.voters.push(node_id);
            storage.wl().set_conf_state(initial_state);
        }

        // Create logger for this node
        let decorator = slog_term::TermDecorator::new().build();
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        let logger = slog::Logger::root(drain, slog::o!("node_id" => node_id));

        let raft_group = RawNode::new(&config, storage, &logger)?;

        Ok(RaftNode {
            raft_group,
            storage: Arc::new(RwLock::new(HashMap::new())),
            node_id,
            logger,
            mailbox,
            peers,
            sync_commands: HashMap::new(),
            next_command_id: AtomicU64::new(1),
            executor,
        })
    }

    pub fn propose_command(&mut self, command: E::Command, sync_id: Option<u64>) -> Result<(), Box<dyn std::error::Error>> {
        let wrapped_command = CommandWrapper {
            id: sync_id,
            command,
        };

        let data = serde_json::to_vec(&wrapped_command)?;
        self.raft_group.propose(vec![], data)?;
        Ok(())
    }

    pub fn propose_conf_change_v2(&mut self, conf_change: ConfChangeV2) -> Result<(), Box<dyn std::error::Error>> {
        // Note: raft-rs 0.7.0 may need legacy propose_conf_change, but we'll prepare for v2
        // For now, convert to legacy for compatibility
        if let Some(change) = conf_change.changes.first() {
            let mut legacy_change = ConfChange::default();
            legacy_change.change_type = change.change_type;
            legacy_change.node_id = change.node_id;
            legacy_change.context = bytes::Bytes::new();

            self.raft_group.propose_conf_change(vec![], legacy_change)?;
        }

        Ok(())
    }

    pub fn step(&mut self, msg: raft::prelude::Message) -> Result<(), Box<dyn std::error::Error>> {
        self.raft_group.step(msg)?;
        Ok(())
    }

    pub fn tick(&mut self) {
        self.raft_group.tick();
    }

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
            store.wl().apply_snapshot(ready.snapshot().clone())?;
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

        // Handle soft state changes (informational only)
        if let Some(_ss) = ready.ss() {
            // Soft state changes don't need special handling
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
            if let Some(sender) = self.peers.get(&msg.to) {
                let msg_to = msg.to;
                let raft_msg = Message::Raft(msg);
                if let Err(e) = sender.send(raft_msg) {
                    slog::warn!(self.logger, "Failed to send message"; "to" => msg_to, "error" => %e);
                }
            } else {
                slog::warn!(self.logger, "No sender for peer"; "peer_id" => msg.to);
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
                    // All commands are now wrapped with CommandWrapper
                    if let Ok(wrapped_command) = serde_json::from_slice::<CommandWrapper<E::Command>>(&entry.data) {
                        let result = self.apply_command(&wrapped_command.command);

                        // Notify the waiting sync callback if there's a sync ID
                        if let Some(sync_id) = wrapped_command.id {
                            if let Some(callback) = self.sync_commands.remove(&sync_id) {
                                let callback_result = match &result {
                                    Ok(_) => Ok(()),
                                    Err(e) => {
                                        let err: Box<dyn std::error::Error + Send + Sync> = format!("{}", e).into();
                                        Err(err)
                                    }
                                };
                                let _ = callback.send(callback_result);
                            }
                        }

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
                        self.apply_conf_change_v2(&conf_change_v2)?;
                    } else {
                        slog::error!(self.logger, "Failed to parse conf change v2 from entry data");
                    }
                    last_applied_index = Some(entry.index);
                },
            }
        }
        Ok(last_applied_index)
    }

    fn apply_command(&mut self, command: &E::Command) -> Result<(), Box<dyn std::error::Error>> {
        // Use the executor to apply the command
        self.executor.apply(command, &self.logger)
    }


    fn apply_conf_change_v2(&mut self, conf_change: &ConfChangeV2) -> Result<(), Box<dyn std::error::Error>> {
        slog::info!(self.logger, "Applying ConfChangeV2"; "transition" => ?conf_change.transition);

        // Process individual changes in the ConfChangeV2
        for change in conf_change.changes.iter() {
            let node_id = change.node_id;
            match change.change_type {
                ConfChangeType::AddNode => {
                    slog::info!(self.logger, "Adding node to cluster (v2)"; "node_id" => node_id);
                },
                ConfChangeType::RemoveNode => {
                    slog::info!(self.logger, "Removing node from cluster (v2)"; "node_id" => node_id);
                },
                ConfChangeType::AddLearnerNode => {
                    slog::info!(self.logger, "Adding learner node to cluster (v2)"; "node_id" => node_id);
                },
            }
        }

        // Note: raft-rs 0.7.0 may not have apply_conf_change_v2, using legacy method
        // Convert ConfChangeV2 to legacy ConfChange for compatibility
        if let Some(change) = conf_change.changes.first() {
            let mut legacy_change = ConfChange::default();
            legacy_change.change_type = change.change_type;
            legacy_change.node_id = change.node_id;
            legacy_change.context = bytes::Bytes::new(); // Empty context for compatibility

            let cs = self.raft_group.apply_conf_change(&legacy_change)?;
            slog::info!(self.logger, "ConfChangeV2 applied via legacy method"; "new_conf" => ?cs);
        }

        Ok(())
    }

    pub fn is_leader(&self) -> bool {
        self.raft_group.raft.state == raft::StateRole::Leader
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
                        Some(Message::Propose { command, callback, sync_callback, .. }) => {
                            let sync_id = if sync_callback.is_some() {
                                Some(self.next_command_id.fetch_add(1, Ordering::SeqCst))
                            } else {
                                None
                            };

                            // Store sync callback if provided
                            if let (Some(sync_cb), Some(id)) = (sync_callback, sync_id) {
                                self.sync_commands.insert(id, sync_cb);
                            }

                            match self.propose_command(command, sync_id) {
                                Ok(_) => {
                                    if let Some(cb) = callback {
                                        let _ = cb.send(true);
                                    }
                                },
                                Err(e) => {
                                    slog::error!(self.logger, "Failed to propose command"; "error" => %e);
                                    if let Some(cb) = callback {
                                        let _ = cb.send(false);
                                    }
                                    // Also notify sync callback of failure if it was set
                                    if let Some(id) = sync_id {
                                        if let Some(sync_cb) = self.sync_commands.remove(&id) {
                                            let err_msg = format!("Failed to propose command: {}", e);
                                            let err: Box<dyn std::error::Error + Send + Sync> = err_msg.into();
                                            let _ = sync_cb.send(Err(err));
                                        }
                                    }
                                }
                            }
                        },
                        Some(Message::Raft(raft_msg)) => {
                            if let Err(e) = self.step(raft_msg) {
                                slog::error!(self.logger, "Failed to step raft message"; "error" => %e);
                            }
                        },
                        Some(Message::ConfChangeV2 { change, callback, .. }) => {
                            match self.propose_conf_change_v2(change) {
                                Ok(_) => {
                                    if let Some(cb) = callback {
                                        let _ = cb.send(true);
                                    }
                                },
                                Err(e) => {
                                    slog::error!(self.logger, "Failed to propose conf change v2"; "error" => %e);
                                    if let Some(cb) = callback {
                                        let _ = cb.send(false);
                                    }
                                }
                            }
                        },
                        Some(Message::Campaign { callback }) => {
                            match self.campaign() {
                                Ok(_) => {
                                    if let Some(cb) = callback {
                                        let _ = cb.send(true);
                                    }
                                },
                                Err(e) => {
                                    slog::error!(self.logger, "Failed to start campaign"; "error" => %e);
                                    if let Some(cb) = callback {
                                        let _ = cb.send(false);
                                    }
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
        }

        Ok(())
    }
}