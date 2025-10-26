///! Management commands for controlling execution cluster membership
///! and tracking workflow lifecycle across the deployment.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Commands for the management cluster
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ManagementCommand {
    /// Create a new virtual execution cluster
    CreateExecutionCluster(CreateExecutionClusterData),

    /// Destroy an execution cluster (must have no active nodes)
    DestroyExecutionCluster(ExecutionClusterId),

    /// Associate a node with an execution cluster
    AssociateNode(AssociateNodeData),

    /// Disassociate a node from an execution cluster
    DisassociateNode(DisassociateNodeData),

    /// Change a node's role (voter/learner) in the management cluster
    ChangeNodeRole(ChangeNodeRoleData),
}

pub type ExecutionClusterId = Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateExecutionClusterData {
    pub cluster_id: Uuid,
    pub initial_node_ids: Vec<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssociateNodeData {
    pub cluster_id: Uuid,
    pub node_id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DisassociateNodeData {
    pub cluster_id: Uuid,
    pub node_id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangeNodeRoleData {
    pub node_id: u64,
    pub is_voter: bool,
}
