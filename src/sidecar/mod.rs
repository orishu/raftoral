//! Sidecar Communication Layer
//!
//! This module provides the streaming gRPC server that enables applications
//! to communicate with the Raftoral sidecar for workflow execution.
//!
//! - `proto`: Generated protobuf code for sidecar↔app communication
//! - `server`: Streaming gRPC server implementation
//! - `stream_manager`: Manages active app connection streams
//! - `workflow_proxy`: Bridges between Raft workflow execution and gRPC streams

// Include generated sidecar protobuf code
pub mod proto {
    tonic::include_proto!("raftoral.sidecar");
}

pub mod server;

pub use server::SidecarServer;
