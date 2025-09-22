use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, mpsc};
use raft::{prelude::*, storage::MemStorage};
use slog::{Drain, Logger};
use protobuf::Message as PbMessage;
use crate::raft::generic::message::{Message, RaftCommandType};

pub type ProposeCallback = tokio::sync::oneshot::Sender<bool>;

pub struct RaftNode<C: RaftCommandType> {
    raft_group: RawNode<MemStorage>,
    storage: Arc<RwLock<HashMap<String, Vec<u8>>>>,
    node_id: u64,
    logger: Logger,

    // For multi-node communication
    mailbox: mpsc::UnboundedReceiver<Message<C>>,
    peers: HashMap<u64, mpsc::UnboundedSender<Message<C>>>,

    // Proposal tracking
    next_proposal_id: u8,
    proposals: HashMap<u8, ProposeCallback>,
}

impl<C: RaftCommandType + 'static> RaftNode<C> {
    pub fn new_single_node(
        node_id: u64,
        mailbox: mpsc::UnboundedReceiver<Message<C>>,
        peers: HashMap<u64, mpsc::UnboundedSender<Message<C>>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let config = Config {
            id: node_id,
            election_tick: 10,
            heartbeat_tick: 3,
            applied: 0,
            max_size_per_msg: 1024 * 1024 * 1024,
            max_inflight_msgs: 256,
            check_quorum: false, // Important: disable for single node
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
            next_proposal_id: 0,
            proposals: HashMap::new(),
        })
    }

    pub fn new(
        node_id: u64,
        mailbox: mpsc::UnboundedReceiver<Message<C>>,
        peers: HashMap<u64, mpsc::UnboundedSender<Message<C>>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let config = Config {
            id: node_id,
            election_tick: 10,
            heartbeat_tick: 3,
            applied: 0,
            max_size_per_msg: 1024 * 1024 * 1024,
            max_inflight_msgs: 256,
            check_quorum: true,
            ..Default::default()
        };

        let storage = MemStorage::new();

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
            next_proposal_id: 0,
            proposals: HashMap::new(),
        })
    }

    pub fn propose_command(&mut self, command: C) -> Result<u8, Box<dyn std::error::Error>> {
        let data = serde_json::to_vec(&command)?;
        let proposal_id = self.next_proposal_id;
        self.next_proposal_id = self.next_proposal_id.wrapping_add(1);

        self.raft_group.propose(vec![], data)?;
        slog::info!(self.logger, "Proposed command"; "proposal_id" => proposal_id);

        Ok(proposal_id)
    }

    pub fn propose_conf_change_v2(&mut self, conf_change: ConfChangeV2) -> Result<u8, Box<dyn std::error::Error>> {
        let proposal_id = self.next_proposal_id;
        self.next_proposal_id = self.next_proposal_id.wrapping_add(1);

        // Note: raft-rs 0.7.0 may need legacy propose_conf_change, but we'll prepare for v2
        // For now, convert to legacy for compatibility
        if let Some(change) = conf_change.changes.first() {
            let mut legacy_change = ConfChange::default();
            legacy_change.change_type = change.change_type;
            legacy_change.node_id = change.node_id;
            legacy_change.context = bytes::Bytes::new();

            self.raft_group.propose_conf_change(vec![], legacy_change)?;
        }

        slog::info!(self.logger, "Proposed conf change v2"; "proposal_id" => proposal_id);

        Ok(proposal_id)
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

        let mut ready = self.raft_group.ready();

        // Handle hard state changes
        if let Some(hs) = ready.hs() {
            slog::debug!(self.logger, "Hard state changed";
                        "term" => hs.term, "vote" => hs.vote, "commit" => hs.commit);
        }

        // Handle soft state changes
        if let Some(ss) = ready.ss() {
            slog::info!(self.logger, "Soft state changed";
                       "leader_id" => ss.leader_id, "raft_state" => ?ss.raft_state);
        }

        // Send out messages to other nodes
        if !ready.messages().is_empty() {
            self.send_messages(ready.take_messages())?;
        }

        // Handle committed entries
        if !ready.committed_entries().is_empty() {
            self.handle_committed_entries(ready.committed_entries())?;
        }

        // Handle configuration changes through committed entries
        // In raft-rs, conf changes come through committed entries with EntryConfChange type
        // The apply_conf_change method will be called when we encounter EntryConfChange entries

        // Advance the Raft state machine
        let mut light_rd = self.raft_group.advance(ready);

        // Handle any additional messages from advance
        if !light_rd.messages().is_empty() {
            self.send_messages(light_rd.take_messages())?;
        }

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

    fn handle_committed_entries(&mut self, entries: &[Entry]) -> Result<(), Box<dyn std::error::Error>> {
        for entry in entries {
            if entry.data.is_empty() {
                // Empty entry, usually a leadership change
                continue;
            }

            match entry.entry_type {
                EntryType::EntryNormal => {
                    if let Ok(command) = serde_json::from_slice::<C>(&entry.data) {
                        self.apply_command(&command)?;
                    }
                },
                EntryType::EntryConfChange => {
                    slog::warn!(self.logger, "Legacy ConfChange not supported - use ConfChangeV2"; "index" => entry.index);
                    // We only support modern ConfChangeV2 for simplicity
                },
                EntryType::EntryConfChangeV2 => {
                    slog::info!(self.logger, "Processing conf change v2 entry"; "index" => entry.index);
                    if let Ok(conf_change_v2) = ConfChangeV2::parse_from_bytes(&entry.data) {
                        self.apply_conf_change_v2(&conf_change_v2)?;
                    } else {
                        slog::error!(self.logger, "Failed to parse conf change v2 from entry data");
                    }
                },
            }
        }
        Ok(())
    }

    fn apply_command(&mut self, command: &C) -> Result<(), Box<dyn std::error::Error>> {
        // Use the trait method to apply the command
        command.apply(&self.logger)
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
                        Some(Message::Propose { command, callback, .. }) => {
                            match self.propose_command(command) {
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