// Custom storage implementation that extends MemStorage with snapshot data persistence
//
// MemStorage from raft-rs only stores snapshot metadata, not the actual data.
// This is intentional for production use where snapshot data is stored separately (disk/S3).
// For our in-memory testing and potential future disk-based storage, we need to persist
// the snapshot data as well.

use raft::prelude::*;
use raft::storage::{MemStorage, MemStorageCore};
use raft::{Error, Result, StorageError};
use raft::GetEntriesContext;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Storage implementation that extends MemStorage with snapshot data persistence.
///
/// This storage wraps raft-rs's MemStorage and adds the ability to store and retrieve
/// snapshot data. MemStorage only stores metadata (index, term, conf_state) but discards
/// the actual snapshot data bytes, which makes it unsuitable for testing or scenarios
/// where snapshots need to be transmitted between nodes.
///
/// # Usage
///
/// ```no_run
/// use raftoral::raft::generic::storage::MemStorageWithSnapshot;
///
/// let storage = MemStorageWithSnapshot::new();
/// // Use like normal MemStorage, but snapshots will include data
/// ```
#[derive(Clone, Default)]
pub struct MemStorageWithSnapshot {
    /// The underlying MemStorage that handles log entries and metadata
    inner: MemStorage,
    /// Stores the actual snapshot data that MemStorage discards
    snapshot_data: Arc<RwLock<bytes::Bytes>>,
}

impl MemStorageWithSnapshot {
    /// Creates a new MemStorageWithSnapshot instance.
    pub fn new() -> Self {
        Self {
            inner: MemStorage::new(),
            snapshot_data: Arc::new(RwLock::new(bytes::Bytes::new())),
        }
    }

    /// Creates a new MemStorageWithSnapshot with an initial configuration.
    pub fn new_with_conf_state<T>(conf_state: T) -> Self
    where
        ConfState: From<T>,
    {
        Self {
            inner: MemStorage::new_with_conf_state(conf_state),
            snapshot_data: Arc::new(RwLock::new(bytes::Bytes::new())),
        }
    }

    /// Get a read lock on the inner MemStorage core
    pub fn rl(&self) -> RwLockReadGuard<'_, MemStorageCore> {
        self.inner.rl()
    }

    /// Get a write lock on the inner MemStorage core
    pub fn wl(&self) -> RwLockWriteGuard<'_, MemStorageCore> {
        self.inner.wl()
    }

    /// Apply a snapshot, storing both metadata (via MemStorage) and data (in our extension)
    pub fn apply_snapshot_with_data(&self, snapshot: Snapshot) -> Result<()> {
        // Extract and store the data before MemStorage discards it
        let data = snapshot.get_data().to_vec();
        *self.snapshot_data.write().unwrap() = bytes::Bytes::from(data);

        // Let MemStorage handle the metadata
        self.wl().apply_snapshot(snapshot)?;

        Ok(())
    }

    /// Get the current snapshot with data included
    pub fn snapshot_with_data(&self, request_index: u64, _to: u64) -> Result<Snapshot> {
        // Build snapshot metadata from current state
        let raft_state = self.inner.initial_state()?;
        let core = self.rl();

        let mut snapshot = Snapshot::default();
        let meta = snapshot.mut_metadata();
        meta.index = core.hard_state().commit;
        meta.term = core.hard_state().term;
        meta.set_conf_state(raft_state.conf_state);

        // Add our stored data
        let data = self.snapshot_data.read().unwrap().clone();
        snapshot.set_data(data);

        // Verify the snapshot meets the request_index requirement
        if snapshot.get_metadata().index < request_index {
            return Err(Error::Store(StorageError::SnapshotOutOfDate));
        }

        Ok(snapshot)
    }
}

/// Implement the Storage trait by delegating to MemStorage and adding snapshot data
impl Storage for MemStorageWithSnapshot {
    fn initial_state(&self) -> Result<RaftState> {
        self.inner.initial_state()
    }

    fn entries(
        &self,
        low: u64,
        high: u64,
        max_size: impl Into<Option<u64>>,
        context: GetEntriesContext,
    ) -> Result<Vec<Entry>> {
        self.inner.entries(low, high, max_size, context)
    }

    fn term(&self, idx: u64) -> Result<u64> {
        self.inner.term(idx)
    }

    fn first_index(&self) -> Result<u64> {
        self.inner.first_index()
    }

    fn last_index(&self) -> Result<u64> {
        self.inner.last_index()
    }

    fn snapshot(&self, request_index: u64, to: u64) -> Result<Snapshot> {
        // Use our enhanced version that includes data
        self.snapshot_with_data(request_index, to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_data_persistence() {
        let storage = MemStorageWithSnapshot::new();

        // Create a snapshot with data
        let mut snapshot = Snapshot::default();
        let test_data = b"test snapshot data".to_vec();
        snapshot.set_data(test_data.clone().into());

        let mut meta = SnapshotMetadata::default();
        meta.index = 100;
        meta.term = 5;
        snapshot.set_metadata(meta);

        // Apply the snapshot
        storage.apply_snapshot_with_data(snapshot.clone()).expect("Should apply snapshot");

        // Retrieve the snapshot
        let retrieved = storage.snapshot_with_data(0, 0).expect("Should get snapshot");

        // Verify data is preserved
        assert_eq!(retrieved.get_data(), test_data.as_slice());
        assert_eq!(retrieved.get_metadata().index, 100);
        assert_eq!(retrieved.get_metadata().term, 5);
    }

    #[test]
    fn test_snapshot_request_index_check() {
        let storage = MemStorageWithSnapshot::new();

        // Create a snapshot at index 100
        let mut snapshot = Snapshot::default();
        snapshot.set_data(b"data".to_vec().into());

        let mut meta = SnapshotMetadata::default();
        meta.index = 100;
        meta.term = 5;
        snapshot.set_metadata(meta);

        storage.apply_snapshot_with_data(snapshot).expect("Should apply snapshot");

        // Request with lower index should succeed
        let result = storage.snapshot_with_data(50, 0);
        assert!(result.is_ok());

        // Request with higher index should fail (snapshot is out of date)
        let result = storage.snapshot_with_data(150, 0);
        assert!(result.is_err());
    }
}
