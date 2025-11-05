//! Management State Machine (Layer 4)
//!
//! Maintains metadata about sub-clusters. The actual sub-cluster Raft nodes
//! are managed by the ManagementRuntime (Layer 7), while this state machine
//! just tracks the metadata.

use crate::management::ManagementEvent;
use crate::raft::generic2::StateMachine;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Commands for the management state machine
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum ManagementCommand {
    /// Create a new sub-cluster
    CreateSubCluster {
        cluster_id: Uuid,
        node_ids: Vec<u64>,
    },

    /// Delete a sub-cluster
    DeleteSubCluster {
        cluster_id: Uuid,
    },

    /// Add a node to a sub-cluster
    AddNodeToSubCluster {
        cluster_id: Uuid,
        node_id: u64,
    },

    /// Remove a node from a sub-cluster
    RemoveNodeFromSubCluster {
        cluster_id: Uuid,
        node_id: u64,
    },

    /// Set metadata for a sub-cluster
    SetMetadata {
        cluster_id: Uuid,
        key: String,
        value: String,
    },

    /// Delete metadata for a sub-cluster
    DeleteMetadata {
        cluster_id: Uuid,
        key: String,
    },
}

/// Metadata for a sub-cluster
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SubClusterMetadata {
    /// Node IDs in this sub-cluster
    pub node_ids: Vec<u64>,

    /// Custom metadata key-value pairs
    pub metadata: HashMap<String, String>,
}

/// Management state machine that tracks sub-cluster metadata
#[derive(Debug)]
pub struct ManagementStateMachine {
    /// Sub-cluster ID → metadata
    sub_clusters: HashMap<Uuid, SubClusterMetadata>,
}

impl ManagementStateMachine {
    /// Create a new management state machine
    pub fn new() -> Self {
        Self {
            sub_clusters: HashMap::new(),
        }
    }

    /// Get metadata for a sub-cluster
    pub fn get_sub_cluster(&self, cluster_id: &Uuid) -> Option<&SubClusterMetadata> {
        self.sub_clusters.get(cluster_id)
    }

    /// List all sub-cluster IDs
    pub fn list_sub_clusters(&self) -> Vec<Uuid> {
        self.sub_clusters.keys().copied().collect()
    }

    /// Get all sub-cluster metadata
    pub fn get_all_sub_clusters(&self) -> &HashMap<Uuid, SubClusterMetadata> {
        &self.sub_clusters
    }
}

impl Default for ManagementStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl StateMachine for ManagementStateMachine {
    type Command = ManagementCommand;
    type Event = ManagementEvent;

    fn apply(&mut self, command: &Self::Command) -> Result<Vec<Self::Event>, Box<dyn std::error::Error>> {
        match command {
            ManagementCommand::CreateSubCluster { cluster_id, node_ids } => {
                self.sub_clusters.insert(
                    *cluster_id,
                    SubClusterMetadata {
                        node_ids: node_ids.clone(),
                        metadata: HashMap::new(),
                    },
                );
                Ok(vec![ManagementEvent::SubClusterCreated {
                    cluster_id: *cluster_id,
                    node_ids: node_ids.clone(),
                }])
            }

            ManagementCommand::DeleteSubCluster { cluster_id } => {
                self.sub_clusters.remove(cluster_id);
                Ok(vec![ManagementEvent::SubClusterDeleted {
                    cluster_id: *cluster_id,
                }])
            }

            ManagementCommand::AddNodeToSubCluster { cluster_id, node_id } => {
                if let Some(metadata) = self.sub_clusters.get_mut(cluster_id) {
                    if !metadata.node_ids.contains(node_id) {
                        metadata.node_ids.push(*node_id);
                    }
                    Ok(vec![ManagementEvent::NodeAddedToSubCluster {
                        cluster_id: *cluster_id,
                        node_id: *node_id,
                    }])
                } else {
                    Err(format!("Sub-cluster {} not found", cluster_id).into())
                }
            }

            ManagementCommand::RemoveNodeFromSubCluster { cluster_id, node_id } => {
                if let Some(metadata) = self.sub_clusters.get_mut(cluster_id) {
                    metadata.node_ids.retain(|id| id != node_id);
                    Ok(vec![ManagementEvent::NodeRemovedFromSubCluster {
                        cluster_id: *cluster_id,
                        node_id: *node_id,
                    }])
                } else {
                    Err(format!("Sub-cluster {} not found", cluster_id).into())
                }
            }

            ManagementCommand::SetMetadata { cluster_id, key, value } => {
                if let Some(metadata) = self.sub_clusters.get_mut(cluster_id) {
                    metadata.metadata.insert(key.clone(), value.clone());
                    Ok(vec![ManagementEvent::MetadataUpdated {
                        cluster_id: *cluster_id,
                        key: key.clone(),
                        value: Some(value.clone()),
                    }])
                } else {
                    Err(format!("Sub-cluster {} not found", cluster_id).into())
                }
            }

            ManagementCommand::DeleteMetadata { cluster_id, key } => {
                if let Some(metadata) = self.sub_clusters.get_mut(cluster_id) {
                    metadata.metadata.remove(key);
                    Ok(vec![ManagementEvent::MetadataUpdated {
                        cluster_id: *cluster_id,
                        key: key.clone(),
                        value: None,
                    }])
                } else {
                    Err(format!("Sub-cluster {} not found", cluster_id).into())
                }
            }
        }
    }

    fn snapshot(&self) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let snapshot_data = serde_json::to_vec(&self.sub_clusters)?;
        Ok(snapshot_data)
    }

    fn restore(&mut self, snapshot: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        self.sub_clusters = serde_json::from_slice(snapshot)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_sub_cluster() {
        let mut sm = ManagementStateMachine::new();
        let cluster_id = Uuid::new_v4();

        let cmd = ManagementCommand::CreateSubCluster {
            cluster_id,
            node_ids: vec![1, 2, 3],
        };

        let events = sm.apply(&cmd).unwrap();
        assert_eq!(events.len(), 1);

        let metadata = sm.get_sub_cluster(&cluster_id).unwrap();
        assert_eq!(metadata.node_ids, vec![1, 2, 3]);
    }

    #[test]
    fn test_add_node_to_sub_cluster() {
        let mut sm = ManagementStateMachine::new();
        let cluster_id = Uuid::new_v4();

        // Create cluster first
        sm.apply(&ManagementCommand::CreateSubCluster {
            cluster_id,
            node_ids: vec![1, 2],
        })
        .unwrap();

        // Add node
        let cmd = ManagementCommand::AddNodeToSubCluster {
            cluster_id,
            node_id: 3,
        };

        let events = sm.apply(&cmd).unwrap();
        assert_eq!(events.len(), 1);

        let metadata = sm.get_sub_cluster(&cluster_id).unwrap();
        assert_eq!(metadata.node_ids, vec![1, 2, 3]);
    }

    #[test]
    fn test_set_metadata() {
        let mut sm = ManagementStateMachine::new();
        let cluster_id = Uuid::new_v4();

        // Create cluster first
        sm.apply(&ManagementCommand::CreateSubCluster {
            cluster_id,
            node_ids: vec![1],
        })
        .unwrap();

        // Set metadata
        let cmd = ManagementCommand::SetMetadata {
            cluster_id,
            key: "type".to_string(),
            value: "kv".to_string(),
        };

        sm.apply(&cmd).unwrap();

        let metadata = sm.get_sub_cluster(&cluster_id).unwrap();
        assert_eq!(metadata.metadata.get("type"), Some(&"kv".to_string()));
    }
}
