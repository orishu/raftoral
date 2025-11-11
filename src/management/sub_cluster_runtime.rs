//! Sub-cluster Runtime Trait
//!
//! Defines the interface that all sub-cluster runtimes must implement
//! to enable dynamic construction by the ManagementRuntime.

use crate::raft::generic2::{RaftNode, RaftNodeConfig, Transport};
use slog::Logger;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, oneshot};

/// Trait for sub-cluster runtimes that can be dynamically constructed
/// by the ManagementRuntime when sub-clusters are created.
///
/// This trait allows ManagementRuntime<R> to create instances of R
/// without knowing the concrete type at compile time.
pub trait SubClusterRuntime: Send + Sync + 'static {
    /// The state machine type used by this runtime
    type StateMachine: crate::raft::generic2::StateMachine + Send + Sync + 'static;

    /// Shared configuration type for this runtime (e.g., WorkflowRegistry for WorkflowRuntime)
    /// This allows sharing configuration/state across multiple runtime instances of the same type
    type SharedConfig: Send + Sync + 'static;

    /// Create a new single-node cluster runtime
    ///
    /// This is called when a sub-cluster is created and this node is the first node.
    ///
    /// # Arguments
    /// * `config` - Raft node configuration
    /// * `transport` - Transport layer for network communication
    /// * `mailbox_rx` - Mailbox receiver for Raft messages
    /// * `shared_config` - Shared configuration for this runtime type
    /// * `logger` - Logger instance
    ///
    /// # Returns
    /// A tuple of (Runtime, RaftNode handle)
    fn new_single_node(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc2::proto::GenericMessage>,
        shared_config: Arc<Mutex<Self::SharedConfig>>,
        logger: Logger,
    ) -> Result<
        (Self, Arc<Mutex<RaftNode<Self::StateMachine>>>),
        Box<dyn std::error::Error>,
    >
    where
        Self: Sized;

    /// Create a new runtime for a node joining an existing cluster
    ///
    /// This is called when a node is added to an existing sub-cluster.
    ///
    /// # Arguments
    /// * `config` - Raft node configuration
    /// * `transport` - Transport layer for network communication
    /// * `mailbox_rx` - Mailbox receiver for Raft messages
    /// * `initial_voters` - IDs of all nodes in the cluster (including this node)
    /// * `shared_config` - Shared configuration for this runtime type
    /// * `logger` - Logger instance
    ///
    /// # Returns
    /// A tuple of (Runtime, RaftNode handle)
    fn new_joining_node(
        config: RaftNodeConfig,
        transport: Arc<dyn Transport>,
        mailbox_rx: mpsc::Receiver<crate::grpc2::proto::GenericMessage>,
        initial_voters: Vec<u64>,
        shared_config: Arc<Mutex<Self::SharedConfig>>,
        logger: Logger,
    ) -> Result<
        (Self, Arc<Mutex<RaftNode<Self::StateMachine>>>),
        Box<dyn std::error::Error>,
    >
    where
        Self: Sized;

    /// Add a node to the cluster
    ///
    /// # Arguments
    /// * `node_id` - ID of the node to add
    /// * `address` - Network address of the node
    ///
    /// # Returns
    /// Receiver that will be notified when the configuration change completes
    fn add_node(
        &self,
        node_id: u64,
        address: String,
    ) -> impl std::future::Future<Output = Result<oneshot::Receiver<Result<(), String>>, String>> + Send;

    /// Remove a node from the cluster
    ///
    /// # Arguments
    /// * `node_id` - ID of the node to remove
    ///
    /// # Returns
    /// Receiver that will be notified when the configuration change completes
    fn remove_node(
        &self,
        node_id: u64,
    ) -> impl std::future::Future<Output = Result<oneshot::Receiver<Result<(), String>>, String>> + Send;

    /// Get the node ID
    fn node_id(&self) -> u64;

    /// Get the cluster ID
    fn cluster_id(&self) -> u32;
}
