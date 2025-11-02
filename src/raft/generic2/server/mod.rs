//! Server layer (Layer 0) - Protocol implementations
//!
//! This module contains different server implementations:
//! - `in_process`: In-memory message routing for testing (no network)
//! - `grpc`: gRPC server for production (future)
//! - `http`: HTTP server with protobuf payloads (future)

pub mod in_process;

pub use in_process::{InProcessServer, InProcessMessageSender};
