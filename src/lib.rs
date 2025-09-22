pub mod raft;

// Type alias for our workflow-specific cluster
pub type WorkflowCluster = raft::RaftCluster<raft::RaftCommand>;

pub use raft::{RaftCluster, PlaceholderCommand, RaftCommand, RaftCommandType};