//! Management State Machine (Layer 4)
//!
//! Maintains metadata about sub-clusters. The actual sub-cluster Raft nodes
//! are managed by the ManagementRuntime (Layer 7), while this state machine
//! just tracks the metadata.
//!
//! Cluster IDs are assigned sequentially starting from 1 (0 is reserved for the management cluster).

use crate::management::ManagementEvent;
use crate::raft::generic::StateMachine;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Commands for the management state machine
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum ManagementCommand {
    /// Create a new sub-cluster (cluster_id assigned sequentially by state machine)
    CreateSubCluster {
        node_ids: Vec<u64>,
    },

    /// Delete a sub-cluster
    DeleteSubCluster {
        cluster_id: u32,
    },

    /// Add a node to a sub-cluster
    AddNodeToSubCluster {
        cluster_id: u32,
        node_id: u64,
    },

    /// Remove a node from a sub-cluster
    RemoveNodeFromSubCluster {
        cluster_id: u32,
        node_id: u64,
    },

    /// Set metadata for a sub-cluster
    SetMetadata {
        cluster_id: u32,
        key: String,
        value: String,
    },

    /// Delete metadata for a sub-cluster
    DeleteMetadata {
        cluster_id: u32,
        key: String,
    },

    /// Register a node's network address (for later use when adding to sub-clusters)
    RegisterNodeAddress {
        node_id: u64,
        address: String,
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
#[derive(Debug, Clone)]
pub struct ManagementStateMachine {
    /// Sub-cluster ID → metadata
    sub_clusters: HashMap<u32, SubClusterMetadata>,

    /// Next cluster ID to assign (starts at 1, 0 is reserved for management cluster)
    next_cluster_id: u32,

    /// Node ID → network address mapping (for adding nodes to sub-clusters)
    node_addresses: HashMap<u64, String>,
}

impl ManagementStateMachine {
    /// Create a new management state machine
    pub fn new() -> Self {
        Self {
            sub_clusters: HashMap::new(),
            next_cluster_id: 1, // Start at 1, 0 is reserved for management
            node_addresses: HashMap::new(),
        }
    }

    /// Get metadata for a sub-cluster
    pub fn get_sub_cluster(&self, cluster_id: &u32) -> Option<&SubClusterMetadata> {
        self.sub_clusters.get(cluster_id)
    }

    /// List all sub-cluster IDs
    pub fn list_sub_clusters(&self) -> Vec<u32> {
        self.sub_clusters.keys().copied().collect()
    }

    /// Get all sub-cluster metadata
    pub fn get_all_sub_clusters(&self) -> &HashMap<u32, SubClusterMetadata> {
        &self.sub_clusters
    }

    /// Get a node's network address
    pub fn get_node_address(&self, node_id: &u64) -> Option<&String> {
        self.node_addresses.get(node_id)
    }

    /// Get mutable access to all sub-cluster metadata (for testing)
    #[cfg(test)]
    pub fn get_all_sub_clusters_mut(&mut self) -> &mut HashMap<u32, SubClusterMetadata> {
        &mut self.sub_clusters
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
            ManagementCommand::CreateSubCluster { node_ids } => {
                // Auto-assign sequential cluster ID
                let cluster_id = self.next_cluster_id;
                self.next_cluster_id += 1;

                self.sub_clusters.insert(
                    cluster_id,
                    SubClusterMetadata {
                        node_ids: node_ids.clone(),
                        metadata: HashMap::new(),
                    },
                );
                Ok(vec![ManagementEvent::SubClusterCreated {
                    cluster_id,
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

            ManagementCommand::RegisterNodeAddress { node_id, address } => {
                self.node_addresses.insert(*node_id, address.clone());
                // No event emitted for address registration (internal bookkeeping only)
                Ok(vec![])
            }
        }
    }

    fn snapshot(&self) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        #[derive(Serialize)]
        struct Snapshot {
            sub_clusters: HashMap<u32, SubClusterMetadata>,
            next_cluster_id: u32,
            node_addresses: HashMap<u64, String>,
        }

        let snapshot = Snapshot {
            sub_clusters: self.sub_clusters.clone(),
            next_cluster_id: self.next_cluster_id,
            node_addresses: self.node_addresses.clone(),
        };

        let snapshot_data = serde_json::to_vec(&snapshot)?;
        Ok(snapshot_data)
    }

    fn restore(&mut self, snapshot: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        #[derive(Deserialize)]
        struct Snapshot {
            sub_clusters: HashMap<u32, SubClusterMetadata>,
            next_cluster_id: u32,
            #[serde(default)]
            node_addresses: HashMap<u64, String>,
        }

        let snapshot: Snapshot = serde_json::from_slice(snapshot)?;
        self.sub_clusters = snapshot.sub_clusters;
        self.next_cluster_id = snapshot.next_cluster_id;
        self.node_addresses = snapshot.node_addresses;
        Ok(())
    }

    fn on_follower_failed(&mut self, node_id: u64) -> Vec<Self::Event> {
        // Emit event for failed node detection
        // The ManagementRuntime will handle this by removing the node from
        // all sub-clusters and then from the management cluster
        vec![ManagementEvent::FailedNodeDetected { node_id }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_sub_cluster() {
        let mut sm = ManagementStateMachine::new();

        let cmd = ManagementCommand::CreateSubCluster {
            node_ids: vec![1, 2, 3],
        };

        let events = sm.apply(&cmd).unwrap();
        assert_eq!(events.len(), 1);

        // First cluster should be assigned ID 1
        if let ManagementEvent::SubClusterCreated { cluster_id, node_ids } = &events[0] {
            assert_eq!(*cluster_id, 1);
            assert_eq!(node_ids, &vec![1, 2, 3]);
        } else {
            panic!("Expected SubClusterCreated event");
        }

        let metadata = sm.get_sub_cluster(&1).unwrap();
        assert_eq!(metadata.node_ids, vec![1, 2, 3]);
    }

    #[test]
    fn test_add_node_to_sub_cluster() {
        let mut sm = ManagementStateMachine::new();

        // Create cluster first (will get ID 1)
        sm.apply(&ManagementCommand::CreateSubCluster {
            node_ids: vec![1, 2],
        })
        .unwrap();

        let cluster_id = 1;

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

        // Create cluster first (will get ID 1)
        sm.apply(&ManagementCommand::CreateSubCluster {
            node_ids: vec![1],
        })
        .unwrap();

        let cluster_id = 1;

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
