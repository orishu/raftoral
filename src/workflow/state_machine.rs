//! Workflow State Machine (Layer 4)
//!
//! Maintains workflow execution state, checkpoints, and ownership.
//! Emits events through the EventBus for the runtime to observe.

use crate::raft::generic::StateMachine;
use crate::workflow::error::WorkflowStatus;
use crate::workflow::event::WorkflowEvent;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

/// Helper function to create a composite key from workflow_id and checkpoint_key
pub(crate) fn make_queue_key(workflow_id: &str, key: &str) -> String {
    format!("{}:{}", workflow_id, key)
}

/// Commands for the workflow state machine
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum WorkflowCommand {
    /// Start a new workflow execution
    WorkflowStart {
        workflow_id: String,
        workflow_type: String,
        version: u32,
        input: Vec<u8>,
        owner_node_id: u64,
    },

    /// End a workflow execution with a result
    WorkflowEnd {
        workflow_id: String,
        result: Vec<u8>,
    },

    /// Set a checkpoint value for a workflow
    SetCheckpoint {
        workflow_id: String,
        key: String,
        value: Vec<u8>,
    },

    /// Change ownership of a workflow to a different node
    OwnerChange {
        workflow_id: String,
        old_owner_node_id: u64,
        new_owner_node_id: u64,
        reason: OwnerChangeReason,
    },
}

/// Reason for ownership change
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum OwnerChangeReason {
    /// Node was removed from cluster configuration
    NodeFailure,
    /// Leader-initiated load balancing
    LoadBalancing,
}

/// Workflow state machine that tracks workflow execution state
#[derive(Debug)]
pub struct WorkflowStateMachine {
    /// Track workflow status by ID
    workflows: HashMap<String, WorkflowStatus>,

    /// Store workflow inputs (serialized) by ID for workflow execution
    inputs: HashMap<String, Vec<u8>>,

    /// Store workflow results (serialized) by ID for retrieval after completion
    results: HashMap<String, Vec<u8>>,

    /// Transient queue for checkpoint values awaiting consumption by followers
    /// Key: "workflow_id:checkpoint_key", Value: Queue of serialized values
    /// This enables followers to consume values that arrived before they reached that execution point
    /// Owner pops its own values from this queue after proposing
    checkpoint_queues: HashMap<String, VecDeque<Vec<u8>>>,

    /// Complete history of checkpoints for snapshot/recovery
    /// Key: "workflow_id:checkpoint_key", Value: Queue of all historical values
    /// Never popped, only appended. Cleaned up on workflow completion.
    /// Used for new nodes joining and restoring from snapshots
    checkpoint_history: HashMap<String, VecDeque<Vec<u8>>>,

    /// Workflow ownership map (workflow_id → owner_node_id)
    ownership: HashMap<String, u64>,
}

impl WorkflowStateMachine {
    /// Create a new workflow state machine
    pub fn new() -> Self {
        Self {
            workflows: HashMap::new(),
            inputs: HashMap::new(),
            results: HashMap::new(),
            checkpoint_queues: HashMap::new(),
            checkpoint_history: HashMap::new(),
            ownership: HashMap::new(),
        }
    }

    /// Get workflow status
    pub fn get_workflow_status(&self, workflow_id: &str) -> Option<&WorkflowStatus> {
        self.workflows.get(workflow_id)
    }

    /// Get workflow result
    pub fn get_result(&self, workflow_id: &str) -> Option<&Vec<u8>> {
        self.results.get(workflow_id)
    }

    /// Get workflow input
    pub fn get_input(&self, workflow_id: &str) -> Option<&Vec<u8>> {
        self.inputs.get(workflow_id)
    }

    /// Get workflow owner node ID
    pub fn get_owner(&self, workflow_id: &str) -> Option<u64> {
        self.ownership.get(workflow_id).copied()
    }

    /// List all active workflow IDs
    pub fn list_active_workflows(&self) -> Vec<String> {
        self.workflows
            .iter()
            .filter(|(_, status)| **status == WorkflowStatus::Running)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Get queued checkpoint value (for late follower catch-up)
    pub fn pop_queued_checkpoint(&mut self, workflow_id: &str, key: &str) -> Option<Vec<u8>> {
        let queue_key = make_queue_key(workflow_id, key);
        self.checkpoint_queues
            .get_mut(&queue_key)
            .and_then(|queue| queue.pop_front())
    }

    /// Check if there are queued checkpoints for a key
    pub fn has_queued_checkpoint(&self, workflow_id: &str, key: &str) -> bool {
        let queue_key = make_queue_key(workflow_id, key);
        self.checkpoint_queues
            .get(&queue_key)
            .map_or(false, |queue| !queue.is_empty())
    }
}

impl Default for WorkflowStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl StateMachine for WorkflowStateMachine {
    type Command = WorkflowCommand;
    type Event = WorkflowEvent;

