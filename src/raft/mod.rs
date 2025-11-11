pub mod generic;  // Generic Raft implementation
pub mod command;

// Re-export command types
pub use command::{RaftCommand, PlaceholderCommand};
