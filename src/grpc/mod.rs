//! gRPC Layer (Layer 0) for generic architecture
//!
//! This module provides gRPC client and server implementations that work with
//! the generic Raft architecture. Unlike the original grpc module, this is
//! designed specifically for the clean layered architecture.
//!
//! - `client`: gRPC client implementing MessageSender trait
//! - `server`: gRPC server for receiving Raft messages
//! - `bootstrap`: Bootstrap functionality for discovering and joining clusters

// Include generated protobuf code
#[cfg(not(target_arch = "wasm32"))]
pub mod proto {
    tonic::include_proto!("raftoral");

    /// File descriptor set for gRPC reflection
    pub const FILE_DESCRIPTOR_SET: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/descriptor.bin"));
}

// For WASM, include proto without tonic
#[cfg(target_arch = "wasm32")]
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/raftoral.rs"));
}

#[cfg(not(target_arch = "wasm32"))]
pub mod bootstrap;
#[cfg(not(target_arch = "wasm32"))]
pub mod client;
#[cfg(not(target_arch = "wasm32"))]
pub mod server;

#[cfg(not(target_arch = "wasm32"))]
pub use bootstrap::{discover_peer, discover_peers, next_node_id, DiscoveredPeer};
#[cfg(not(target_arch = "wasm32"))]
pub use client::GrpcMessageSender;
#[cfg(not(target_arch = "wasm32"))]
pub use server::GrpcServer;
