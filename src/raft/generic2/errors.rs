//! Error types for the generic Raft infrastructure

use std::fmt;

/// Errors that can occur during transport operations
#[derive(Debug, Clone)]
pub enum TransportError {
    /// Peer node not found in registry
    PeerNotFound { node_id: u64 },

    /// Failed to send message to peer
    SendFailed { node_id: u64, reason: String },

    /// Failed to serialize message
    SerializationError { reason: String },

    /// Failed to deserialize message
    DeserializationError { reason: String },

    /// Transport is not initialized
    NotInitialized,

    /// Other transport error
    Other(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::PeerNotFound { node_id } => {
                write!(f, "Peer node {} not found in registry", node_id)
            }
            TransportError::SendFailed { node_id, reason } => {
                write!(f, "Failed to send message to node {}: {}", node_id, reason)
            }
            TransportError::SerializationError { reason } => {
                write!(f, "Failed to serialize message: {}", reason)
            }
            TransportError::DeserializationError { reason } => {
                write!(f, "Failed to deserialize message: {}", reason)
            }
            TransportError::NotInitialized => {
                write!(f, "Transport is not initialized")
            }
            TransportError::Other(msg) => write!(f, "Transport error: {}", msg),
        }
    }
}

impl std::error::Error for TransportError {}

/// Errors that can occur during message routing
#[derive(Debug, Clone)]
pub enum RoutingError {
    /// Cluster not found in router
    ClusterNotFound { cluster_id: u32 },

    /// Failed to route message
    RouteFailed { cluster_id: u32, reason: String },

    /// Mailbox full (backpressure)
    MailboxFull { cluster_id: u32 },

    /// Other routing error
    Other(String),
}

impl fmt::Display for RoutingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RoutingError::ClusterNotFound { cluster_id } => {
                write!(f, "Cluster {} not found in router", cluster_id)
            }
            RoutingError::RouteFailed { cluster_id, reason } => {
                write!(f, "Failed to route message to cluster {}: {}", cluster_id, reason)
            }
            RoutingError::MailboxFull { cluster_id } => {
                write!(f, "Mailbox full for cluster {}", cluster_id)
            }
            RoutingError::Other(msg) => write!(f, "Routing error: {}", msg),
        }
    }
}

impl std::error::Error for RoutingError {}
