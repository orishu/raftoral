use crate::raft::generic::message::CommandExecutor;
use crate::workflow::commands::*;
use crate::workflow::error::*;
use crate::workflow::registry::WorkflowRegistry;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// Helper function to create a composite key from workflow_id and checkpoint_key
pub(crate) fn make_queue_key(workflow_id: &str, key: &str) -> String {
    format!("{}:{}", workflow_id, key)
}

/// Events emitted when workflow state changes
#[derive(Clone, Debug)]
pub enum WorkflowEvent {
    /// A workflow has started with input data
    Started { workflow_id: String },
    /// A workflow has completed with output data
    Completed { workflow_id: String, result: Vec<u8> },
    /// A workflow has failed with error data
    Failed { workflow_id: String, error: Vec<u8> },
    /// A checkpoint was set for a workflow
    CheckpointSet { workflow_id: String, key: String, value: Vec<u8> },
    /// Workflow ownership changed (e.g., due to node failure)
    OwnershipChanged { workflow_id: String, new_owner_node_id: u64 },
}

/// Workflow state for tracking workflows across the cluster
#[derive(Default)]
pub(crate) struct WorkflowState {
    /// Track workflow status by ID
    pub workflows: HashMap<String, WorkflowStatus>,
    /// Store workflow results (serialized) by ID for retrieval after completion
    pub results: HashMap<String, Vec<u8>>,
    /// Queue of checkpoint values that arrived before execution reached them
    /// Key: "workflow_id:checkpoint_key", Value: Queue of serialized values
    /// This enables late followers to catch up without blocking on consensus
    pub checkpoint_queues: HashMap<String, VecDeque<Vec<u8>>>,
    /// Checkpoint history for snapshot creation (never popped, only appended)
    /// Complete history for active workflows, cleaned up on workflow completion
    pub checkpoint_history: crate::workflow::snapshot::CheckpointHistory,
    /// Track command completion by command ID (for propose_and_sync polling)
    /// Maps command_id -> (completed: bool, error: Option<String>)
    pub command_completions: HashMap<u64, (bool, Option<String>)>,
}

/// Command executor for workflow commands with embedded state
pub struct WorkflowCommandExecutor {
    /// Internal state protected by mutex
    pub(crate) state: Arc<Mutex<WorkflowState>>,
    /// Event broadcaster for workflow state changes
    event_tx: broadcast::Sender<WorkflowEvent>,
    /// Reference to workflow registry for spawning executions
    registry: Arc<Mutex<WorkflowRegistry>>,
    /// Reference to workflow runtime (set after construction)
    runtime: Arc<Mutex<Option<Arc<crate::workflow::runtime::WorkflowRuntime>>>>,
    /// Workflow ownership map (workflow_id -> owner_node_id)
    pub ownership_map: crate::workflow::ownership::WorkflowOwnershipMap,
    /// This node's ID for ownership checks
    pub node_id: Arc<Mutex<Option<u64>>>,
}

impl Default for WorkflowCommandExecutor {
    fn default() -> Self {
        let (event_tx, _) = broadcast::channel(1000);
        Self {
            state: Arc::new(Mutex::new(WorkflowState::default())),
            event_tx,
            registry: Arc::new(Mutex::new(WorkflowRegistry::new())),
            runtime: Arc::new(Mutex::new(None)),
            ownership_map: crate::workflow::ownership::WorkflowOwnershipMap::new(),
            node_id: Arc::new(Mutex::new(None)),
        }
    }
}

