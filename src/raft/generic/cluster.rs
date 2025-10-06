use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::{mpsc, broadcast};
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
    // Track the current leader node ID (0 = unknown)
    leader_id: Arc<AtomicU64>,
    // The ID of this node
    node_id: u64,
    // Cached Raft configuration (shared with RaftNode for direct access)
    cached_config: Arc<RwLock<Vec<u64>>>,
}

impl<E: CommandExecutor + 'static> RaftCluster<E> {
    /// Create a new cluster node with pre-configured transport channels
    ///
    /// This is the primary constructor used by ClusterTransport implementations.
    /// It creates a single RaftCluster instance that represents one node in a
    /// potentially multi-node cluster.
    ///
    /// # Arguments
    /// * `node_id` - Unique identifier for this node
    /// * `receiver` - Channel to receive messages for this node
    /// * `node_senders` - Shared map of all node IDs to their message senders
    /// * `executor` - Command executor for applying committed commands
    /// * `transport_updater` - Optional transport updater for dynamic peer management
    pub async fn new_with_transport(
        node_id: u64,
        receiver: mpsc::UnboundedReceiver<Message<E::Command>>,
        node_senders: Arc<RwLock<HashMap<u64, mpsc::UnboundedSender<Message<E::Command>>>>>,
        executor: E,
        transport_updater: Option<Arc<dyn crate::raft::generic::transport::TransportUpdater>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Create role change broadcast channel
        let (role_change_tx, _role_change_rx) = broadcast::channel(100);

        let executor_arc = Arc::new(executor);

        // Set node ID on executor (for ownership checks)
        executor_arc.set_node_id(node_id);

        let node_count = node_senders.read().unwrap().len();

        // Determine if this is a single-node or multi-node setup
        let is_single_node = node_count == 1;

        let leader_id_arc = Arc::new(AtomicU64::new(0)); // 0 = unknown

        // Build a flat node_senders HashMap for RaftCluster (keeps the old structure for now)
        let node_senders_flat = node_senders.read().unwrap().clone();

        // Spawn a task to listen for role changes and update leader_id
        let leader_id_listener = leader_id_arc.clone();
        let role_change_tx_clone = role_change_tx.clone();
        tokio::spawn(async move {
            let mut role_rx = role_change_tx_clone.subscribe();
            while let Ok(role_change) = role_rx.recv().await {
                if let RoleChange::BecameLeader(id) = role_change {
                    leader_id_listener.store(id, Ordering::SeqCst);
                }
            }
        });

        // Create and spawn the Raft node with shared node_senders Arc
        let role_tx_for_node = role_change_tx.clone();
        let node_senders_for_node = node_senders.clone();
        let (node, cached_config_arc) = RaftNode::<E>::new(node_id, receiver, node_senders_for_node, executor_arc.clone(), role_tx_for_node, transport_updater)?;

        // Spawn the node
        tokio::spawn(async move {
            let mut n = node;
            if let Err(e) = n.run().await {
                eprintln!("Node {} exited with error: {}", node_id, e);
            }
        });

        // Create cluster with the cached_config from the node
        let cluster = RaftCluster {
            node_senders: node_senders_flat,
            node_count,
            role_change_tx: role_change_tx.clone(),
            next_command_id: Arc::new(AtomicU64::new(1)),
            executor: executor_arc.clone(),
            leader_id: leader_id_arc.clone(),
            node_id,
            cached_config: cached_config_arc,
        };

        // Give the node time to initialize
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // For single node, manually trigger campaign to become leader
        if is_single_node {
            cluster.campaign().await?;
        }

        Ok(cluster)
    }

    pub async fn new_single_node(node_id: u64, executor: E) -> Result<Self, Box<dyn std::error::Error>> {
        let mut node_senders_map = HashMap::new();
        let (sender, receiver) = mpsc::unbounded_channel();

        node_senders_map.insert(node_id, sender.clone());

        // Wrap in Arc<RwLock> for sharing with RaftNode
        let node_senders_shared = Arc::new(RwLock::new(node_senders_map.clone()));

        // Create role change broadcast channel
        let (role_change_tx, _role_change_rx) = broadcast::channel(100);

        let executor_arc = Arc::new(executor);

        let leader_id_arc = Arc::new(AtomicU64::new(node_id)); // Single node is always leader

        // Create the RaftNode - it returns (node, cached_config)
        let role_tx_clone = role_change_tx.clone();
        let (node, cached_config_arc) = RaftNode::<E>::new(node_id, receiver, node_senders_shared, executor_arc.clone(), role_tx_clone, None)?;

        // Build cluster with the cached_config from the node
        let cluster = RaftCluster {
            node_senders: node_senders_map,
            node_count: 1,
            role_change_tx: role_change_tx.clone(),
            next_command_id: Arc::new(AtomicU64::new(1)),
            executor: executor_arc.clone(),
            leader_id: leader_id_arc.clone(),
            node_id,
            cached_config: cached_config_arc,
        };

        // Spawn task to run the node
        tokio::spawn(async move {
            let mut n = node;
            if let Err(e) = n.run().await {
                eprintln!("Node {} exited with error: {}", node_id, e);
            }
        });

        // Give the node time to initialize
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // For single node, manually trigger campaign to become leader
        cluster.campaign().await?;

        Ok(cluster)
    }

    /// Generic method to propose any command using the CommandExecutor pattern
    pub async fn propose_command(&self, command: E::Command) -> Result<bool, Box<dyn std::error::Error>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        // Send to this node's local Raft instance, which will forward to leader if needed
        if let Some(node_sender) = self.node_senders.get(&self.node_id) {
            node_sender.send(Message::Propose {
                id: 0,
                callback: Some(sender),
                sync_callback: None,
                command,
            })?;

            let result = receiver.await?;
            Ok(result)
        } else {
            Err("Local node sender not found".into())
        }
    }

    /// Send a message to the leader node with retry logic
    /// This is the common implementation used by propose_and_sync, add_node, and remove_node
    async fn send_to_leader<R>(
        &self,
        create_message: impl Fn(tokio::sync::oneshot::Sender<R>) -> Message<E::Command>,
        max_retries: usize,
    ) -> Result<R, Box<dyn std::error::Error>>
    where
        R: Send + 'static,
    {
        let mut retry_delay_ms = 100;

        for retry in 0..max_retries {
            // If this node is the leader, try sending to self first
            if self.leader_id.load(Ordering::SeqCst) == self.node_id {
                if let Some(self_sender) = self.node_senders.get(&self.node_id) {
                    let (sender, receiver) = tokio::sync::oneshot::channel();
                    let message = create_message(sender);

                    if self_sender.send(message).is_ok() {
                        match tokio::time::timeout(std::time::Duration::from_millis(500), receiver).await {
                            Ok(Ok(result)) => return Ok(result),
                            _ => {
                                // Failed as leader
                            }
                        }
                    }
                }
            }

            // Check if we know the leader from role changes
            let known_leader_id = self.leader_id.load(Ordering::SeqCst);

            // Try sending to known leader
            if known_leader_id != 0 && known_leader_id != self.node_id {
                if let Some(leader_sender) = self.node_senders.get(&known_leader_id) {
                    let (sender, receiver) = tokio::sync::oneshot::channel();
                    let message = create_message(sender);

                    if leader_sender.send(message).is_ok() {
                        match tokio::time::timeout(std::time::Duration::from_millis(500), receiver).await {
                            Ok(Ok(result)) => return Ok(result),
                            _ => {
                                // Known leader failed, will try discovery
                            }
                        }
                    }
                }
            }

            // Leader unknown or failed - try scanning all nodes to find the leader
            // This is a fallback for when the role change subscription misses the initial election
            for (node_id, sender) in &self.node_senders {
                let (test_sender, test_receiver) = tokio::sync::oneshot::channel();
                let test_message = create_message(test_sender);

                if sender.send(test_message).is_ok() {
                    match tokio::time::timeout(std::time::Duration::from_millis(500), test_receiver).await {
                        Ok(Ok(result)) => {
                            // Found the leader! Update our cached leader_id
                            self.leader_id.store(*node_id, Ordering::SeqCst);
                            return Ok(result);
                        },
                        _ => {
                            // This node isn't the leader or failed, try next
                            continue;
                        }
                    }
                }
            }

            // Still no leader found - subscribe to role changes and wait for one to emerge
            let mut role_rx = self.role_change_tx.subscribe();

            // Wait for a leader to emerge
            let wait_result = tokio::time::timeout(
                std::time::Duration::from_millis(retry_delay_ms),
                async {
                    while let Ok(role_change) = role_rx.recv().await {
                        if let RoleChange::BecameLeader(leader_id) = role_change {
                            return Some(leader_id);
                        }
                    }
                    None
                }
            ).await;

            if let Ok(Some(leader_id)) = wait_result {
                // We learned who the leader is, try sending to them
                if let Some(leader_sender) = self.node_senders.get(&leader_id) {
                    let (sender, receiver) = tokio::sync::oneshot::channel();
                    let message = create_message(sender);

                    if leader_sender.send(message).is_ok() {
                        match tokio::time::timeout(std::time::Duration::from_millis(500), receiver).await {
                            Ok(Ok(result)) => return Ok(result),
                            _ => {
                                // Failed, will retry
                            }
                        }
                    }
                }
            }

            // Increase retry delay with exponential backoff
            if retry < max_retries - 1 {
                tokio::time::sleep(std::time::Duration::from_millis(retry_delay_ms)).await;
                retry_delay_ms = (retry_delay_ms * 2).min(2000);
            }
        }

        Err("Failed to send message to leader after retries".into())
    }

    /// Propose a command and wait until it's applied to the state machine
    /// Uses precise tracking with unique command IDs to wait for exact completion
    pub async fn propose_and_sync(&self, command: E::Command) -> Result<(), Box<dyn std::error::Error>> {
        let cmd = command; // Move ownership
        let _ = self.send_to_leader(
            move |sender| Message::Propose {
                id: 0,
                callback: None,
                sync_callback: Some(sender),
                command: cmd.clone(),
            },
            10, // max_retries
        ).await?;
        Ok(())
    }

    /// Check if this cluster is currently the leader
    pub async fn is_leader(&self) -> bool {
        self.leader_id.load(Ordering::SeqCst) == self.node_id
    }

    /// Subscribe to role change notifications
    pub fn subscribe_role_changes(&self) -> broadcast::Receiver<RoleChange> {
        self.role_change_tx.subscribe()
    }

    /// Notify about role change (internal use)
    pub fn notify_role_change(&self, role_change: RoleChange) {
        let _ = self.role_change_tx.send(role_change);
    }


    /// Add a node to the Raft cluster
    /// Automatically finds the leader and sends the request
    pub async fn add_node(&self, node_id: u64, address: String) -> Result<(), Box<dyn std::error::Error>> {
        match self.send_to_leader(
            move |sender| Message::AddNode {
                node_id,
                address: address.clone(),
                callback: Some(sender),
            },
            10, // max_retries
        ).await? {
            Ok(()) => Ok(()),
            Err(e) => Err(format!("{}", e).into()),
        }
    }

    /// Remove a node from the Raft cluster
    /// Automatically finds the leader and sends the request
    pub async fn remove_node(&self, node_id: u64) -> Result<(), Box<dyn std::error::Error>> {
        match self.send_to_leader(
            move |sender| Message::RemoveNode {
                node_id,
                callback: Some(sender),
            },
            10, // max_retries
        ).await? {
            Ok(()) => Ok(()),
            Err(e) => Err(format!("{}", e).into()),
        }
    }

    pub fn node_count(&self) -> usize {
        self.node_count
    }

    /// Get the current cluster node IDs from the cached Raft configuration.
    /// This uses a shared reference to the RaftNode's configuration, eliminating
    /// the need for message-based queries.
    pub fn get_node_ids(&self) -> Vec<u64> {
        self.cached_config.read().unwrap().clone()
    }

    /// Get the ID of this node
    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    pub async fn campaign(&self) -> Result<bool, Box<dyn std::error::Error>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();

        if let Some(node_sender) = self.node_senders.get(&self.node_id) {
            node_sender.send(Message::Campaign {
                callback: Some(sender),
            })?;

            let result = receiver.await?;
            Ok(result)
        } else {
            Err("Local node sender not found".into())
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

        fn apply_with_index(&self, command: &Self::Command, _logger: &slog::Logger, _log_index: u64) -> Result<(), Box<dyn std::error::Error>> {
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