use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fmt::Debug;
use crate::grpc::server::raft_proto;

// Re-export CommandExecutor from its own module
pub use super::command_executor::CommandExecutor;

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
