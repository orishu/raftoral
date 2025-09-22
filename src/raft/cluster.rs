use std::collections::HashMap;
use tokio::sync::mpsc;
use raft::prelude::*;
use crate::raft::message::{Message, RaftCommand, PlaceholderCommand};
use crate::raft::node::RaftNode;

#[derive(Clone)]
pub struct RaftCluster {
    node_senders: HashMap<u64, mpsc::UnboundedSender<Message>>,
    node_count: usize,
}

impl RaftCluster {
    pub async fn new_single_node(node_id: u64) -> Result<Self, Box<dyn std::error::Error>> {
        let mut node_senders = HashMap::new();
        let (sender, receiver) = mpsc::unbounded_channel();

        // For single node, it doesn't need peers but we still use the same structure
        let peers = HashMap::new();

        node_senders.insert(node_id, sender.clone());

        let cluster = RaftCluster {
            node_senders,
            node_count: 1,
        };

        // Create and run the single node
        tokio::spawn(async move {
            let mut node = match RaftNode::new_single_node(node_id, receiver, peers) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("Failed to create RaftNode {}: {}", node_id, e);
                    return;
                }
            };

            if let Err(e) = node.run().await {
                eprintln!("Node {} exited with error: {}", node_id, e);
            }
        });

        // Give the node time to initialize
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        Ok(cluster)
    }

    pub async fn new_multi_node(node_ids: Vec<u64>) -> Result<Self, Box<dyn std::error::Error>> {
        let mut node_senders = HashMap::new();
        let mut node_receivers = HashMap::new();

        // Create communication channels for all nodes
        for &node_id in &node_ids {
            let (sender, receiver) = mpsc::unbounded_channel();
            node_senders.insert(node_id, sender);
            node_receivers.insert(node_id, receiver);
        }

        let cluster = RaftCluster {
            node_senders: node_senders.clone(),
            node_count: node_ids.len(),
        };

        // Create and start all nodes
        for node_id in node_ids {
            let receiver = node_receivers.remove(&node_id).unwrap();

            // Create peer senders (all other nodes)
            let mut peers = HashMap::new();
            for (&peer_id, sender) in &node_senders {
                if peer_id != node_id {
                    peers.insert(peer_id, sender.clone());
                }
            }

            tokio::spawn(async move {
                let mut node = match RaftNode::new(node_id, receiver, peers) {
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!("Failed to create RaftNode {}: {}", node_id, e);
                        return;
                    }
                };

                if let Err(e) = node.run().await {
                    eprintln!("Node {} exited with error: {}", node_id, e);
                }
            });
        }

        // Give nodes time to initialize
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        Ok(cluster)
    }

    pub async fn propose_placeholder_command(&self, cmd: PlaceholderCommand) -> Result<bool, Box<dyn std::error::Error>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        // Send to the first available node (in a real implementation, you'd route to the leader)
        if let Some(node_sender) = self.node_senders.values().next() {
            node_sender.send(Message::Propose {
                id: 0,
                callback: Some(sender),
                command: RaftCommand::PlaceholderCmd(cmd),
            })?;

            let result = receiver.await?;
            Ok(result)
        } else {
            Err("No nodes available in cluster".into())
        }
    }

    pub async fn propose_workflow_start(&self, workflow_id: u64, payload: Vec<u8>) -> Result<bool, Box<dyn std::error::Error>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        if let Some(node_sender) = self.node_senders.values().next() {
            node_sender.send(Message::Propose {
                id: 0,
                callback: Some(sender),
                command: RaftCommand::WorkflowStart { workflow_id, payload },
            })?;

            let result = receiver.await?;
            Ok(result)
        } else {
            Err("No nodes available in cluster".into())
        }
    }

    pub async fn propose_workflow_end(&self, workflow_id: u64) -> Result<bool, Box<dyn std::error::Error>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        if let Some(node_sender) = self.node_senders.values().next() {
            node_sender.send(Message::Propose {
                id: 0,
                callback: Some(sender),
                command: RaftCommand::WorkflowEnd { workflow_id },
            })?;

            let result = receiver.await?;
            Ok(result)
        } else {
            Err("No nodes available in cluster".into())
        }
    }

    pub async fn propose_checkpoint(&self, workflow_id: u64, key: String, value: Vec<u8>) -> Result<bool, Box<dyn std::error::Error>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        if let Some(node_sender) = self.node_senders.values().next() {
            node_sender.send(Message::Propose {
                id: 0,
                callback: Some(sender),
                command: RaftCommand::SetCheckpoint { workflow_id, key, value },
            })?;

            let result = receiver.await?;
            Ok(result)
        } else {
            Err("No nodes available in cluster".into())
        }
    }

    pub async fn add_node(&self, node_id: u64) -> Result<bool, Box<dyn std::error::Error>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        // Create modern ConfChangeV2
        let mut conf_change = ConfChangeV2::default();
        let mut change = ConfChangeSingle::default();
        change.change_type = ConfChangeType::AddNode.into();
        change.node_id = node_id;
        conf_change.changes.push(change);

        if let Some(node_sender) = self.node_senders.values().next() {
            node_sender.send(Message::ConfChangeV2 {
                id: 0,
                callback: Some(sender),
                change: conf_change,
            })?;

            let result = receiver.await?;
            Ok(result)
        } else {
            Err("No nodes available in cluster".into())
        }
    }

    pub async fn remove_node(&self, node_id: u64) -> Result<bool, Box<dyn std::error::Error>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        // Create modern ConfChangeV2
        let mut conf_change = ConfChangeV2::default();
        let mut change = ConfChangeSingle::default();
        change.change_type = ConfChangeType::RemoveNode.into();
        change.node_id = node_id;
        conf_change.changes.push(change);

        if let Some(node_sender) = self.node_senders.values().next() {
            node_sender.send(Message::ConfChangeV2 {
                id: 0,
                callback: Some(sender),
                change: conf_change,
            })?;

            let result = receiver.await?;
            Ok(result)
        } else {
            Err("No nodes available in cluster".into())
        }
    }

    pub fn node_count(&self) -> usize {
        self.node_count
    }

    pub fn get_node_ids(&self) -> Vec<u64> {
        self.node_senders.keys().copied().collect()
    }
}