    fn apply(
        &mut self,
        command: &Self::Command,
    ) -> Result<Vec<Self::Event>, Box<dyn std::error::Error>> {
        match command {
            WorkflowCommand::WorkflowStart {
                workflow_id,
                workflow_type,
                version,
                input,
                owner_node_id,
            } => {
                // Mark workflow as running
                self.workflows
                    .insert(workflow_id.clone(), WorkflowStatus::Running);

                // Store input for workflow execution
                self.inputs.insert(workflow_id.clone(), input.clone());

                // Set ownership
                self.ownership.insert(workflow_id.clone(), *owner_node_id);

                Ok(vec![WorkflowEvent::WorkflowStarted {
                    workflow_id: workflow_id.clone(),
                    workflow_type: workflow_type.clone(),
                    version: *version,
                    owner_node_id: *owner_node_id,
                }])
            }

            WorkflowCommand::WorkflowEnd {
                workflow_id,
                result,
            } => {
                // Mark workflow as completed
                self.workflows
                    .insert(workflow_id.clone(), WorkflowStatus::Completed);

                // Store result
                self.results.insert(workflow_id.clone(), result.clone());

                // Clean up input (no longer needed)
                self.inputs.remove(workflow_id);

                // Clean up both checkpoint queues and history for this workflow
                let prefix = format!("{}:", workflow_id);
                self.checkpoint_queues
                    .retain(|k, _| !k.starts_with(&prefix));
                self.checkpoint_history
                    .retain(|k, _| !k.starts_with(&prefix));

                // Keep ownership for result retrieval

                Ok(vec![WorkflowEvent::WorkflowCompleted {
                    workflow_id: workflow_id.clone(),
                    result: result.clone(),
                }])
            }

            WorkflowCommand::SetCheckpoint {
                workflow_id,
                key,
                value,
            } => {
                let queue_key = make_queue_key(workflow_id, key);

                // Push to transient queue (for active follower coordination)
                self.checkpoint_queues
                    .entry(queue_key.clone())
                    .or_insert_with(VecDeque::new)
                    .push_back(value.clone());

                // Push to history (for snapshots and new nodes)
                self.checkpoint_history
                    .entry(queue_key)
                    .or_insert_with(VecDeque::new)
                    .push_back(value.clone());

                Ok(vec![WorkflowEvent::CheckpointSet {
                    workflow_id: workflow_id.clone(),
                    key: key.clone(),
                    value: value.clone(),
                }])
            }

            WorkflowCommand::OwnerChange {
                workflow_id,
                old_owner_node_id,
                new_owner_node_id,
                reason: _,
            } => {
                // Update ownership
                self.ownership
                    .insert(workflow_id.clone(), *new_owner_node_id);

                Ok(vec![WorkflowEvent::OwnershipChanged {
                    workflow_id: workflow_id.clone(),
                    old_owner_node_id: *old_owner_node_id,
                    new_owner_node_id: *new_owner_node_id,
                }])
            }
        }
    }

    fn snapshot(&self) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        #[derive(Serialize)]
        struct Snapshot {
            workflows: HashMap<String, WorkflowStatus>,
            inputs: HashMap<String, Vec<u8>>,
            results: HashMap<String, Vec<u8>>,
            checkpoint_history: HashMap<String, VecDeque<Vec<u8>>>,
            ownership: HashMap<String, u64>,
        }

        let snapshot = Snapshot {
            workflows: self.workflows.clone(),
            inputs: self.inputs.clone(),
            results: self.results.clone(),
            checkpoint_history: self.checkpoint_history.clone(),
            ownership: self.ownership.clone(),
        };

        let snapshot_data = serde_json::to_vec(&snapshot)?;
        Ok(snapshot_data)
    }

