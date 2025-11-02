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
pub mod server;
pub mod transport;

pub use cluster_router::ClusterRouter;
pub use errors::{TransportError, RoutingError};
pub use transport::{Transport, TransportLayer, MessageSender};
pub use server::{InProcessServer, InProcessMessageSender};
