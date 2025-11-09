//! Generic Raft infrastructure (V2 Architecture)
//!
//! This module implements a clean layered architecture for Raft-based systems:
//! - Layer 0: Server (protocol implementations: gRPC, HTTP, InProcess)
//! - Layer 1: Transport (protocol-agnostic message sending/receiving)
//! - Layer 2: Cluster Router (routes messages by cluster_id)
//! - Layer 3: Raft Node (Raft consensus)
//! - Layer 4: State Machine (applies commands, emits events)
//! - Layer 5: Event Bus (decouples state changes from subscribers)
//! - Layer 6: Proposal Router (routes proposals to leader)
//! - Layer 7: Application Runtime (management/workflow logic)

pub mod cluster_router;
pub mod errors;
pub mod event_bus;
pub mod integration_tests;
pub mod node;
pub mod proposal_router;
pub mod server;
pub mod state_machine;
pub mod transport;

#[cfg(feature = "persistent-storage")]
pub mod rocksdb_storage;

pub use cluster_router::ClusterRouter;
pub use errors::{TransportError, RoutingError};
pub use event_bus::EventBus;
pub use node::{RaftNode, RaftNodeConfig, NodeMetadata, RoleChange};
pub use proposal_router::{ProposalRouter, ProposalError};
pub use state_machine::{StateMachine, KvStateMachine, KvCommand, KvEvent};
pub use transport::{Transport, TransportLayer, MessageSender};
pub use server::{InProcessServer, InProcessMessageSender, InProcessNetwork, InProcessNetworkSender};

// Re-export KvRuntime from the kv module (Layer 7)
pub use crate::kv::KvRuntime;
