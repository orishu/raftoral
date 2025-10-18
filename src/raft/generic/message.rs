use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fmt::Debug;
use crate::grpc::server::raft_proto;

/// Trait for executing commands that have been committed through Raft
pub trait CommandExecutor: Send + Sync + 'static {
    /// The command type that this executor handles
    type Command: Clone + Debug + Serialize + DeserializeOwned + Send + Sync + 'static;

    /// Apply a command to the state machine with log index tracking
    fn apply_with_index(&self, command: &Self::Command, logger: &slog::Logger, log_index: u64) -> Result<(), Box<dyn std::error::Error>>;

    /// Apply a command to the state machine (backward compatibility)
    fn apply(&self, command: &Self::Command, logger: &slog::Logger) -> Result<(), Box<dyn std::error::Error>> {
        self.apply_with_index(command, logger, 0)
    }

    /// Set the node ID for ownership checks (default: no-op)
    fn set_node_id(&self, _node_id: u64) {
        // Default implementation does nothing
        // WorkflowCommandExecutor overrides this
    }

    /// Notify when a node is added to the cluster
    /// This allows the executor to react to cluster membership changes
    fn on_node_added(&self, _added_node_id: u64, _address: &str, _logger: &slog::Logger) {
        // Default implementation does nothing
        // ManagementCommandExecutor overrides this for dynamic execution cluster construction
    }

    /// Notify when a node is removed from the cluster
    /// This allows the executor to react to node failures (e.g., reassign workflows)
    fn on_node_removed(&self, _removed_node_id: u64, _logger: &slog::Logger) {
        // Default implementation does nothing
        // WorkflowCommandExecutor overrides this for workflow reassignment
    }

    /// Create a snapshot of the current state
    /// Returns serialized snapshot data
    fn create_snapshot(&self, _snapshot_index: u64) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        // Default implementation: no snapshot support
        Ok(Vec::new())
    }

    /// Restore state from a snapshot
    /// Called when receiving a snapshot from leader or during recovery
    fn restore_from_snapshot(&self, _snapshot_data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        // Default implementation: no snapshot support
        Ok(())
    }

    /// Check if snapshot should be created based on log size
    /// Executors can override to implement custom logic
    fn should_create_snapshot(&self, _log_size: u64, _snapshot_interval: u64) -> bool {
        // Default implementation: use simple threshold check
        false
    }
}

#[derive(Clone, Debug)]
pub enum Message<C>
where
    C: Clone + Debug + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    Propose {
        command: C,
        sync_id: Option<u64>,  // For synchronous proposal tracking
    },
    Raft(raft::prelude::Message),
    Campaign,
    AddNode {
        node_id: u64,
        address: String,
        sync_id: Option<u64>,  // For synchronous tracking
    },
    RemoveNode {
        node_id: u64,
        sync_id: Option<u64>,  // For synchronous tracking
    },
}

impl<C> Message<C>
where
    C: Clone + Debug + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    /// Convert Message<C> directly to protobuf GenericMessage
    /// This eliminates the need for SerializableMessage and JSON serialization
    ///
    /// # Arguments
    /// * `cluster_id` - The cluster ID for routing (0 = management, 1+ = execution clusters)
    pub fn to_protobuf(&self, cluster_id: u64) -> Result<raft_proto::GenericMessage, Box<dyn std::error::Error + Send + Sync>> {
        use protobuf::Message as ProtobufMessage;

        let message = match self {
            Message::Propose { command, sync_id } => {
                let command_json = serde_json::to_vec(command)?;
                raft_proto::generic_message::Message::Propose(raft_proto::ProposeMessage {
                    command_json,
                    sync_id: sync_id.unwrap_or(0),
                })
            },
            Message::Raft(raft_msg) => {
                let bytes = raft_msg.write_to_bytes()?;
                raft_proto::generic_message::Message::RaftMessage(bytes)
            },
            Message::Campaign => {
                raft_proto::generic_message::Message::Campaign(raft_proto::CampaignMessage {})
            },
            Message::AddNode { node_id, address, sync_id } => {
                raft_proto::generic_message::Message::AddNode(raft_proto::AddNodeMessage {
                    node_id: *node_id,
                    address: address.clone(),
                    sync_id: sync_id.unwrap_or(0),
                })
            },
            Message::RemoveNode { node_id, sync_id } => {
                raft_proto::generic_message::Message::RemoveNode(raft_proto::RemoveNodeMessage {
                    node_id: *node_id,
                    sync_id: sync_id.unwrap_or(0),
                })
            },
        };

        Ok(raft_proto::GenericMessage {
            cluster_id,
            message: Some(message),
        })
    }

    /// Convert protobuf GenericMessage directly to Message<C>
    /// This eliminates the need for SerializableMessage and JSON deserialization
    pub fn from_protobuf(proto_msg: raft_proto::GenericMessage) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        use protobuf::Message as ProtobufMessage;

        let message = proto_msg.message
            .ok_or("GenericMessage missing message field")?;

        Ok(match message {
            raft_proto::generic_message::Message::Propose(propose) => {
                let command: C = serde_json::from_slice(&propose.command_json)?;
                Message::Propose {
                    command,
                    sync_id: if propose.sync_id == 0 { None } else { Some(propose.sync_id) },
                }
            },
            raft_proto::generic_message::Message::RaftMessage(bytes) => {
                let raft_msg = raft::prelude::Message::parse_from_bytes(&bytes)?;
                Message::Raft(raft_msg)
            },
            raft_proto::generic_message::Message::Campaign(_) => {
                Message::Campaign
            },
            raft_proto::generic_message::Message::AddNode(add_node) => {
                Message::AddNode {
                    node_id: add_node.node_id,
                    address: add_node.address,
                    sync_id: if add_node.sync_id == 0 { None } else { Some(add_node.sync_id) },
                }
            },
            raft_proto::generic_message::Message::RemoveNode(remove_node) => {
                Message::RemoveNode {
                    node_id: remove_node.node_id,
                    sync_id: if remove_node.sync_id == 0 { None } else { Some(remove_node.sync_id) },
                }
            },
            raft_proto::generic_message::Message::ConfChange(_) => {
                return Err("ConfChangeV2 message variant is deprecated".into());
            },
        })
    }
}