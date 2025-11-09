//! Management events emitted by the management cluster

use serde::{Deserialize, Serialize};

/// Events emitted by the ManagementStateMachine
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ManagementEvent {
    /// A sub-cluster was created
    SubClusterCreated {
        cluster_id: u32,
        node_ids: Vec<u64>,
    },

    /// A sub-cluster was deleted
    SubClusterDeleted {
        cluster_id: u32,
    },

    /// A node was added to a sub-cluster
    NodeAddedToSubCluster {
        cluster_id: u32,
        node_id: u64,
    },

    /// A node was removed from a sub-cluster
    NodeRemovedFromSubCluster {
        cluster_id: u32,
        node_id: u64,
    },

    /// Metadata was updated for a sub-cluster
    MetadataUpdated {
        cluster_id: u32,
        key: String,
        value: Option<String>, // None = deleted
    },

    /// A node has been detected as failed (no progress for configured timeout)
    ///
    /// This event is emitted by the RaftNode's failure detection when it detects
    /// that a follower node hasn't made progress within the configured timeout.
    /// The ManagementRuntime should handle this by removing the node from all
    /// sub-clusters and then from the management cluster itself.
    FailedNodeDetected {
        node_id: u64,
    },
}
