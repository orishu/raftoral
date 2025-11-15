//! HTTP Layer for Raftoral
//!
//! This module provides HTTP-based transport for Raft consensus, enabling:
//! - REST API for Raft messages
//! - JSON-encoded protobuf messages for debugging
//! - CORS support for browser environments
//! - WebAssembly compatibility
//!
//! The HTTP layer is equivalent to the gRPC layer but uses standard HTTP/REST instead.

pub mod messages;
pub mod transport;
pub mod server;
pub mod client;
pub mod bootstrap;

pub use bootstrap::{discover_peer, discover_peers, next_node_id, DiscoveredPeer};
pub use client::HttpMessageSender;
pub use server::HttpServer;
pub use transport::HttpTransport;
