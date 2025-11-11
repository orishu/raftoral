//! Storage abstraction for Raft
//!
//! Provides a unified interface for both persistent (RocksDB) and in-memory storage.
//! When the `persistent-storage` feature is enabled, RaftStorage can be either variant.
//! When disabled, it's just a type alias to MemStorageWithSnapshot.

use raft::prelude::*;
use raft::{Storage, GetEntriesContext};
use raft::storage::MemStorage;

/// Type alias for MemStorage from raft-rs (in-memory storage with snapshot support)
pub type MemStorageWithSnapshot = MemStorage;

#[cfg(feature = "persistent-storage")]
use crate::raft::generic::rocksdb_storage::RocksDBStorage;

/// Storage type that can be either RocksDB (persistent) or in-memory
///
/// When `persistent-storage` feature is enabled, this enum allows choosing
/// between persistent RocksDB storage or in-memory storage at runtime.
#[cfg(feature = "persistent-storage")]
#[derive(Clone)]
pub enum RaftStorage {
    RocksDB(RocksDBStorage),
    Memory(MemStorageWithSnapshot),
}

#[cfg(feature = "persistent-storage")]
impl Storage for RaftStorage {
    fn initial_state(&self) -> raft::Result<RaftState> {
        match self {
            RaftStorage::RocksDB(s) => s.initial_state(),
            RaftStorage::Memory(s) => s.initial_state(),
        }
    }

    fn entries(&self, low: u64, high: u64, max_size: impl Into<Option<u64>>, context: GetEntriesContext) -> raft::Result<Vec<Entry>> {
        match self {
            RaftStorage::RocksDB(s) => s.entries(low, high, max_size, context),
            RaftStorage::Memory(s) => s.entries(low, high, max_size, context),
        }
    }

    fn term(&self, idx: u64) -> raft::Result<u64> {
        match self {
            RaftStorage::RocksDB(s) => s.term(idx),
            RaftStorage::Memory(s) => s.term(idx),
        }
    }

    fn first_index(&self) -> raft::Result<u64> {
        match self {
            RaftStorage::RocksDB(s) => s.first_index(),
            RaftStorage::Memory(s) => s.first_index(),
        }
    }

    fn last_index(&self) -> raft::Result<u64> {
        match self {
            RaftStorage::RocksDB(s) => s.last_index(),
            RaftStorage::Memory(s) => s.last_index(),
        }
    }

    fn snapshot(&self, request_index: u64, to: u64) -> raft::Result<Snapshot> {
        match self {
            RaftStorage::RocksDB(s) => s.snapshot(request_index, to),
            RaftStorage::Memory(s) => s.snapshot(request_index, to),
        }
    }
}

#[cfg(feature = "persistent-storage")]
impl RaftStorage {
    /// Compatibility method for write lock (no-op for enum, delegates to inner types)
    pub fn wl(&self) -> &Self {
        self
    }

    /// Compatibility method for read lock (no-op for enum, delegates to inner types)
    pub fn rl(&self) -> &Self {
        self
    }

    /// Apply snapshot with data
    pub fn apply_snapshot_with_data(&self, snapshot: Snapshot) -> raft::Result<()> {
        match self {
            RaftStorage::RocksDB(s) => s.apply_snapshot_with_data(snapshot),
            RaftStorage::Memory(s) => {
                // MemStorage doesn't have apply_snapshot_with_data, use apply_snapshot instead
                s.wl().apply_snapshot(snapshot)
            }
        }
    }

    /// Set hard state
    pub fn set_hardstate(&self, hs: HardState) -> raft::Result<()> {
        match self {
            RaftStorage::RocksDB(s) => s.set_hardstate(hs),
            RaftStorage::Memory(s) => {
                s.wl().mut_hard_state().clone_from(&hs);
                Ok(())
            }
        }
    }

    /// Update commit index
    pub fn update_commit(&self, commit: u64) -> raft::Result<()> {
        match self {
            RaftStorage::RocksDB(s) => s.update_commit(commit),
            RaftStorage::Memory(s) => {
                s.wl().mut_hard_state().set_commit(commit);
                Ok(())
            }
        }
    }

    /// Append entries to the log
    pub fn append(&self, entries: &[Entry]) -> raft::Result<()> {
        match self {
            RaftStorage::RocksDB(s) => s.append(entries),
            RaftStorage::Memory(s) => s.wl().append(entries),
        }
    }

    /// Get hard state (for MemStorage, needs read lock)
    pub fn hard_state(&self) -> HardState {
        match self {
            RaftStorage::RocksDB(s) => s.load_hard_state().unwrap_or_default(),
            RaftStorage::Memory(s) => s.rl().hard_state().clone(),
        }
    }
}

/// When persistent-storage is disabled, RaftStorage is just an alias
#[cfg(not(feature = "persistent-storage"))]
pub type RaftStorage = MemStorageWithSnapshot;
