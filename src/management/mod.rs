//! Management Runtime (Layer 7)
//!
//! This module provides a high-level runtime for managing sub-clusters.
//! The management cluster maintains metadata about execution clusters and
//! routes operations to the appropriate cluster.

pub mod cluster_manager;
pub mod config;
pub mod event;
pub mod runtime;
pub mod state_machine;
pub mod sub_cluster_runtime;

pub use cluster_manager::{ClusterAction, ClusterManager, ClusterManagerConfig};
pub use config::{ManagementClusterConfig, VoterSelectionStrategy};
pub use event::ManagementEvent;
pub use runtime::ManagementRuntime;
pub use state_machine::ManagementStateMachine;
pub use sub_cluster_runtime::SubClusterRuntime;