impl WorkflowCommandExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the workflow runtime reference (called after runtime construction)
    pub fn set_runtime(&self, runtime: Arc<crate::workflow::runtime::WorkflowRuntime>) {
        let mut rt = self.runtime.lock().unwrap();
        *rt = Some(runtime);
    }

    /// Set this node's ID for ownership checks
    pub fn set_node_id(&self, id: u64) {
        let mut node_id = self.node_id.lock().unwrap();
        *node_id = Some(id);
    }

    /// Get this node's ID
    pub fn get_node_id(&self) -> Option<u64> {
        *self.node_id.lock().unwrap()
    }

    /// Get the workflow registry for registration
    pub fn registry(&self) -> Arc<Mutex<WorkflowRegistry>> {
        self.registry.clone()
    }

    fn emit_started_event(&self, workflow_id: &str) {
        let event = WorkflowEvent::Started {
            workflow_id: workflow_id.to_string(),
        };
        let _ = self.event_tx.send(event);
    }

    fn emit_completed_event(&self, workflow_id: &str, result: Vec<u8>) {
        let mut state = self.state.lock().unwrap();
        state.workflows.insert(workflow_id.to_string(), WorkflowStatus::Completed);
        drop(state);

        let event = WorkflowEvent::Completed {
            workflow_id: workflow_id.to_string(),
            result,
        };
        let _ = self.event_tx.send(event);
    }

    fn emit_checkpoint_event(&self, workflow_id: &str, key: &str, value: Vec<u8>) {
        let event = WorkflowEvent::CheckpointSet {
            workflow_id: workflow_id.to_string(),
            key: key.to_string(),
            value,
        };
        let _ = self.event_tx.send(event);
    }

    fn emit_ownership_changed_event(&self, workflow_id: &str, new_owner_node_id: u64) {
        let event = WorkflowEvent::OwnershipChanged {
            workflow_id: workflow_id.to_string(),
            new_owner_node_id,
        };
        let _ = self.event_tx.send(event);
    }

    /// Get workflow status from executor state
    pub fn get_workflow_status(&self, workflow_id: &str) -> Option<WorkflowStatus> {
        let state = self.state.lock().unwrap();
        state.workflows.get(workflow_id).cloned()
    }

    /// Get stored workflow result (if completed)
    pub fn get_workflow_result(&self, workflow_id: &str) -> Option<Vec<u8>> {
        let state = self.state.lock().unwrap();
        state.results.get(workflow_id).cloned()
    }

    /// Get count of checkpoint queue entries for a workflow
    pub fn get_checkpoint_queue_count(&self, workflow_id: &str) -> usize {
        let state = self.state.lock().unwrap();
        let prefix = format!("{}:", workflow_id);
        state.checkpoint_queues
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .count()
    }

    /// Subscribe to workflow events
    pub fn subscribe_to_workflow(&self, workflow_id: &str) -> crate::workflow::runtime::WorkflowSubscription {
        crate::workflow::runtime::WorkflowSubscription::new(
            self.event_tx.subscribe(),
            workflow_id.to_string()
        )
    }

    /// Mark a command as completed (called after apply)
    pub(crate) fn mark_command_completed(&self, command_id: u64, error: Option<String>) {
        let mut state = self.state.lock().unwrap();
        state.command_completions.insert(command_id, (true, error));
    }

    /// Check if a command is completed (for polling in propose_and_sync)
    pub fn is_command_completed(&self, command_id: u64) -> Option<Result<(), String>> {
        let state = self.state.lock().unwrap();
        state.command_completions.get(&command_id).map(|(_, error)| {
            if let Some(err) = error {
                Err(err.clone())
            } else {
                Ok(())
            }
        })
    }

    /// Get state for snapshot creation (only active workflows)
    pub fn get_state_for_snapshot(&self) -> crate::workflow::snapshot::SnapshotState {
        let state = self.state.lock().unwrap();

        // Only include active workflows
        let active_workflows: HashMap<String, WorkflowStatus> = state.workflows
            .iter()
            .filter(|(_, status)| matches!(status, WorkflowStatus::Running))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        crate::workflow::snapshot::SnapshotState {
            active_workflows,
            checkpoint_history: state.checkpoint_history.clone(),
        }
    }

    /// Restore state from snapshot
    pub fn restore_from_workflow_snapshot(&self, snapshot: crate::workflow::snapshot::WorkflowSnapshot) -> Result<(), Box<dyn std::error::Error>> {
        let mut state = self.state.lock().unwrap();

        // Clear current state
        state.workflows.clear();
        state.checkpoint_history = crate::workflow::snapshot::CheckpointHistory::default();

        // Restore active workflows
        for (workflow_id, status) in snapshot.active_workflows {
            state.workflows.insert(workflow_id, status);
        }

        // Restore checkpoint history
        state.checkpoint_history = snapshot.checkpoint_history.clone();

        // IMPORTANT: Build checkpoint queues from history
        // New node needs queues populated for when execution starts
        for (composite_key, entries) in snapshot.checkpoint_history.all_checkpoints() {
            let mut queue = VecDeque::new();

            // Add all historical values to queue (oldest to newest)
            for entry in entries {
                queue.push_back(entry.value.clone());
            }

            state.checkpoint_queues.insert(composite_key.clone(), queue);
        }

        Ok(())
    }
}

