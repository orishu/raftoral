//! Error types for Raftoral client

use thiserror::Error;

/// Result type for Raftoral client operations
pub type Result<T> = std::result::Result<T, Error>;

/// Errors that can occur when using the Raftoral client
#[derive(Debug, Error)]
pub enum Error {
    /// gRPC connection error
    #[error("gRPC error: {0}")]
    Grpc(#[from] tonic::Status),

    /// Transport error
    #[error("Transport error: {0}")]
    Transport(#[from] tonic::transport::Error),

    /// Invalid URI error
    #[error("Invalid URI: {0}")]
    InvalidUri(#[from] http::uri::InvalidUri),

    /// Serialization error
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Workflow execution error
    #[error("Workflow error: {0}")]
    Workflow(String),

    /// Timeout error
    #[error("Timeout waiting for {0}")]
    Timeout(String),

    /// Connection closed
    #[error("Connection to sidecar closed")]
    ConnectionClosed,

    /// Invalid state
    #[error("Invalid state: {0}")]
    InvalidState(String),
}
