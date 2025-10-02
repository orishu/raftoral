use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::sync::{mpsc, broadcast};
use raft::prelude::*;
use crate::raft::generic::message::{Message, CommandExecutor};
use crate::raft::generic::node::RaftNode;

/// Role change event for leadership notifications
#[derive(Debug, Clone, PartialEq)]
pub enum RoleChange {
    BecameLeader(u64),    // node_id that became leader
    BecameFollower(u64),  // node_id that became follower
    BecameCandidate(u64), // node_id that became candidate
}

#[derive(Clone)]
pub struct RaftCluster<E: CommandExecutor> {
    node_senders: HashMap<u64, mpsc::UnboundedSender<Message<E::Command>>>,
    node_count: usize,
    // Role change notifications
    role_change_tx: broadcast::Sender<RoleChange>,
    // Command ID counter for precise tracking
    #[allow(dead_code)] // Reserved for future command tracking
    next_command_id: Arc<AtomicU64>,
    // Command executor for applying commands
    pub executor: Arc<E>,
}

impl<E: CommandExecutor + 'static> RaftCluster<E> {
    pub async fn new_single_node(node_id: u64, executor: E) -> Result<Self, Box<dyn std::error::Error>> {
        let mut node_senders = HashMap::new();
        let (sender, receiver) = mpsc::unbounded_channel();

        // For single node, it doesn't need peers but we still use the same structure
        let peers = HashMap::new();

        node_senders.insert(node_id, sender.clone());

        // Create role change broadcast channel
        let (role_change_tx, _role_change_rx) = broadcast::channel(100);

        let executor_arc = Arc::new(executor);
        let cluster = RaftCluster {
            node_senders,
            node_count: 1,
            role_change_tx: role_change_tx.clone(),
            next_command_id: Arc::new(AtomicU64::new(1)),
            executor: executor_arc.clone(),
        };

        // Create and run the single node using the multi-node constructor
        tokio::spawn(async move {
            let mut node = match RaftNode::<E>::new(node_id, receiver, peers, executor_arc.clone()) {
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
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // For single node, manually trigger campaign to become leader
        cluster.campaign().await?;

        Ok(cluster)
    }

    pub async fn new_multi_node(node_ids: Vec<u64>, executor: E) -> Result<Self, Box<dyn std::error::Error>> {
        let mut node_senders = HashMap::new();
        let mut node_receivers = HashMap::new();

        // Create communication channels for all nodes
        for &node_id in &node_ids {
            let (sender, receiver) = mpsc::unbounded_channel();
            node_senders.insert(node_id, sender);
            node_receivers.insert(node_id, receiver);
        }

        // Create role change broadcast channel
        let (role_change_tx, _role_change_rx) = broadcast::channel(100);

        let executor_arc = Arc::new(executor);
        let cluster = RaftCluster {
            node_senders: node_senders.clone(),
            node_count: node_ids.len(),
            role_change_tx: role_change_tx.clone(),
            next_command_id: Arc::new(AtomicU64::new(1)),
            executor: executor_arc.clone(),
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

            let executor_clone = executor_arc.clone();
            tokio::spawn(async move {
                let mut node = match RaftNode::<E>::new(node_id, receiver, peers, executor_clone) {
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


    /// Generic method to propose any command using the CommandExecutor pattern
    pub async fn propose_command(&self, command: E::Command) -> Result<bool, Box<dyn std::error::Error>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        // Send to the first available node (in a real implementation, you'd route to the leader)
        if let Some(node_sender) = self.node_senders.values().next() {
            node_sender.send(Message::Propose {
                id: 0,
                callback: Some(sender),
                sync_callback: None,
                command,
            })?;

            let result = receiver.await?;
            Ok(result)
        } else {
            Err("No nodes available in cluster".into())
        }
    }

    /// Propose a command and wait until it's applied to the state machine
    /// Uses precise tracking with unique command IDs to wait for exact completion
    pub async fn propose_and_sync(&self, command: E::Command) -> Result<(), Box<dyn std::error::Error>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        // Send to the first available node (in a real implementation, you'd route to the leader)
        if let Some(node_sender) = self.node_senders.values().next() {
            node_sender.send(Message::Propose {
                id: 0,
                callback: None,
                sync_callback: Some(sender),
                command,
            })?;

            // Wait for the specific command to be applied
            match receiver.await {
                Ok(result) => result.map_err(|e| -> Box<dyn std::error::Error> { format!("Command application error: {}", e).into() }),
                Err(e) => Err(format!("Command callback error: {}", e).into()),
            }
        } else {
            Err("No nodes available in cluster".into())
        }
    }

    /// Check if this cluster is currently the leader
    pub async fn is_leader(&self) -> bool {
        // For single-node clusters, we're always the leader
        // TODO: In multi-node implementation, query actual Raft leadership status
        self.node_count == 1
    }

    /// Subscribe to role change notifications
    pub fn subscribe_role_changes(&self) -> broadcast::Receiver<RoleChange> {
        self.role_change_tx.subscribe()
    }

    /// Notify about role change (internal use)
    pub fn notify_role_change(&self, role_change: RoleChange) {
        let _ = self.role_change_tx.send(role_change);
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

    pub async fn campaign(&self) -> Result<bool, Box<dyn std::error::Error>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        if let Some(node_sender) = self.node_senders.values().next() {
            node_sender.send(Message::Campaign {
                callback: Some(sender),
            })?;

            let result = receiver.await?;
            Ok(result)
        } else {
            Err("No nodes available in cluster".into())
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    // Shared test state with key prefixes for test isolation
    static TEST_STATE: std::sync::OnceLock<Arc<Mutex<HashMap<String, String>>>> = std::sync::OnceLock::new();

    fn get_test_state() -> Arc<Mutex<HashMap<String, String>>> {
        TEST_STATE.get_or_init(|| Arc::new(Mutex::new(HashMap::new()))).clone()
    }

    fn clear_test_state_for_prefix(prefix: &str) {
        let state = get_test_state();
        let mut map = state.lock().unwrap();
        map.retain(|key, _| !key.starts_with(prefix));
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    enum TestCommand {
        SetValue { key: String, value: String },
        DeleteKey { key: String },
        Noop,
    }

    struct TestCommandExecutor;

    impl CommandExecutor for TestCommandExecutor {
        type Command = TestCommand;

        fn apply(&self, command: &Self::Command, _logger: &slog::Logger) -> Result<(), Box<dyn std::error::Error>> {
            let state = get_test_state();
            match command {
                TestCommand::SetValue { key, value } => {
                    let mut map = state.lock().unwrap();
                    map.insert(key.clone(), value.clone());
                },
                TestCommand::DeleteKey { key } => {
                    let mut map = state.lock().unwrap();
                    map.remove(key);
                },
                TestCommand::Noop => {
                    // No-op command does nothing
                },
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_single_node_cluster_creation() {
        let executor = TestCommandExecutor;
        let cluster = RaftCluster::new_single_node(1, executor).await;
        assert!(cluster.is_ok());

        let cluster = cluster.unwrap();
        assert_eq!(cluster.node_count(), 1);
        assert_eq!(cluster.get_node_ids(), vec![1]);
    }

    #[tokio::test]
    async fn test_single_node_propose_and_apply() {
        const TEST_PREFIX: &str = "test1_";

        // Clear any previous test state for this prefix
        clear_test_state_for_prefix(TEST_PREFIX);

        let executor = TestCommandExecutor;
        let cluster = RaftCluster::new_single_node(1, executor).await
            .expect("Failed to create single node cluster");

        // Wait for leadership establishment
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let state = get_test_state();

        // Test SetValue command
        let set_command = TestCommand::SetValue {
            key: format!("{}test_key", TEST_PREFIX),
            value: "test_value".to_string(),
        };

        let result = cluster.propose_command(set_command).await;
        assert!(result.is_ok(), "SetValue command proposal should succeed");
        assert!(result.unwrap(), "SetValue command should be successfully applied");

        // Wait for command to be applied
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Verify the state was modified
        {
            let map = state.lock().unwrap();
            assert_eq!(map.get(&format!("{}test_key", TEST_PREFIX)), Some(&"test_value".to_string()),
                      "SetValue command should have updated the state");
        }

        // Test another SetValue command
        let set_command2 = TestCommand::SetValue {
            key: format!("{}another_key", TEST_PREFIX),
            value: "another_value".to_string(),
        };

        let result = cluster.propose_command(set_command2).await;
        assert!(result.is_ok(), "Second SetValue command proposal should succeed");
        assert!(result.unwrap(), "Second SetValue command should be successfully applied");

        // Wait for command to be applied
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Verify both values exist (count only our test's keys)
        {
            let map = state.lock().unwrap();
            assert_eq!(map.get(&format!("{}test_key", TEST_PREFIX)), Some(&"test_value".to_string()));
            assert_eq!(map.get(&format!("{}another_key", TEST_PREFIX)), Some(&"another_value".to_string()));

            let test_keys: Vec<_> = map.keys().filter(|k| k.starts_with(TEST_PREFIX)).collect();
            assert_eq!(test_keys.len(), 2, "State should contain exactly 2 entries for this test");
        }

        // Test DeleteKey command
        let delete_command = TestCommand::DeleteKey {
            key: format!("{}test_key", TEST_PREFIX),
        };

        let result = cluster.propose_command(delete_command).await;
        assert!(result.is_ok(), "DeleteKey command proposal should succeed");
        assert!(result.unwrap(), "DeleteKey command should be successfully applied");

        // Wait for command to be applied
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Verify the key was deleted
        {
            let map = state.lock().unwrap();
            assert_eq!(map.get(&format!("{}test_key", TEST_PREFIX)), None, "test_key should have been deleted");
            assert_eq!(map.get(&format!("{}another_key", TEST_PREFIX)), Some(&"another_value".to_string()),
                      "another_key should still exist");

            let test_keys: Vec<_> = map.keys().filter(|k| k.starts_with(TEST_PREFIX)).collect();
            assert_eq!(test_keys.len(), 1, "State should contain exactly 1 entry for this test after deletion");
        }

        // Test Noop command
        let noop_command = TestCommand::Noop;
        let result = cluster.propose_command(noop_command).await;
        assert!(result.is_ok(), "Noop command proposal should succeed");
        assert!(result.unwrap(), "Noop command should be successfully applied");

        // Wait for command to be applied
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Verify state unchanged by Noop
        {
            let map = state.lock().unwrap();
            let test_keys: Vec<_> = map.keys().filter(|k| k.starts_with(TEST_PREFIX)).collect();
            assert_eq!(test_keys.len(), 1, "Noop should not change state");
            assert_eq!(map.get(&format!("{}another_key", TEST_PREFIX)), Some(&"another_value".to_string()));
        }
    }

    #[tokio::test]
    async fn test_multiple_commands_sequential() {
        const TEST_PREFIX: &str = "test2_";

        // Clear any previous test state for this prefix
        clear_test_state_for_prefix(TEST_PREFIX);

        let executor = TestCommandExecutor;
        let cluster = RaftCluster::new_single_node(1, executor).await
            .expect("Failed to create single node cluster");

        // Wait for leadership establishment
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let state = get_test_state();

        let commands = vec![
            TestCommand::SetValue {
                key: format!("{}key1", TEST_PREFIX),
                value: "value1".to_string(),
            },
            TestCommand::SetValue {
                key: format!("{}key2", TEST_PREFIX),
                value: "value2".to_string(),
            },
            TestCommand::SetValue {
                key: format!("{}key1", TEST_PREFIX),
                value: "updated_value1".to_string(),
            },
            TestCommand::DeleteKey {
                key: format!("{}key2", TEST_PREFIX),
            },
        ];

        for command in commands {
            let result = cluster.propose_command(command).await;
            assert!(result.is_ok(), "Command proposal should succeed");
            assert!(result.unwrap(), "Command should be successfully applied");
        }

        // Wait for all commands to be applied
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Verify final state
        {
            let map = state.lock().unwrap();
            assert_eq!(map.get(&format!("{}key1", TEST_PREFIX)), Some(&"updated_value1".to_string()),
                      "key1 should have updated value");
            assert_eq!(map.get(&format!("{}key2", TEST_PREFIX)), None, "key2 should have been deleted");

            let test_keys: Vec<_> = map.keys().filter(|k| k.starts_with(TEST_PREFIX)).collect();
            assert_eq!(test_keys.len(), 1, "State should contain exactly 1 entry for this test");
        }
    }
}