impl CommandExecutor for WorkflowCommandExecutor {
    type Command = WorkflowCommand;

    fn apply_with_index(&self, command: &Self::Command, logger: &slog::Logger, log_index: u64) -> Result<(), Box<dyn std::error::Error>> {
        match command {
            WorkflowCommand::WorkflowStart(data) => {
                slog::info!(logger, "Started workflow";
                           "workflow_id" => &data.workflow_id,
                           "workflow_type" => &data.workflow_type,
                           "version" => data.version,
                           "owner_node_id" => data.owner_node_id);

                // Update ownership map (all nodes)
                self.ownership_map.set_owner(data.workflow_id.clone(), data.owner_node_id);

                // Set workflow status to Running
                {
                    let mut state = self.state.lock().unwrap();
                    state.workflows.insert(data.workflow_id.clone(), WorkflowStatus::Running);
                }

                // Emit started event
                self.emit_started_event(&data.workflow_id);

                // Mark command as completed (for propose_and_sync polling)
                self.mark_command_completed(data.command_id, None);

                // Only the OWNER node spawns workflow execution
                let is_owner = self.get_node_id()
                    .map(|node_id| node_id == data.owner_node_id)
                    .unwrap_or(false);

                if is_owner && !data.workflow_type.is_empty() && data.version > 0 {
                    let registry = self.registry.clone();
                    let runtime_opt = self.runtime.lock().unwrap().clone();

                    if let Some(runtime) = runtime_opt {
                        let workflow_id = data.workflow_id.clone();
                        let workflow_type = data.workflow_type.clone();
                        let version = data.version;
                        let input_bytes = data.input.clone();

                        // Spawn the workflow execution in the background
                        tokio::spawn(async move {
                            // Look up the workflow function
                            let workflow_function = {
                                let registry = registry.lock().unwrap();
                                match registry.get(&workflow_type, version) {
                                    Some(func) => func,
                                    None => {
                                        eprintln!("Workflow '{}' version {} not found in registry", workflow_type, version);
                                        return;
                                    }
                                }
                            };

                            // Create workflow context and WorkflowRun for this execution
                            let context = crate::workflow::context::WorkflowContext {
                                workflow_id: workflow_id.clone(),
                                runtime: runtime.clone(),
                            };

                            let workflow_run = Arc::new(crate::workflow::context::WorkflowRun {
                                context: context.clone(),
                                runtime: runtime.clone(),
                            });

                            // Deserialize input and execute the workflow function
                            let input_any: Box<dyn std::any::Any + Send> = Box::new(input_bytes);

                            match workflow_function.execute(input_any, context).await {
                                Ok(result_bytes) => {
                                    // Workflow execution succeeded - call finish_with to propose WorkflowEnd
                                    // Owner node proposes the WorkflowEnd command
                                    // The proposal will be forwarded to the leader if needed
                                    match workflow_run.finish_with_bytes(result_bytes).await {
                                        Ok(_) => {
                                            slog::info!(slog::Logger::root(slog::Discard, slog::o!()),
                                                       "Workflow execution completed successfully";
                                                       "workflow_id" => &workflow_id);
                                        },
                                        Err(e) => {
                                            eprintln!("Workflow {} failed to propose WorkflowEnd: {:?}", workflow_id, e);
                                        }
                                    }
                                },
                                Err(e) => {
                                    eprintln!("Workflow {} failed during execution: {}", workflow_id, e);
                                    // TODO: Emit failure event or propose WorkflowEnd with error
                                }
                            }
                        });
                    }
                }
            },
            WorkflowCommand::WorkflowEnd(data) => {
                slog::info!(logger, "Ended workflow"; "workflow_id" => &data.workflow_id);

                // Remove ownership (workflow completed)
                self.ownership_map.remove_owner(&data.workflow_id);

                // Store the result in state for later retrieval
                self.state.lock().unwrap().results.insert(data.workflow_id.clone(), data.result.clone());

                // Clean up checkpoint history for completed workflow
                self.state.lock().unwrap()
                    .checkpoint_history
                    .remove_workflow(&data.workflow_id);

                // Clean up checkpoint queues for completed workflow
                let prefix = format!("{}:", data.workflow_id);
                self.state.lock().unwrap()
                    .checkpoint_queues
                    .retain(|key, _| !key.starts_with(&prefix));

                self.emit_completed_event(&data.workflow_id, data.result.clone());

                // Mark command as completed (for propose_and_sync polling)
                self.mark_command_completed(data.command_id, None);
            },
            WorkflowCommand::SetCheckpoint(data) => {
                slog::info!(logger, "Set checkpoint";
                           "workflow_id" => &data.workflow_id,
                           "key" => &data.key,
                           "value_size" => data.value.len());

                // Enqueue the checkpoint value for late followers
                // All nodes enqueue - the consumption side will determine whether to use it
                let queue_key = make_queue_key(&data.workflow_id, &data.key);
                self.state.lock().unwrap()
                    .checkpoint_queues
                    .entry(queue_key)
                    .or_insert_with(VecDeque::new)
                    .push_back(data.value.clone());

                // Add to checkpoint history for snapshots (only for active workflows)
                {
                    let state = self.state.lock().unwrap();
                    if let Some(status) = state.workflows.get(&data.workflow_id) {
                        if matches!(status, WorkflowStatus::Running) {
                            drop(state); // Release lock before getting mutable access
                            let timestamp = crate::workflow::snapshot::current_timestamp();
                            self.state.lock().unwrap().checkpoint_history.add_checkpoint(
                                data.workflow_id.clone(),
                                data.key.clone(),
                                crate::workflow::snapshot::CheckpointEntry {
                                    value: data.value.clone(),
                                    log_index,
                                    timestamp,
                                }
                            );
                        }
                    }
                }

                self.emit_checkpoint_event(&data.workflow_id, &data.key, data.value.clone());

                // Mark command as completed (for propose_and_sync polling)
                self.mark_command_completed(data.command_id, None);
            },
            WorkflowCommand::OwnerChange(data) => {
                slog::info!(logger, "Owner change";
                           "workflow_id" => &data.workflow_id,
                           "old_owner" => data.old_owner_node_id,
                           "new_owner" => data.new_owner_node_id,
                           "reason" => format!("{:?}", data.reason));

                // Update ownership map (all nodes)
                self.ownership_map.set_owner(data.workflow_id.clone(), data.new_owner_node_id);

                // Emit event to wake up waiting workflow executions on the new owner node
                // This allows the waiting workflow execution to immediately become active
                // No need to spawn new execution - the workflow is already running on all nodes,
                // waiting on checkpoints. The OwnershipChanged event will wake up the new owner's
                // execution, which will then check ownership and continue executing actively.
                self.emit_ownership_changed_event(&data.workflow_id, data.new_owner_node_id);

                // Mark command as completed (for propose_and_sync polling)
                self.mark_command_completed(data.command_id, None);
            },
        }
        Ok(())
    }

