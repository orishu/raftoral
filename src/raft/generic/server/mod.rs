//! Server layer (Layer 0) - Protocol implementations
//!
//! This module contains different server implementations:
//! - `in_process`: In-memory message routing for testing (callback-based, legacy)
//! - `network`: In-memory message bus for testing (async-native, preferred)
//! - `grpc`: gRPC server for production (future)
//! - `http`: HTTP server with protobuf payloads (future)

pub mod in_process;
pub mod network;

pub use in_process::{InProcessServer, InProcessMessageSender};
pub use network::{InProcessNetwork, InProcessNetworkSender};
