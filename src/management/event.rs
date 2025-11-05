//! Management events emitted by the management cluster

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Events emitted by the ManagementStateMachine
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ManagementEvent {
    /// A sub-cluster was created
    SubClusterCreated {
        cluster_id: Uuid,
        node_ids: Vec<u64>,
    },

    /// A sub-cluster was deleted
    SubClusterDeleted {
        cluster_id: Uuid,
    },

    /// A node was added to a sub-cluster
    NodeAddedToSubCluster {
        cluster_id: Uuid,
        node_id: u64,
    },

    /// A node was removed from a sub-cluster
    NodeRemovedFromSubCluster {
        cluster_id: Uuid,
        node_id: u64,
    },

    /// Metadata was updated for a sub-cluster
    MetadataUpdated {
        cluster_id: Uuid,
        key: String,
        value: Option<String>, // None = deleted
    },
}
