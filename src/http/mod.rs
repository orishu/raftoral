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
pub mod bootstrap;

// Native (non-WASM) HTTP client using reqwest
#[cfg(not(target_arch = "wasm32"))]
pub mod client;

// WASM HTTP client using browser fetch API
#[cfg(target_arch = "wasm32")]
pub mod wasm_client;

pub use bootstrap::{discover_peer, discover_peers, next_node_id, DiscoveredPeer};

#[cfg(not(target_arch = "wasm32"))]
pub use client::HttpMessageSender;

#[cfg(target_arch = "wasm32")]
pub use wasm_client::WasmHttpClient as HttpMessageSender;

pub use server::HttpServer;
pub use transport::HttpTransport;