    fn restore(&mut self, snapshot: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        #[derive(Deserialize)]
        struct Snapshot {
            workflows: HashMap<String, WorkflowStatus>,
            inputs: HashMap<String, Vec<u8>>,
            results: HashMap<String, Vec<u8>>,
            checkpoint_history: HashMap<String, VecDeque<Vec<u8>>>,
            ownership: HashMap<String, u64>,
        }

        let snapshot: Snapshot = serde_json::from_slice(snapshot)?;
        self.workflows = snapshot.workflows;
        self.inputs = snapshot.inputs;
        self.results = snapshot.results;
        self.checkpoint_history = snapshot.checkpoint_history.clone();
        self.ownership = snapshot.ownership;

        // Build transient checkpoint_queues from history
        // New node needs queues populated for when execution starts
        self.checkpoint_queues.clear();
        for (composite_key, history) in snapshot.checkpoint_history {
            let mut queue = VecDeque::new();
            // Add all historical values to queue (in order)
            for value in history {
                queue.push_back(value);
            }
            self.checkpoint_queues.insert(composite_key, queue);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workflow_start_and_end() {
        let mut sm = WorkflowStateMachine::new();

        // Start workflow
        let start_cmd = WorkflowCommand::WorkflowStart {
            workflow_id: "wf-1".to_string(),
            workflow_type: "test".to_string(),
            version: 1,
            input: vec![1, 2, 3],
            owner_node_id: 1,
        };

        let events = sm.apply(&start_cmd).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            sm.get_workflow_status("wf-1"),
            Some(&WorkflowStatus::Running)
        );
        assert_eq!(sm.get_owner("wf-1"), Some(1));

        // End workflow
        let end_cmd = WorkflowCommand::WorkflowEnd {
            workflow_id: "wf-1".to_string(),
            result: vec![4, 5, 6],
        };

        let events = sm.apply(&end_cmd).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            sm.get_workflow_status("wf-1"),
            Some(&WorkflowStatus::Completed)
        );
        assert_eq!(sm.get_result("wf-1"), Some(&vec![4, 5, 6]));
    }

    #[test]
    fn test_checkpoint_queuing() {
        let mut sm = WorkflowStateMachine::new();

        // Set checkpoint
        let checkpoint_cmd = WorkflowCommand::SetCheckpoint {
            workflow_id: "wf-1".to_string(),
            key: "counter".to_string(),
            value: vec![0, 0, 0, 1],
        };

        sm.apply(&checkpoint_cmd).unwrap();

        // Check queue
        assert!(sm.has_queued_checkpoint("wf-1", "counter"));

        // Pop from queue
        let value = sm.pop_queued_checkpoint("wf-1", "counter").unwrap();
        assert_eq!(value, vec![0, 0, 0, 1]);
        assert!(!sm.has_queued_checkpoint("wf-1", "counter"));
    }

    #[test]
    fn test_ownership_change() {
        let mut sm = WorkflowStateMachine::new();

        // Start workflow with owner node 1
        let start_cmd = WorkflowCommand::WorkflowStart {
            workflow_id: "wf-1".to_string(),
            workflow_type: "test".to_string(),
            version: 1,
            input: vec![],
            owner_node_id: 1,
        };
        sm.apply(&start_cmd).unwrap();
        assert_eq!(sm.get_owner("wf-1"), Some(1));

        // Change ownership to node 2
        let change_cmd = WorkflowCommand::OwnerChange {
            workflow_id: "wf-1".to_string(),
            old_owner_node_id: 1,
            new_owner_node_id: 2,
            reason: OwnerChangeReason::NodeFailure,
        };
        sm.apply(&change_cmd).unwrap();
        assert_eq!(sm.get_owner("wf-1"), Some(2));
    }

    #[test]
    fn test_snapshot_and_restore() {
        let mut sm = WorkflowStateMachine::new();

        // Add some state
        sm.apply(&WorkflowCommand::WorkflowStart {
            workflow_id: "wf-1".to_string(),
            workflow_type: "test".to_string(),
            version: 1,
            input: vec![1, 2, 3],
            owner_node_id: 1,
        })
        .unwrap();

        sm.apply(&WorkflowCommand::SetCheckpoint {
            workflow_id: "wf-1".to_string(),
            key: "counter".to_string(),
            value: vec![0, 0, 0, 1],
        })
        .unwrap();

        // Create snapshot
        let snapshot = sm.snapshot().unwrap();

        // Create new state machine and restore
        let mut sm2 = WorkflowStateMachine::new();
        sm2.restore(&snapshot).unwrap();

        // Verify state was restored
        assert_eq!(
            sm2.get_workflow_status("wf-1"),
            Some(&WorkflowStatus::Running)
        );
        assert_eq!(sm2.get_owner("wf-1"), Some(1));
        assert!(sm2.has_queued_checkpoint("wf-1", "counter"));
    }
}