    /// Override the default trait implementation to actually set the node ID
    fn set_node_id(&self, id: u64) {
        let mut node_id = self.node_id.lock().unwrap();
        *node_id = Some(id);
    }

    /// Handle node removal - reassign workflows if this node is the leader
    fn on_node_removed(&self, removed_node_id: u64, logger: &slog::Logger) {
        slog::info!(logger, "Node removed from cluster";
                   "removed_node_id" => removed_node_id);

        // Get the list of workflows owned by the removed node
        let orphaned_workflows = self.ownership_map.get_workflows_owned_by(removed_node_id);

        if orphaned_workflows.is_empty() {
            slog::info!(logger, "No workflows owned by removed node";
                       "removed_node_id" => removed_node_id);
            return;
        }

        slog::info!(logger, "Found orphaned workflows";
                   "removed_node_id" => removed_node_id,
                   "count" => orphaned_workflows.len());

        // Get the runtime to check if we're the leader and to propose reassignments
        let runtime_opt = self.runtime.lock().unwrap().clone();
        if let Some(runtime) = runtime_opt {
            // Spawn a task to handle reassignment asynchronously
            // This avoids blocking the ConfChange application
            tokio::spawn(async move {
                // Only the leader should reassign workflows, and it can't be the removed node
                let our_node_id = runtime.cluster.node_id();
                if our_node_id == removed_node_id {
                    // We're the node being removed - don't try to reassign
                    return;
                }

                if !runtime.cluster.is_leader().await {
                    return;
                }

                // Get list of available nodes (excluding the removed one)
                let node_ids = runtime.cluster.get_node_ids();
                let available_nodes: Vec<u64> = node_ids
                    .into_iter()
                    .filter(|&id| id != removed_node_id)
                    .collect();

                if available_nodes.is_empty() {
                    eprintln!("No available nodes to reassign workflows to!");
                    return;
                }

                // Reassign each workflow to a different node (simple round-robin)
                for (idx, workflow_id) in orphaned_workflows.iter().enumerate() {
                    // Select new owner using round-robin
                    let new_owner_id = available_nodes[idx % available_nodes.len()];

                    // Propose OwnerChange command
                    let command_id = runtime.cluster.generate_command_id();
                    let command = WorkflowCommand::OwnerChange(OwnerChangeData {
                        command_id,
                        workflow_id: workflow_id.clone(),
                        old_owner_node_id: removed_node_id,
                        new_owner_node_id: new_owner_id,
                        reason: OwnerChangeReason::NodeFailure,
                    });

                    if let Err(e) = runtime.cluster.propose_and_sync(command).await {
                        eprintln!("Failed to reassign workflow {}: {:?}", workflow_id, e);
                    }
                }
            });
        }
    }

