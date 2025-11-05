//! gRPC Layer (Layer 0) for generic2 architecture
//!
//! This module provides gRPC client and server implementations that work with
//! the generic2 Raft architecture. Unlike the original grpc module, this is
//! designed specifically for the clean layered architecture.
//!
//! - `client`: gRPC client implementing MessageSender trait
//! - `server`: gRPC server for receiving Raft messages

pub mod client;
pub mod server;

pub use client::GrpcMessageSender;
pub use server::GrpcServer;

// Re-export proto definitions
pub use crate::grpc::server::raft_proto;
