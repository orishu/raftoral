use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, mpsc};
use tokio::time;
use serde::{Deserialize, Serialize};
use raft::{prelude::*, storage::MemStorage};
use slog::{Drain, Logger};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlaceholderCommand {
    pub id: u64,
    pub data: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RaftCommand {
    WorkflowStart { workflow_id: u64, payload: Vec<u8> },
    WorkflowEnd { workflow_id: u64 },
    SetCheckpoint { workflow_id: u64, key: String, value: Vec<u8> },
    PlaceholderCmd(PlaceholderCommand),
}

pub enum Message {
    Propose {
        id: u8,
        callback: Option<tokio::sync::oneshot::Sender<bool>>,
        command: RaftCommand,
    },
    Raft(raft::prelude::Message),
}

pub struct RaftNode {
    raft_group: RawNode<MemStorage>,
    storage: Arc<RwLock<HashMap<String, Vec<u8>>>>,
    node_id: u64,
    next_proposal_id: u8,
    proposals: HashMap<u8, tokio::sync::oneshot::Sender<bool>>,
}

impl RaftNode {
    pub fn new(node_id: u64) -> Result<Self, Box<dyn std::error::Error>> {
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

        // For single-node cluster, we need to initialize with ourselves as a voter
        let mut initial_state = ConfState::default();
        initial_state.voters.push(node_id);
        storage.wl().set_conf_state(initial_state);

        let decorator = slog_term::TermDecorator::new().build();
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        let logger = slog::Logger::root(drain, slog::o!());

        let raft_group = RawNode::new(&config, storage, &logger)?;

        Ok(RaftNode {
            raft_group,
            storage: Arc::new(RwLock::new(HashMap::new())),
            node_id,
            next_proposal_id: 0,
            proposals: HashMap::new(),
        })
    }

    pub fn propose_command(&mut self, command: RaftCommand) -> Result<u8, Box<dyn std::error::Error>> {
        let data = serde_json::to_vec(&command)?;
        let proposal_id = self.next_proposal_id;
        self.next_proposal_id = self.next_proposal_id.wrapping_add(1);

        self.raft_group.propose(vec![], data)?;
        println!("Proposed command with ID: {}", proposal_id);

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

        // Handle committed entries
        if !ready.committed_entries().is_empty() {
            for entry in ready.committed_entries() {
                if !entry.data.is_empty() {
                    if let Err(e) = self.handle_committed_entry(entry) {
                        eprintln!("Failed to handle committed entry: {}", e);
                    }
                } else if entry.entry_type == EntryType::EntryConfChange {
                    // For conf changes
                    println!("Conf change entry at index {}", entry.index);
                }
            }
        }

        // Send out messages if any
        if !ready.messages().is_empty() {
            // For single-node cluster, we don't need to send messages to other nodes
            // but we need to process them internally if there are any self-messages
            for message in ready.take_messages() {
                if message.to == self.node_id {
                    println!("Processing self-message of type: {:?}", message.msg_type);
                }
            }
        }

        // Advance the Raft state machine
        let mut light_rd = self.raft_group.advance(ready);

        // Handle any additional messages from advance
        if !light_rd.messages().is_empty() {
            for message in light_rd.take_messages() {
                if message.to == self.node_id {
                    println!("Processing self-message from advance: {:?}", message.msg_type);
                }
            }
        }

        Ok(())
    }

    fn handle_committed_entry(&mut self, entry: &Entry) -> Result<(), Box<dyn std::error::Error>> {
        if entry.data.is_empty() {
            return Ok(());
        }

        let command: RaftCommand = serde_json::from_slice(&entry.data)?;
        println!("Applying committed entry at index {}", entry.index);

        match command {
            RaftCommand::SetCheckpoint { key, value, workflow_id } => {
                // For now, just print since we can't await in this context
                println!("Applied checkpoint for workflow {}: key={}, {} bytes", workflow_id, key, value.len());
            },
            RaftCommand::PlaceholderCmd(cmd) => {
                println!("Applied placeholder command: id={}, data={}", cmd.id, cmd.data);
            },
            RaftCommand::WorkflowStart { workflow_id, payload } => {
                println!("Started workflow: {} with payload size: {}", workflow_id, payload.len());
            },
            RaftCommand::WorkflowEnd { workflow_id } => {
                println!("Ended workflow: {}", workflow_id);
            },
        }

        Ok(())
    }

    pub fn is_leader(&self) -> bool {
        self.raft_group.raft.state == raft::StateRole::Leader
    }

    pub fn node_id(&self) -> u64 {
        self.node_id
    }
}

#[derive(Clone)]
pub struct RaftCluster {
    command_sender: mpsc::UnboundedSender<Message>,
}

impl RaftCluster {
    pub async fn new_single_node(node_id: u64) -> Result<Self, Box<dyn std::error::Error>> {
        let (command_sender, mut command_receiver) = mpsc::unbounded_channel();

        let cluster = RaftCluster {
            command_sender,
        };

        // Spawn the main Raft loop
        tokio::spawn(async move {
            let mut node = match RaftNode::new(node_id) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("Failed to create RaftNode: {}", e);
                    return;
                }
            };

            let mut tick_timer = time::interval(Duration::from_millis(100));
            let mut now = Instant::now();

            loop {
                tokio::select! {
                    // Handle incoming messages
                    msg = command_receiver.recv() => {
                        match msg {
                            Some(Message::Propose { command, callback, .. }) => {
                                match node.propose_command(command) {
                                    Ok(_) => {
                                        if let Some(cb) = callback {
                                            let _ = cb.send(true);
                                        }
                                    },
                                    Err(e) => {
                                        eprintln!("Failed to propose command: {}", e);
                                        if let Some(cb) = callback {
                                            let _ = cb.send(false);
                                        }
                                    }
                                }
                            },
                            Some(Message::Raft(raft_msg)) => {
                                if let Err(e) = node.step(raft_msg) {
                                    eprintln!("Failed to step raft message: {}", e);
                                }
                            },
                            None => break,
                        }
                    },

                    // Periodic tick
                    _ = tick_timer.tick() => {
                        let elapsed = now.elapsed();
                        if elapsed >= Duration::from_millis(100) {
                            node.tick();
                            now = Instant::now();
                        }
                    }
                }

                // Process ready state
                if let Err(e) = node.on_ready() {
                    eprintln!("Failed to process ready state: {}", e);
                }
            }
        });

        Ok(cluster)
    }

    pub async fn propose_placeholder_command(&self, cmd: PlaceholderCommand) -> Result<bool, Box<dyn std::error::Error>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        self.command_sender.send(Message::Propose {
            id: 0,
            callback: Some(sender),
            command: RaftCommand::PlaceholderCmd(cmd),
        })?;

        let result = receiver.await?;
        Ok(result)
    }

    pub async fn propose_workflow_start(&self, workflow_id: u64, payload: Vec<u8>) -> Result<bool, Box<dyn std::error::Error>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        self.command_sender.send(Message::Propose {
            id: 0,
            callback: Some(sender),
            command: RaftCommand::WorkflowStart { workflow_id, payload },
        })?;

        let result = receiver.await?;
        Ok(result)
    }

    pub async fn propose_workflow_end(&self, workflow_id: u64) -> Result<bool, Box<dyn std::error::Error>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        self.command_sender.send(Message::Propose {
            id: 0,
            callback: Some(sender),
            command: RaftCommand::WorkflowEnd { workflow_id },
        })?;

        let result = receiver.await?;
        Ok(result)
    }

    pub async fn propose_checkpoint(&self, workflow_id: u64, key: String, value: Vec<u8>) -> Result<bool, Box<dyn std::error::Error>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        self.command_sender.send(Message::Propose {
            id: 0,
            callback: Some(sender),
            command: RaftCommand::SetCheckpoint { workflow_id, key, value },
        })?;

        let result = receiver.await?;
        Ok(result)
    }
}