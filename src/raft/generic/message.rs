use serde::{Serialize, Deserialize};
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

/// Wrapper for commands with optional tracking ID
#[derive(Clone, Debug, Serialize)]
pub struct CommandWrapper<C>
where
    C: Clone + Debug + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    pub id: Option<u64>,
    pub command: C,
}

// Manual Deserialize implementation to work with command constraints
impl<'de, C> Deserialize<'de> for CommandWrapper<C>
where
    C: Clone + Debug + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    fn deserialize<D>(deserializer: D) -> Result<CommandWrapper<C>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct CommandWrapperHelper<T> {
            id: Option<u64>,
            command: T,
        }

        let helper = CommandWrapperHelper::<C>::deserialize(deserializer)?;
        Ok(CommandWrapper {
            id: helper.id,
            command: helper.command,
        })
    }
}


#[derive(Clone, Debug)]
pub enum Message<C>
where
    C: Clone + Debug + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    Propose {
        id: u8,
        command: C,
    },
    Raft(raft::prelude::Message),
    ConfChangeV2 {
        id: u8,
        change: raft::prelude::ConfChangeV2,
    },
    Campaign,
    AddNode {
        node_id: u64,
        address: String,
    },
    RemoveNode {
        node_id: u64,
    },
}

impl<C> Message<C>
where
    C: Clone + Debug + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    /// Convert Message<C> directly to protobuf GenericMessage
    /// This eliminates the need for SerializableMessage and JSON serialization
    pub fn to_protobuf(&self) -> Result<raft_proto::GenericMessage, Box<dyn std::error::Error>> {
        use protobuf::Message as ProtobufMessage;

        let message = match self {
            Message::Propose { id, command } => {
                let command_json = serde_json::to_vec(command)?;
                raft_proto::generic_message::Message::Propose(raft_proto::ProposeMessage {
                    id: *id as u32,
                    command_json,
                })
            },
            Message::Raft(raft_msg) => {
                let bytes = raft_msg.write_to_bytes()?;
                raft_proto::generic_message::Message::RaftMessage(bytes)
            },
            Message::ConfChangeV2 { id, change } => {
                let bytes = change.write_to_bytes()?;
                raft_proto::generic_message::Message::ConfChange(raft_proto::ConfChangeV2Message {
                    id: *id as u32,
                    change_bytes: bytes,
                })
            },
            Message::Campaign => {
                raft_proto::generic_message::Message::Campaign(raft_proto::CampaignMessage {})
            },
            Message::AddNode { node_id, address } => {
                raft_proto::generic_message::Message::AddNode(raft_proto::AddNodeMessage {
                    node_id: *node_id,
                    address: address.clone(),
                })
            },
            Message::RemoveNode { node_id } => {
                raft_proto::generic_message::Message::RemoveNode(raft_proto::RemoveNodeMessage {
                    node_id: *node_id,
                })
            },
        };

        Ok(raft_proto::GenericMessage {
            message: Some(message),
        })
    }

    /// Convert protobuf GenericMessage directly to Message<C>
    /// This eliminates the need for SerializableMessage and JSON deserialization
    pub fn from_protobuf(proto_msg: raft_proto::GenericMessage) -> Result<Self, Box<dyn std::error::Error>> {
        use protobuf::Message as ProtobufMessage;

        let message = proto_msg.message
            .ok_or("GenericMessage missing message field")?;

        Ok(match message {
            raft_proto::generic_message::Message::Propose(propose) => {
                let command: C = serde_json::from_slice(&propose.command_json)?;
                Message::Propose {
                    id: propose.id as u8,
                    command,
                }
            },
            raft_proto::generic_message::Message::RaftMessage(bytes) => {
                let raft_msg = raft::prelude::Message::parse_from_bytes(&bytes)?;
                Message::Raft(raft_msg)
            },
            raft_proto::generic_message::Message::ConfChange(conf_change) => {
                let change = raft::prelude::ConfChangeV2::parse_from_bytes(&conf_change.change_bytes)?;
                Message::ConfChangeV2 {
                    id: conf_change.id as u8,
                    change,
                }
            },
            raft_proto::generic_message::Message::Campaign(_) => {
                Message::Campaign
            },
            raft_proto::generic_message::Message::AddNode(add_node) => {
                Message::AddNode {
                    node_id: add_node.node_id,
                    address: add_node.address,
                }
            },
            raft_proto::generic_message::Message::RemoveNode(remove_node) => {
                Message::RemoveNode {
                    node_id: remove_node.node_id,
                }
            },
        })
    }
}