    /// Create a snapshot of the current workflow state
    fn create_snapshot(&self, snapshot_index: u64) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let snapshot_state = self.get_state_for_snapshot();

        // Note: We don't have access to conf_state here, so we create a minimal snapshot
        // The RaftNode will add raft metadata (term, conf_state) separately
        let snapshot = crate::workflow::snapshot::WorkflowSnapshot {
            snapshot_index,
            timestamp: crate::workflow::snapshot::current_timestamp(),
            active_workflows: snapshot_state.active_workflows,
            checkpoint_history: snapshot_state.checkpoint_history,
            metadata: crate::workflow::snapshot::SnapshotMetadata {
                creator_node_id: self.get_node_id().unwrap_or(0),
                voters: vec![], // Will be filled by RaftNode
                learners: vec![], // Will be filled by RaftNode
            },
        };

        let snapshot_data = serde_json::to_vec(&snapshot)?;
        Ok(snapshot_data)
    }

    /// Restore state from a snapshot (CommandExecutor trait implementation)
    fn restore_from_snapshot(&self, snapshot_data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        let snapshot: crate::workflow::snapshot::WorkflowSnapshot = serde_json::from_slice(snapshot_data)?;

        // Delegate to the existing restore logic
        self.restore_from_workflow_snapshot(snapshot)
    }

    /// Check if snapshot should be created based on log size
    fn should_create_snapshot(&self, log_size: u64, snapshot_interval: u64) -> bool {
        log_size >= snapshot_interval
    }
}
