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
pub mod proto {
    tonic::include_proto!("raftoral");

    /// File descriptor set for gRPC reflection
    pub const FILE_DESCRIPTOR_SET: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/descriptor.bin"));
}

pub mod bootstrap;
pub mod client;
pub mod server;

pub use bootstrap::{discover_peer, discover_peers, next_node_id, DiscoveredPeer};
pub use client::GrpcMessageSender;
pub use server::GrpcServer;
