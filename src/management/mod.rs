//! Management Runtime (Layer 7)
//!
//! This module provides a high-level runtime for managing sub-clusters.
//! The management cluster maintains metadata about execution clusters and
//! routes operations to the appropriate cluster.

pub mod event;
pub mod runtime;
pub mod state_machine;

pub use event::ManagementEvent;
pub use runtime::ManagementRuntime;
pub use state_machine::ManagementStateMachine;
