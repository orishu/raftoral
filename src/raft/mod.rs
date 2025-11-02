pub mod generic;
pub mod generic2;  // V2 architecture
pub mod command;

// Re-export the generic infrastructure
pub use generic::{RaftCluster, RaftNode, RoleChange};

// Export our workflow-specific types
pub use command::{RaftCommand, PlaceholderCommand};