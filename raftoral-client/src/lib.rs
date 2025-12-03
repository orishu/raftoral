//! Raftoral Client SDK
//!
//! Client library for communicating with Raftoral sidecar containers.
//! Provides ergonomic API for workflow execution with checkpoint support.

pub mod client;
pub mod context;
pub mod error;

// Include generated protobuf code
pub mod proto {
    tonic::include_proto!("raftoral.sidecar");
}

pub use client::RaftoralClient;
pub use context::WorkflowContext;
pub use error::{Error, Result};

/// Re-export commonly used types
pub use proto::{ExecuteWorkflowRequest, WorkflowResult};
