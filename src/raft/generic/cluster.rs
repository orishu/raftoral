use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Mutex};
use std::collections::HashMap;
use tokio::sync::{mpsc, broadcast, oneshot};
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
    transport: Arc<dyn crate::raft::generic::transport::TransportInteraction<Message<E::Command>>>,
    node_count: usize,
    // Role change notifications
    role_change_tx: broadcast::Sender<RoleChange>,
    // Command executor for applying commands
    pub executor: Arc<E>,
    // Track the current leader node ID (0 = unknown)
    leader_id: Arc<AtomicU64>,
    // The ID of this node
    node_id: u64,
    // The cluster ID for multi-cluster routing (0 = management, 1+ = execution)
    cluster_id: u64,
    // Cached Raft configuration (shared with RaftNode for direct access)
    cached_config: Arc<RwLock<Vec<u64>>>,
    // Cached full configuration state (voters and learners separately)
    cached_conf_state: Arc<RwLock<raft::prelude::ConfState>>,
    // Sync ID tracking: map of sync_id -> oneshot sender
    // Used for synchronous command/confchange operations
    sync_waiters: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<(), String>>>>>,
    // Local mailbox sender for sending messages to this node's RaftNode
    // Used for self-messaging instead of going through transport
    // Public so that InMemoryClusterTransport can register it
    pub local_sender: mpsc::UnboundedSender<Message<E::Command>>,
}

