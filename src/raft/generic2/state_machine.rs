//! State Machine (Layer 4)
//!
//! Provides a trait for state machines that apply commands and emit events.
//! Commands come from Raft consensus (committed entries), events go to the Event Bus.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// State Machine trait
///
/// Implementations apply commands (from Raft consensus) and emit events (to Event Bus).
/// The state machine is responsible for:
/// - Applying commands to update internal state
/// - Generating events that describe what changed
/// - Creating snapshots of state for Raft persistence
/// - Restoring from snapshots
pub trait StateMachine: Send + Sync {
    /// Command type (must be serializable for Raft)
    type Command: Serialize + for<'de> Deserialize<'de> + Send + Sync;

    /// Event type (emitted to Event Bus, must be cloneable for broadcasting)
    type Event: Clone + Send + Sync;

    /// Apply a command to the state machine
    ///
    /// Returns a list of events describing the changes that occurred.
    /// Events will be published to the Event Bus.
    fn apply(&mut self, command: &Self::Command) -> Result<Vec<Self::Event>, Box<dyn std::error::Error>>;

    /// Create a snapshot of the current state
    ///
    /// Returns serialized state bytes for Raft snapshot storage.
    fn snapshot(&self) -> Result<Vec<u8>, Box<dyn std::error::Error>>;

    /// Restore state from a snapshot
    ///
    /// Called when Raft needs to restore from a snapshot (e.g., new node joining).
    fn restore(&mut self, snapshot: &[u8]) -> Result<(), Box<dyn std::error::Error>>;

    /// Handle a failed follower node detection
    ///
    /// Called by RaftNode when it detects that a follower hasn't made progress
    /// within the configured timeout. State machines can override this to emit
    /// events for handling the failure (e.g., removing the node from sub-clusters).
    ///
    /// Default implementation returns no events (failure detection disabled).
    fn on_follower_failed(&mut self, _node_id: u64) -> Vec<Self::Event> {
        vec![] // Default: no failure detection events
    }
}

// ============================================================================
// Sample Implementation: Key-Value Store
// ============================================================================

/// Commands for the KV store state machine
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum KvCommand {
    Set { key: String, value: String },
    Delete { key: String },
}

/// Events emitted by the KV store
#[derive(Clone, Debug, PartialEq)]
pub enum KvEvent {
    KeySet { key: String, value: String },
    KeyDeleted { key: String },
}

/// Simple key-value store state machine for testing
#[derive(Debug)]
pub struct KvStateMachine {
    store: HashMap<String, String>,
}

impl KvStateMachine {
    /// Create a new KV state machine
    pub fn new() -> Self {
        Self {
            store: HashMap::new(),
        }
    }

    /// Get a value by key (for testing/queries)
    pub fn get(&self, key: &str) -> Option<&String> {
        self.store.get(key)
    }

    /// Get all keys (for testing)
    pub fn keys(&self) -> Vec<String> {
        self.store.keys().cloned().collect()
    }
}

impl Default for KvStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl StateMachine for KvStateMachine {
    type Command = KvCommand;
    type Event = KvEvent;

    fn apply(&mut self, command: &Self::Command) -> Result<Vec<Self::Event>, Box<dyn std::error::Error>> {
        match command {
            KvCommand::Set { key, value } => {
                self.store.insert(key.clone(), value.clone());
                Ok(vec![KvEvent::KeySet {
                    key: key.clone(),
                    value: value.clone(),
                }])
            }
            KvCommand::Delete { key } => {
                self.store.remove(key);
                Ok(vec![KvEvent::KeyDeleted { key: key.clone() }])
            }
        }
    }

    fn snapshot(&self) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        // Serialize the HashMap to JSON
        let snapshot_data = serde_json::to_vec(&self.store)?;
        Ok(snapshot_data)
    }

    fn restore(&mut self, snapshot: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        // Deserialize HashMap from JSON
        self.store = serde_json::from_slice(snapshot)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kv_state_machine_set() {
        let mut sm = KvStateMachine::new();

        let cmd = KvCommand::Set {
            key: "foo".to_string(),
            value: "bar".to_string(),
        };

        let events = sm.apply(&cmd).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            KvEvent::KeySet {
                key: "foo".to_string(),
                value: "bar".to_string()
            }
        );

        assert_eq!(sm.get("foo"), Some(&"bar".to_string()));
    }

    #[test]
    fn test_kv_state_machine_delete() {
        let mut sm = KvStateMachine::new();

        // Set a value first
        sm.apply(&KvCommand::Set {
            key: "foo".to_string(),
            value: "bar".to_string(),
        })
        .unwrap();

        // Delete it
        let events = sm
            .apply(&KvCommand::Delete {
                key: "foo".to_string(),
            })
            .unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            KvEvent::KeyDeleted {
                key: "foo".to_string()
            }
        );

        assert_eq!(sm.get("foo"), None);
    }

    #[test]
    fn test_kv_state_machine_snapshot_restore() {
        let mut sm1 = KvStateMachine::new();

        // Add some data
        sm1.apply(&KvCommand::Set {
            key: "key1".to_string(),
            value: "value1".to_string(),
        })
        .unwrap();
        sm1.apply(&KvCommand::Set {
            key: "key2".to_string(),
            value: "value2".to_string(),
        })
        .unwrap();

        // Take snapshot
        let snapshot = sm1.snapshot().unwrap();

        // Create new state machine and restore
        let mut sm2 = KvStateMachine::new();
        sm2.restore(&snapshot).unwrap();

        // Verify restored state
        assert_eq!(sm2.get("key1"), Some(&"value1".to_string()));
        assert_eq!(sm2.get("key2"), Some(&"value2".to_string()));
    }

    #[test]
    fn test_kv_state_machine_multiple_operations() {
        let mut sm = KvStateMachine::new();

        sm.apply(&KvCommand::Set {
            key: "a".to_string(),
            value: "1".to_string(),
        })
        .unwrap();
        sm.apply(&KvCommand::Set {
            key: "b".to_string(),
            value: "2".to_string(),
        })
        .unwrap();
        sm.apply(&KvCommand::Delete {
            key: "a".to_string(),
        })
        .unwrap();

        assert_eq!(sm.get("a"), None);
        assert_eq!(sm.get("b"), Some(&"2".to_string()));
        assert_eq!(sm.keys().len(), 1);
    }
}
