//! Key-Value Store Runtime (Layer 7)
//!
//! This module provides a high-level runtime for a distributed key-value store
//! built on the generic Raft infrastructure.

pub mod runtime;

pub use runtime::KvRuntime;