impl<E: CommandExecutor + 'static> RaftCluster<E> {
    /// Create a new Raft cluster node
    ///
    /// # Parameters
    /// * `node_id` - Unique identifier for this node
    /// * `cluster_id` - Cluster ID for multi-cluster routing (0 = management, 1+ = execution)
    /// * `transport` - Transport implementation for sending messages to peers
    /// * `executor` - Command executor for applying committed commands
    ///
    /// # Example
    /// ```ignore
    /// let transport = Arc::new(InMemoryClusterTransport::new(vec![1, 2, 3]));
    /// let executor = MyExecutor::default();
    /// let cluster = RaftCluster::new(1, 0, transport, executor).await?;
    /// ```
    pub async fn new(
        node_id: u64,
        cluster_id: u64,
        transport: Arc<dyn crate::raft::generic::transport::TransportInteraction<Message<E::Command>>>,
        executor: E,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Create mailbox channel for this node
        let (local_sender, mailbox_receiver) = mpsc::unbounded_channel();

        // Determine initial node count from discovered voters (peer nodes)
        // For InMemoryClusterTransport, this will be empty initially (no peers registered yet)
        // For GrpcClusterTransport, this comes from peer discovery
        // Note: transport only handles outgoing messages to OTHER nodes, so discovered_voters
        // represents peer nodes only. Total cluster size = discovered_voters + self (this node)
        let discovered_voters = transport.get_discovered_voters();
        let node_count = if discovered_voters.is_empty() {
            // No discovered voters means single-node bootstrap (only this node, no peers)
            1
        } else {
            // Multi-node: discovered peers + this node
            discovered_voters.len() + 1
        };

        // Special handling for InMemoryClusterTransport: register this node
        // This is necessary because InMemoryClusterTransport is an in-process testing transport
        // that routes messages via channels. Even though conceptually the transport is for
        // "outgoing to peers", the raft-rs library may internally send messages that need routing.
        // GrpcClusterTransport doesn't need this because it receives from the network.
        use std::any::Any;
        use crate::raft::generic::transport::InMemoryClusterTransport;
        if let Some(in_memory_transport) = (transport.as_ref() as &dyn Any).downcast_ref::<InMemoryClusterTransport<Message<E::Command>>>() {
            in_memory_transport.register_node(node_id, local_sender.clone());
        }

        // Create role change broadcast channel
        let (role_change_tx, _role_change_rx) = broadcast::channel(100);

        let executor_arc = Arc::new(executor);

        // Set node ID on executor (for ownership checks)
        executor_arc.set_node_id(node_id);

        // Determine if this is a single-node or multi-node setup
        let is_single_node = node_count == 1;

        let leader_id_arc = Arc::new(AtomicU64::new(0)); // 0 = unknown

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

        // Create cached_config - will be populated by RaftNode::new
        let cached_config_arc = Arc::new(RwLock::new(Vec::new()));
        let cached_conf_state_arc = Arc::new(RwLock::new(raft::prelude::ConfState::default()));

        // Create applied_entries channel for synchronous operations
        let (applied_tx, mut applied_rx) = mpsc::unbounded_channel::<(u64, Vec<u8>)>();

        // Create sync_waiters map
        let sync_waiters: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<(), String>>>>> = Arc::new(Mutex::new(HashMap::new()));

        // Spawn task to listen on applied_entries and trigger one-shot channels
        let sync_waiters_listener = sync_waiters.clone();
        tokio::spawn(async move {
            while let Some((_log_index, context)) = applied_rx.recv().await {
                // Extract sync_id from context (first 8 bytes as u64)
                if context.len() >= 8 {
                    let sync_id = u64::from_le_bytes([
                        context[0], context[1], context[2], context[3],
                        context[4], context[5], context[6], context[7],
                    ]);

                    // Check if we have a waiter for this sync_id
                    let mut waiters = sync_waiters_listener.lock().unwrap();
                    if let Some(sender) = waiters.remove(&sync_id) {
                        // Trigger the one-shot channel (ignore if receiver dropped)
                        let _ = sender.send(Ok(()));
                    }
                }
            }
        });

        // Create and spawn the Raft node with transport reference and applied_tx
        let role_tx_for_node = role_change_tx.clone();
        let transport_for_node = transport.clone();
        let node = RaftNode::<E>::new(
            node_id,
            cluster_id,
            mailbox_receiver,
            transport_for_node,
            executor_arc.clone(),
            role_tx_for_node,
            cached_config_arc.clone(),
            cached_conf_state_arc.clone(),
            applied_tx.clone(),
        )?;

        // Spawn the node
        tokio::spawn(async move {
            let mut n = node;
            if let Err(e) = n.run().await {
                eprintln!("Node {} exited with error: {}", node_id, e);
            }
        });

        // Create cluster with the cached_config shared with the node
        let cluster = RaftCluster {
            transport,
            node_count,
            role_change_tx: role_change_tx.clone(),
            executor: executor_arc.clone(),
            leader_id: leader_id_arc.clone(),
            node_id,
            cluster_id,
            cached_config: cached_config_arc,
            cached_conf_state: cached_conf_state_arc,
            sync_waiters,
            local_sender,
        };

        // Give the node time to initialize
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // For single node, manually trigger campaign to become leader
        if is_single_node {
            cluster.campaign().await?;
        }

        Ok(cluster)
    }

    /// Generic method to propose any command using the CommandExecutor pattern
    pub async fn propose_command(&self, command: E::Command) -> Result<bool, Box<dyn std::error::Error>> {
        // Send to this node's local Raft instance via local channel
        self.local_sender.send(Message::Propose {
            command,
            sync_id: None,
        }).map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?;

        Ok(true)
    }

    /// Helper method to perform a synchronous operation with timeout and cleanup
    /// This eliminates code duplication between propose_and_sync, add_node, and remove_node
    async fn sync_operation<F>(&self, create_message: F, timeout_secs: u64, operation_name: &str) -> Result<(), Box<dyn std::error::Error>>
    where
        F: Fn(u64) -> Message<E::Command>,
    {
        // Generate random sync_id
        let sync_id = rand::random::<u64>();

        // Create oneshot channel for completion notification
        let (tx, rx) = oneshot::channel();

        // Register the oneshot sender
        {
            let mut waiters = self.sync_waiters.lock().unwrap();
            waiters.insert(sync_id, tx);
        }

        // Try to send to leader (first try self if we're leader, then known leader, then scan)
        let mut sent = false;

        // Try 1: If we're the leader, send to self via local channel
        if self.leader_id.load(Ordering::SeqCst) == self.node_id {
            let message = create_message(sync_id);
            if self.local_sender.send(message).is_ok() {
                sent = true;
            }
        }

        // Try 2: If not sent yet and we know the leader, send to known leader
        if !sent {
            let known_leader_id = self.leader_id.load(Ordering::SeqCst);
            if known_leader_id != 0 {
                let message = create_message(sync_id);
                if self.transport.send_message_to_node(known_leader_id, message, self.cluster_id).is_ok() {
                    sent = true;
                }
            }
        }

        // Try 3: If still not sent, scan all nodes to find the leader
        if !sent {
            let node_ids = self.transport.list_nodes();
            for node_id in node_ids {
                if node_id == self.node_id {
                    continue; // Already tried self if we were leader
                }
                let message = create_message(sync_id);
                if self.transport.send_message_to_node(node_id, message, self.cluster_id).is_ok() {
                    sent = true;
                    break;
                }
            }
        }

        if !sent {
            // Failed to send - clean up the waiter
            let mut waiters = self.sync_waiters.lock().unwrap();
            waiters.remove(&sync_id);
            return Err(format!("Failed to send {} to any node", operation_name).into());
        }

        // Wait for the entry to be applied
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(e))) => Err(e.into()),
            Ok(Err(_)) => Err("Oneshot channel dropped before completion".into()),
            Err(_) => {
                // Timeout - clean up the waiter
                let mut waiters = self.sync_waiters.lock().unwrap();
                waiters.remove(&sync_id);
                Err(format!("Timeout waiting for {} to be applied", operation_name).into())
            }
        }
    }

    /// Propose a command and wait until it's applied
    /// Uses synchronous tracking via sync_id mechanism
    pub async fn propose_and_sync(&self, command: E::Command) -> Result<(), Box<dyn std::error::Error>> {
        self.sync_operation(
            |sync_id| Message::Propose {
                command: command.clone(),
                sync_id: Some(sync_id),
            },
            5,
            "command",
        ).await
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
        self.sync_operation(
            |sync_id| Message::AddNode {
                node_id,
                address: address.clone(),
                sync_id: Some(sync_id),
            },
            5,
            "add_node",
        ).await
    }

    /// Remove a node from the Raft cluster
    /// Automatically finds the leader and sends the request
    pub async fn remove_node(&self, node_id: u64) -> Result<(), Box<dyn std::error::Error>> {
        self.sync_operation(
            |sync_id| Message::RemoveNode {
                node_id,
                sync_id: Some(sync_id),
            },
            5,
            "remove_node",
        ).await
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

    /// Get the full Raft configuration state (voters and learners separately)
    pub fn get_conf_state(&self) -> raft::prelude::ConfState {
        self.cached_conf_state.read().unwrap().clone()
    }

    /// Get the ID of this node
    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    pub async fn campaign(&self) -> Result<bool, Box<dyn std::error::Error>> {
        self.local_sender.send(Message::Campaign)
            .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?;
        Ok(true)
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

    #[derive(Default, Clone, Debug)]
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
        use crate::raft::generic::transport::InMemoryClusterTransport;
        use crate::raft::generic::message::Message;

        let transport = Arc::new(InMemoryClusterTransport::<Message<TestCommand>>::new());
        let executor = TestCommandExecutor;
        let transport_ref: Arc<dyn crate::raft::generic::transport::TransportInteraction<Message<TestCommand>>> = transport.clone();
        let cluster = RaftCluster::new(1, 0, transport_ref, executor).await;
        assert!(cluster.is_ok());

        let cluster = Arc::new(cluster.unwrap());

        assert_eq!(cluster.node_count(), 1);

        // Wait for leadership and config to be established
        // Subscribe to role changes first
        let mut role_rx = cluster.subscribe_role_changes();

        // Wait for BecameLeader event with timeout
        let timeout = tokio::time::Duration::from_secs(2);
        match tokio::time::timeout(timeout, async {
            while let Ok(role_change) = role_rx.recv().await {
                if matches!(role_change, crate::raft::RoleChange::BecameLeader(_)) {
                    return;
                }
            }
        }).await {
            Ok(_) => {
                // Wait a bit more for config to be populated
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                assert_eq!(cluster.get_node_ids(), vec![1]);
            },
            Err(_) => {
                panic!("Timeout waiting for leadership");
            }
        }
    }

    #[tokio::test]
    async fn test_single_node_propose_and_apply() {
        use crate::raft::generic::transport::InMemoryClusterTransport;
        use crate::raft::generic::message::Message;
        const TEST_PREFIX: &str = "test1_";

        // Clear any previous test state for this prefix
        clear_test_state_for_prefix(TEST_PREFIX);

        let transport = Arc::new(InMemoryClusterTransport::<Message<TestCommand>>::new());
        let executor = TestCommandExecutor;
        let transport_ref: Arc<dyn crate::raft::generic::transport::TransportInteraction<Message<TestCommand>>> = transport.clone();
        let cluster = Arc::new(RaftCluster::new(1, 0, transport_ref, executor).await
            .expect("Failed to create single node cluster"));

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
        use crate::raft::generic::transport::InMemoryClusterTransport;
        use crate::raft::generic::message::Message;
        const TEST_PREFIX: &str = "test2_";

        // Clear any previous test state for this prefix
        clear_test_state_for_prefix(TEST_PREFIX);

        let transport = Arc::new(InMemoryClusterTransport::<Message<TestCommand>>::new());
        let executor = TestCommandExecutor;
        let transport_ref: Arc<dyn crate::raft::generic::transport::TransportInteraction<Message<TestCommand>>> = transport.clone();
        let cluster = Arc::new(RaftCluster::new(1, 0, transport_ref, executor).await
            .expect("Failed to create single node cluster"));

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