//! RocksDB-backed Raft Storage Implementation
//!
//! Provides persistent storage for Raft log entries, hard state, configuration state,
//! and snapshots using RocksDB. This enables node restart and recovery.
//!
//! # Schema Design
//!
//! **Entries Column Family:**
//! - Key: `entry_{index}` (u64 as bytes)
//! - Value: Protobuf-serialized Entry
//!
//! **Metadata Column Family:**
//! - `hard_state` → HardState (term, vote, commit)
//! - `conf_state` → ConfState (voters, learners)
//! - `first_index` → u64
//! - `last_index` → u64
//!
//! **Snapshot Column Family:**
//! - `snapshot_data` → Snapshot protobuf bytes
//! - `snapshot_metadata` → SnapshotMetadata

use rocksdb::{ColumnFamilyDescriptor, DB, Options, WriteBatch};
use raft::prelude::*;
use raft::{Error, Result, StorageError};
use raft::GetEntriesContext;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use protobuf::Message as ProtobufMessage;

const CF_ENTRIES: &str = "entries";
const CF_METADATA: &str = "metadata";
const CF_SNAPSHOT: &str = "snapshot";

const KEY_NODE_ID: &[u8] = b"node_id";
const KEY_HARD_STATE: &[u8] = b"hard_state";
const KEY_CONF_STATE: &[u8] = b"conf_state";
const KEY_FIRST_INDEX: &[u8] = b"first_index";
const KEY_LAST_INDEX: &[u8] = b"last_index";
const KEY_SNAPSHOT_DATA: &[u8] = b"snapshot_data";
const KEY_SNAPSHOT_METADATA: &[u8] = b"snapshot_metadata";

/// RocksDB-backed persistent Raft storage
#[derive(Debug)]
pub struct RocksDBStorage {
    /// RocksDB instance
    db: Arc<DB>,

    /// Cached first index (for performance)
    first_index_cache: AtomicU64,

    /// Cached last index (for performance)
    last_index_cache: AtomicU64,

    /// RwLock for coordinating reads and writes
    /// (RocksDB is thread-safe, but we need to coordinate cache updates)
    lock: Arc<RwLock<()>>,
}

impl RocksDBStorage {
    /// Create a new RocksDBStorage instance
    ///
    /// Creates a new database at the given path with the specified initial configuration.
    /// If the database already exists, it will be opened.
    ///
    /// # Arguments
    /// * `path` - Path to the RocksDB database directory
    /// * `node_id` - Node ID to persist (required for restarts)
    /// * `conf_state` - Initial configuration state (voters and learners)
    ///
    /// # Returns
    /// * `Ok(RocksDBStorage)` - Successfully created/opened storage
    /// * `Err(...)` - Failed to create/open database
    pub fn open_or_create<P: AsRef<Path>>(
        path: P,
        node_id: u64,
        conf_state: ConfState,
    ) -> Result<Self> {
        let path = path.as_ref();

        // Configure RocksDB options
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);

        // Define column families
        let cfs = vec![
            ColumnFamilyDescriptor::new(CF_ENTRIES, Options::default()),
            ColumnFamilyDescriptor::new(CF_METADATA, Options::default()),
            ColumnFamilyDescriptor::new(CF_SNAPSHOT, Options::default()),
        ];

        // Open database
        let db = DB::open_cf_descriptors(&opts, path, cfs)
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;

        let storage = Self {
            db: Arc::new(db),
            first_index_cache: AtomicU64::new(0),
            last_index_cache: AtomicU64::new(0),
            lock: Arc::new(RwLock::new(())),
        };

        // Initialize metadata if this is a new database
        storage.initialize_if_needed(node_id, conf_state)?;

        // Load index caches
        storage.reload_index_caches()?;

        Ok(storage)
    }

    /// Check if a RocksDB storage directory exists and contains Raft state
    ///
    /// This helps distinguish between a fresh start and a node restart.
    /// Returns Ok(true) if the directory exists and contains Raft metadata.
    pub fn storage_exists<P: AsRef<Path>>(path: P) -> Result<bool> {
        use rocksdb::Options;

        let path = path.as_ref();

        // Check if directory exists
        if !path.exists() {
            return Ok(false);
        }

        // Try to open the database read-only to check if it's valid
        let opts = Options::default();
        let cfs = vec![CF_ENTRIES, CF_METADATA, CF_SNAPSHOT];

        match rocksdb::DB::open_cf_for_read_only(&opts, path, cfs, false) {
            Ok(db) => {
                // Check if node_id exists in metadata (indicates initialized storage)
                if let Some(cf_metadata) = db.cf_handle(CF_METADATA) {
                    match db.get_cf(cf_metadata, KEY_NODE_ID) {
                        Ok(Some(_)) => Ok(true),  // Node ID exists, this is a restart
                        Ok(None) => Ok(false),     // DB exists but not initialized
                        Err(_) => Ok(false),       // Error reading, treat as non-existent
                    }
                } else {
                    Ok(false)
                }
            }
            Err(_) => Ok(false),  // Can't open, treat as non-existent
        }
    }

    /// Initialize database with default state if it's empty
    fn initialize_if_needed(&self, node_id: u64, conf_state: ConfState) -> Result<()> {
        let _lock = self.lock.write().unwrap();

        let cf_metadata = self.db.cf_handle(CF_METADATA)
            .ok_or_else(|| Error::Store(StorageError::Unavailable))?;

        // Check if already initialized
        if self.db.get_cf(cf_metadata, KEY_HARD_STATE)
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?
            .is_some()
        {
            // Already initialized - verify node ID matches if it exists
            if let Ok(Some(stored_node_id)) = self.load_node_id() {
                if stored_node_id != node_id {
                    return Err(Error::Store(StorageError::Other(
                        format!("Node ID mismatch: storage contains {}, but {} was provided",
                                stored_node_id, node_id).into()
                    )));
                }
            }
            return Ok(());
        }

        // Initialize with node ID
        self.db.put_cf(cf_metadata, KEY_NODE_ID, node_id.to_be_bytes())
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;

        // Initialize with default hard state
        let hard_state = HardState::default();
        self.store_hard_state_locked(&hard_state)?;

        // Initialize with provided conf state
        self.store_conf_state_locked(&conf_state)?;

        // Initialize indices following raft-rs convention:
        // - Empty log has first_index = 1, last_index = 0
        // - This maintains the invariant: first_index > last_index when empty
        self.store_index_locked(KEY_FIRST_INDEX, 1)?;
        self.store_index_locked(KEY_LAST_INDEX, 0)?;

        Ok(())
    }

    /// Reload index caches from database
    fn reload_index_caches(&self) -> Result<()> {
        let first = self.load_index(KEY_FIRST_INDEX)?;
        let last = self.load_index(KEY_LAST_INDEX)?;

        self.first_index_cache.store(first, Ordering::SeqCst);
        self.last_index_cache.store(last, Ordering::SeqCst);

        Ok(())
    }

    /// Store an entry at the given index
    fn store_entry(&self, entry: &Entry) -> Result<()> {
        let cf_entries = self.db.cf_handle(CF_ENTRIES)
            .ok_or_else(|| Error::Store(StorageError::Unavailable))?;

        let key = entry.index.to_be_bytes();
        let value = entry.write_to_bytes()
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;

        self.db.put_cf(cf_entries, key, value)
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;

        Ok(())
    }

    /// Load an entry at the given index
    fn load_entry(&self, index: u64) -> Result<Option<Entry>> {
        let cf_entries = self.db.cf_handle(CF_ENTRIES)
            .ok_or_else(|| Error::Store(StorageError::Unavailable))?;

        let key = index.to_be_bytes();
        let value = self.db.get_cf(cf_entries, key)
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;

        match value {
            Some(bytes) => {
                let entry = Entry::parse_from_bytes(&bytes)
                    .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;
                Ok(Some(entry))
            }
            None => Ok(None),
        }
    }

    /// Store hard state (requires write lock to be held)
    fn store_hard_state_locked(&self, hs: &HardState) -> Result<()> {
        let cf_metadata = self.db.cf_handle(CF_METADATA)
            .ok_or_else(|| Error::Store(StorageError::Unavailable))?;

        let value = hs.write_to_bytes()
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;

        self.db.put_cf(cf_metadata, KEY_HARD_STATE, value)
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;

        Ok(())
    }

    /// Store hard state
    pub fn store_hard_state(&self, hs: &HardState) -> Result<()> {
        let _lock = self.lock.write().unwrap();
        self.store_hard_state_locked(hs)
    }

    /// Load hard state
    pub fn load_hard_state(&self) -> Result<HardState> {
        let cf_metadata = self.db.cf_handle(CF_METADATA)
            .ok_or_else(|| Error::Store(StorageError::Unavailable))?;

        let value = self.db.get_cf(cf_metadata, KEY_HARD_STATE)
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;

        match value {
            Some(bytes) => {
                HardState::parse_from_bytes(&bytes)
                    .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))
            }
            None => Ok(HardState::default()),
        }
    }

    /// Store node ID
    pub fn store_node_id(&self, node_id: u64) -> Result<()> {
        let cf_metadata = self.db.cf_handle(CF_METADATA)
            .ok_or_else(|| Error::Store(StorageError::Unavailable))?;

        self.db.put_cf(cf_metadata, KEY_NODE_ID, node_id.to_be_bytes())
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))
    }

    /// Load node ID (returns None if not set)
    pub fn load_node_id(&self) -> Result<Option<u64>> {
        let cf_metadata = self.db.cf_handle(CF_METADATA)
            .ok_or_else(|| Error::Store(StorageError::Unavailable))?;

        let value = self.db.get_cf(cf_metadata, KEY_NODE_ID)
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;

        match value {
            Some(bytes) => {
                if bytes.len() == 8 {
                    let mut array = [0u8; 8];
                    array.copy_from_slice(&bytes);
                    Ok(Some(u64::from_be_bytes(array)))
                } else {
                    Err(Error::Store(StorageError::Other(
                        "Invalid node_id size in storage".into()
                    )))
                }
            }
            None => Ok(None),
        }
    }

    /// Store conf state (requires write lock to be held)
    fn store_conf_state_locked(&self, cs: &ConfState) -> Result<()> {
        let cf_metadata = self.db.cf_handle(CF_METADATA)
            .ok_or_else(|| Error::Store(StorageError::Unavailable))?;

        let value = cs.write_to_bytes()
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;

        self.db.put_cf(cf_metadata, KEY_CONF_STATE, value)
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;

        Ok(())
    }

    /// Store conf state
    pub fn store_conf_state(&self, cs: &ConfState) -> Result<()> {
        let _lock = self.lock.write().unwrap();
        self.store_conf_state_locked(cs)
    }

    /// Load conf state
    pub fn load_conf_state(&self) -> Result<ConfState> {
        let cf_metadata = self.db.cf_handle(CF_METADATA)
            .ok_or_else(|| Error::Store(StorageError::Unavailable))?;

        let value = self.db.get_cf(cf_metadata, KEY_CONF_STATE)
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;

        match value {
            Some(bytes) => {
                ConfState::parse_from_bytes(&bytes)
                    .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))
            }
            None => Ok(ConfState::default()),
        }
    }

    /// Store an index value (first_index or last_index)
    fn store_index_locked(&self, key: &[u8], value: u64) -> Result<()> {
        let cf_metadata = self.db.cf_handle(CF_METADATA)
            .ok_or_else(|| Error::Store(StorageError::Unavailable))?;

        self.db.put_cf(cf_metadata, key, value.to_be_bytes())
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;

        Ok(())
    }

    /// Load an index value
    fn load_index(&self, key: &[u8]) -> Result<u64> {
        let cf_metadata = self.db.cf_handle(CF_METADATA)
            .ok_or_else(|| Error::Store(StorageError::Unavailable))?;

        let value = self.db.get_cf(cf_metadata, key)
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;

        match value {
            Some(bytes) => {
                if bytes.len() != 8 {
                    return Err(Error::Store(StorageError::Other(
                        "Invalid index bytes".into()
                    )));
                }
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&bytes);
                Ok(u64::from_be_bytes(buf))
            }
            None => Ok(0),
        }
    }

    /// Append entries to the log
    ///
    /// This is the main write path for log entries. It uses a WriteBatch
    /// for atomic multi-entry writes.
    pub fn append(&self, entries: &[Entry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        let _lock = self.lock.write().unwrap();

        let cf_entries = self.db.cf_handle(CF_ENTRIES)
            .ok_or_else(|| Error::Store(StorageError::Unavailable))?;
        let cf_metadata = self.db.cf_handle(CF_METADATA)
            .ok_or_else(|| Error::Store(StorageError::Unavailable))?;

        let mut batch = WriteBatch::default();

        // Add all entries to batch
        for entry in entries {
            let key = entry.index.to_be_bytes();
            let value = entry.write_to_bytes()
                .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;
            batch.put_cf(cf_entries, key, value);
        }

        // Update last_index
        let last_index = entries.last().unwrap().index;
        batch.put_cf(cf_metadata, KEY_LAST_INDEX, last_index.to_be_bytes());

        // Update first_index if this is the first write
        let current_first = self.first_index_cache.load(Ordering::SeqCst);
        if current_first == 0 {
            let first_index = entries.first().unwrap().index;
            batch.put_cf(cf_metadata, KEY_FIRST_INDEX, first_index.to_be_bytes());
            self.first_index_cache.store(first_index, Ordering::SeqCst);
        }

        // Write batch atomically
        self.db.write(batch)
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;

        // Update cache
        self.last_index_cache.store(last_index, Ordering::SeqCst);

        Ok(())
    }

    /// Compact entries through the given index
    ///
    /// Removes entries with index <= through_index from storage.
    /// Updates first_index to through_index + 1.
    pub fn compact(&self, through_index: u64) -> Result<()> {
        let _lock = self.lock.write().unwrap();

        let cf_entries = self.db.cf_handle(CF_ENTRIES)
            .ok_or_else(|| Error::Store(StorageError::Unavailable))?;
        let cf_metadata = self.db.cf_handle(CF_METADATA)
            .ok_or_else(|| Error::Store(StorageError::Unavailable))?;

        let first_index = self.first_index_cache.load(Ordering::SeqCst);

        // Delete entries from first_index to through_index
        for idx in first_index..=through_index {
            let key = idx.to_be_bytes();
            self.db.delete_cf(cf_entries, key)
                .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;
        }

        // Update first_index
        let new_first = through_index + 1;
        self.db.put_cf(cf_metadata, KEY_FIRST_INDEX, new_first.to_be_bytes())
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;

        self.first_index_cache.store(new_first, Ordering::SeqCst);

        Ok(())
    }

    /// Apply a snapshot
    ///
    /// Stores the snapshot data and metadata, and updates hard state and conf state.
    pub fn apply_snapshot(&self, snapshot: Snapshot) -> Result<()> {
        let _lock = self.lock.write().unwrap();

        let cf_snapshot = self.db.cf_handle(CF_SNAPSHOT)
            .ok_or_else(|| Error::Store(StorageError::Unavailable))?;
        let cf_metadata = self.db.cf_handle(CF_METADATA)
            .ok_or_else(|| Error::Store(StorageError::Unavailable))?;

        // Store snapshot data
        let snapshot_bytes = snapshot.write_to_bytes()
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;
        self.db.put_cf(cf_snapshot, KEY_SNAPSHOT_DATA, snapshot_bytes)
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;

        // Update conf state from snapshot
        let conf_state = snapshot.get_metadata().get_conf_state().clone();
        self.store_conf_state_locked(&conf_state)?;

        // Update indices - snapshot becomes the new first entry
        let snapshot_index = snapshot.get_metadata().index;
        self.store_index_locked(KEY_FIRST_INDEX, snapshot_index + 1)?;

        // Update caches
        self.first_index_cache.store(snapshot_index + 1, Ordering::SeqCst);

        Ok(())
    }

    /// Get the stored snapshot
    pub fn get_snapshot(&self) -> Result<Option<Snapshot>> {
        let cf_snapshot = self.db.cf_handle(CF_SNAPSHOT)
            .ok_or_else(|| Error::Store(StorageError::Unavailable))?;

        let value = self.db.get_cf(cf_snapshot, KEY_SNAPSHOT_DATA)
            .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;

        match value {
            Some(bytes) => {
                let snapshot = Snapshot::parse_from_bytes(&bytes)
                    .map_err(|e| Error::Store(StorageError::Other(Box::new(e))))?;
                Ok(Some(snapshot))
            }
            None => Ok(None),
        }
    }

    /// Compatibility method: apply snapshot with data (matches MemStorageWithSnapshot API)
    pub fn apply_snapshot_with_data(&self, snapshot: Snapshot) -> Result<()> {
        self.apply_snapshot(snapshot)
    }

    /// Compatibility method: returns self for write operations (matches MemStorageWithSnapshot::wl())
    pub fn wl(&self) -> &Self {
        self
    }

    /// Compatibility method: returns self for read operations (matches MemStorageWithSnapshot::rl())
    pub fn rl(&self) -> &Self {
        self
    }

    /// Compatibility method: set hard state (matches MemStorageWithSnapshot write lock API)
    pub fn set_hardstate(&self, hs: HardState) -> Result<()> {
        self.store_hard_state(&hs)
    }

    /// Compatibility method: update commit index in hard state
    pub fn update_commit(&self, commit: u64) -> Result<()> {
        let mut hs = self.load_hard_state()?;
        hs.set_commit(commit);
        self.store_hard_state(&hs)
    }

    /// Compatibility method: get mutable hard state reference (not truly mutable for RocksDB)
    /// Note: For RocksDB, mutations should use specific methods like update_commit()
    pub fn mut_hard_state(&self) -> HardState {
        self.load_hard_state().unwrap_or_default()
    }

    /// Compatibility method: get hard state (matches MemStorageWithSnapshot read lock API)
    pub fn hard_state(&self) -> HardState {
        self.load_hard_state().unwrap_or_default()
    }
}

impl Clone for RocksDBStorage {
    fn clone(&self) -> Self {
        Self {
            db: self.db.clone(),
            first_index_cache: AtomicU64::new(self.first_index_cache.load(Ordering::SeqCst)),
            last_index_cache: AtomicU64::new(self.last_index_cache.load(Ordering::SeqCst)),
            lock: self.lock.clone(),
        }
    }
}

impl raft::Storage for RocksDBStorage {
    fn initial_state(&self) -> Result<RaftState> {
        let hard_state = self.load_hard_state()?;
        let conf_state = self.load_conf_state()?;

        Ok(RaftState {
            hard_state,
            conf_state,
        })
    }

    fn entries(
        &self,
        low: u64,
        high: u64,
        max_size: impl Into<Option<u64>>,
        _context: GetEntriesContext,
    ) -> Result<Vec<Entry>> {
        let max_size = max_size.into();

        if low >= high {
            return Ok(vec![]);
        }

        let first_index = self.first_index_cache.load(Ordering::SeqCst);
        let last_index = self.last_index_cache.load(Ordering::SeqCst);

        // Check bounds
        if low < first_index {
            return Err(Error::Store(StorageError::Compacted));
        }

        if high > last_index + 1 {
            return Err(Error::Store(StorageError::Unavailable));
        }

        let mut entries = Vec::new();
        let mut total_size = 0u64;

        for idx in low..high {
            match self.load_entry(idx)? {
                Some(entry) => {
                    let entry_size = entry.compute_size() as u64;

                    // Check size limit
                    if let Some(max) = max_size {
                        if total_size > 0 && total_size + entry_size > max {
                            break;
                        }
                    }

                    total_size += entry_size;
                    entries.push(entry);
                }
                None => {
                    // Entry not found - this is an error
                    return Err(Error::Store(StorageError::Unavailable));
                }
            }
        }

        Ok(entries)
    }

    fn term(&self, idx: u64) -> Result<u64> {
        let first_index = self.first_index_cache.load(Ordering::SeqCst);
        let last_index = self.last_index_cache.load(Ordering::SeqCst);

        // Check if this matches the snapshot (following MemStorage logic)
        if let Some(snapshot) = self.get_snapshot()? {
            let snapshot_index = snapshot.get_metadata().index;
            if idx == snapshot_index {
                return Ok(snapshot.get_metadata().term);
            }
        } else {
            // No snapshot stored yet - default snapshot has index=0, term=0
            if idx == 0 {
                return Ok(0);
            }
        }

        if idx < first_index {
            return Err(Error::Store(StorageError::Compacted));
        }

        if idx > last_index {
            return Err(Error::Store(StorageError::Unavailable));
        }

        match self.load_entry(idx)? {
            Some(entry) => Ok(entry.term),
            None => Err(Error::Store(StorageError::Unavailable)),
        }
    }

    fn first_index(&self) -> Result<u64> {
        Ok(self.first_index_cache.load(Ordering::SeqCst))
    }

    fn last_index(&self) -> Result<u64> {
        Ok(self.last_index_cache.load(Ordering::SeqCst))
    }

    fn snapshot(&self, request_index: u64, _to: u64) -> Result<Snapshot> {
        match self.get_snapshot()? {
            Some(snapshot) => {
                let snapshot_index = snapshot.get_metadata().index;
                if snapshot_index < request_index {
                    return Err(Error::Store(StorageError::SnapshotTemporarilyUnavailable));
                }
                Ok(snapshot)
            }
            None => Err(Error::Store(StorageError::SnapshotTemporarilyUnavailable)),
        }
    }
}

#[cfg(all(test, feature = "persistent-storage"))]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_storage() -> (RocksDBStorage, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let storage = RocksDBStorage::open_or_create(
            temp_dir.path(),
            1,  // test node_id
            ConfState::default(),
        ).unwrap();
        (storage, temp_dir)
    }

    #[test]
    fn test_create_and_open() {
        let (storage, _temp_dir) = create_test_storage();

        // Verify initial state follows raft-rs convention:
        // Empty log has first_index=1, last_index=0 (first > last indicates empty)
        assert_eq!(storage.first_index().unwrap(), 1);
        assert_eq!(storage.last_index().unwrap(), 0);

        let raft_state = storage.initial_state().unwrap();
        assert_eq!(raft_state.hard_state.term, 0);
        assert_eq!(raft_state.hard_state.vote, 0);
        assert_eq!(raft_state.hard_state.commit, 0);
    }

    #[test]
    fn test_append_entries() {
        let (storage, _temp_dir) = create_test_storage();

        // Create some entries
        let mut entries = vec![];
        for i in 1..=5 {
            let mut entry = Entry::default();
            entry.index = i;
            entry.term = 1;
            entry.set_data(format!("data{}", i).into_bytes().into());
            entries.push(entry);
        }

        // Append entries
        storage.append(&entries).unwrap();

        // Verify indices
        assert_eq!(storage.first_index().unwrap(), 1);
        assert_eq!(storage.last_index().unwrap(), 5);

        // Retrieve entries
        let retrieved = storage.entries(1, 6, None, GetEntriesContext::empty(false)).unwrap();
        assert_eq!(retrieved.len(), 5);
        assert_eq!(retrieved[0].index, 1);
        assert_eq!(retrieved[4].index, 5);
        assert_eq!(retrieved[0].get_data(), b"data1");
    }

    #[test]
    fn test_entries_with_max_size() {
        let (storage, _temp_dir) = create_test_storage();

        // Create entries with known sizes
        let mut entries = vec![];
        for i in 1..=10 {
            let mut entry = Entry::default();
            entry.index = i;
            entry.term = 1;
            entry.set_data(vec![0u8; 100].into()); // 100 bytes of data
            entries.push(entry);
        }

        storage.append(&entries).unwrap();

        // Request with size limit (should get fewer entries)
        let retrieved = storage.entries(1, 11, Some(300), GetEntriesContext::empty(false)).unwrap();
        assert!(retrieved.len() < 10);
        assert!(retrieved.len() > 0);
    }

    #[test]
    fn test_hard_state_persistence() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().to_path_buf();

        // Create storage and set hard state
        {
            let storage = RocksDBStorage::open_or_create(&path, 1, ConfState::default()).unwrap();

            let mut hs = HardState::default();
            hs.term = 5;
            hs.vote = 2;
            hs.commit = 10;

            storage.store_hard_state(&hs).unwrap();
        }

        // Reopen and verify
        {
            let storage = RocksDBStorage::open_or_create(&path, 1, ConfState::default()).unwrap();
            let hs = storage.load_hard_state().unwrap();

            assert_eq!(hs.term, 5);
            assert_eq!(hs.vote, 2);
            assert_eq!(hs.commit, 10);
        }
    }

    #[test]
    fn test_conf_state_persistence() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().to_path_buf();

        // Create storage and set conf state
        {
            let storage = RocksDBStorage::open_or_create(&path, 1, ConfState::default()).unwrap();

            let mut cs = ConfState::default();
            cs.voters = vec![1, 2, 3];
            cs.learners = vec![4, 5];

            storage.store_conf_state(&cs).unwrap();
        }

        // Reopen and verify
        {
            let storage = RocksDBStorage::open_or_create(&path, 1, ConfState::default()).unwrap();
            let cs = storage.load_conf_state().unwrap();

            assert_eq!(cs.voters, vec![1, 2, 3]);
            assert_eq!(cs.learners, vec![4, 5]);
        }
    }

    #[test]
    fn test_compaction() {
        let (storage, _temp_dir) = create_test_storage();

        // Append entries 1-10
        let mut entries = vec![];
        for i in 1..=10 {
            let mut entry = Entry::default();
            entry.index = i;
            entry.term = 1;
            entries.push(entry);
        }
        storage.append(&entries).unwrap();

        // Compact through index 5
        storage.compact(5).unwrap();

        // Verify first_index updated
        assert_eq!(storage.first_index().unwrap(), 6);
        assert_eq!(storage.last_index().unwrap(), 10);

        // Old entries should be compacted
        let result = storage.entries(1, 6, None, GetEntriesContext::empty(false));
        assert!(result.is_err());

        // New entries should be available
        let retrieved = storage.entries(6, 11, None, GetEntriesContext::empty(false)).unwrap();
        assert_eq!(retrieved.len(), 5);
        assert_eq!(retrieved[0].index, 6);
    }

    #[test]
    fn test_snapshot_storage() {
        let (storage, _temp_dir) = create_test_storage();

        // Create a snapshot
        let mut snapshot = Snapshot::default();
        let mut meta = SnapshotMetadata::default();
        meta.index = 100;
        meta.term = 5;

        let mut cs = ConfState::default();
        cs.voters = vec![1, 2, 3];
        meta.set_conf_state(cs);

        snapshot.set_metadata(meta);
        snapshot.set_data(b"snapshot data".to_vec().into());

        // Apply snapshot
        storage.apply_snapshot(snapshot.clone()).unwrap();

        // Retrieve snapshot
        let retrieved = storage.get_snapshot().unwrap().unwrap();
        assert_eq!(retrieved.get_metadata().index, 100);
        assert_eq!(retrieved.get_metadata().term, 5);
        assert_eq!(retrieved.get_data(), b"snapshot data");

        // Verify first_index updated
        assert_eq!(storage.first_index().unwrap(), 101);
    }

    #[test]
    fn test_crash_recovery() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().to_path_buf();

        // Write some data
        {
            let storage = RocksDBStorage::open_or_create(&path, 1, ConfState::default()).unwrap();

            let mut entries = vec![];
            for i in 1..=5 {
                let mut entry = Entry::default();
                entry.index = i;
                entry.term = 1;
                entry.set_data(format!("data{}", i).into_bytes().into());
                entries.push(entry);
            }
            storage.append(&entries).unwrap();

            let mut hs = HardState::default();
            hs.term = 1;
            hs.commit = 5;
            storage.store_hard_state(&hs).unwrap();
        }
        // Storage dropped (simulates crash)

        // Reopen and verify all data persisted
        {
            let storage = RocksDBStorage::open_or_create(&path, 1, ConfState::default()).unwrap();

            assert_eq!(storage.first_index().unwrap(), 1);
            assert_eq!(storage.last_index().unwrap(), 5);

            let entries = storage.entries(1, 6, None, GetEntriesContext::empty(false)).unwrap();
            assert_eq!(entries.len(), 5);
            assert_eq!(entries[0].get_data(), b"data1");

            let hs = storage.load_hard_state().unwrap();
            assert_eq!(hs.term, 1);
            assert_eq!(hs.commit, 5);
        }
    }

    #[test]
    fn test_term_lookup() {
        let (storage, _temp_dir) = create_test_storage();

        let mut entries = vec![];
        for i in 1..=5 {
            let mut entry = Entry::default();
            entry.index = i;
            entry.term = i; // term == index for this test
            entries.push(entry);
        }
        storage.append(&entries).unwrap();

        // Test term lookup
        assert_eq!(storage.term(1).unwrap(), 1);
        assert_eq!(storage.term(3).unwrap(), 3);
        assert_eq!(storage.term(5).unwrap(), 5);

        // term(0) returns 0 (default snapshot term) following raft-rs MemStorage convention
        assert_eq!(storage.term(0).unwrap(), 0);

        // Index beyond last_index is unavailable
        assert!(storage.term(6).is_err());
    }

    #[test]
    fn test_empty_entries_range() {
        let (storage, _temp_dir) = create_test_storage();

        let mut entries = vec![];
        for i in 1..=5 {
            let mut entry = Entry::default();
            entry.index = i;
            entry.term = 1;
            entries.push(entry);
        }
        storage.append(&entries).unwrap();

        // Empty range (low >= high)
        let result = storage.entries(3, 3, None, GetEntriesContext::empty(false)).unwrap();
        assert_eq!(result.len(), 0);

        let result = storage.entries(5, 3, None, GetEntriesContext::empty(false)).unwrap();
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_node_id_persistence_and_restart() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().to_path_buf();

        // First start: create storage with node_id = 42
        {
            let storage = RocksDBStorage::open_or_create(&path, 42, ConfState::default()).unwrap();

            // Verify node_id was stored
            assert_eq!(storage.load_node_id().unwrap(), Some(42));

            // Write some Raft state
            let mut entries = vec![];
            for i in 1..=3 {
                let mut entry = Entry::default();
                entry.index = i;
                entry.term = 1;
                entry.set_data(format!("data{}", i).into_bytes().into());
                entries.push(entry);
            }
            storage.append(&entries).unwrap();

            let mut hs = HardState::default();
            hs.term = 1;
            hs.commit = 3;
            storage.store_hard_state(&hs).unwrap();
        }
        // Storage dropped (simulates node shutdown)

        // Restart: open storage with same node_id (should succeed)
        {
            let storage = RocksDBStorage::open_or_create(&path, 42, ConfState::default()).unwrap();

            // Verify node_id matches
            assert_eq!(storage.load_node_id().unwrap(), Some(42));

            // Verify Raft state was persisted
            assert_eq!(storage.first_index().unwrap(), 1);
            assert_eq!(storage.last_index().unwrap(), 3);

            let entries = storage.entries(1, 4, None, GetEntriesContext::empty(false)).unwrap();
            assert_eq!(entries.len(), 3);

            let hs = storage.load_hard_state().unwrap();
            assert_eq!(hs.term, 1);
            assert_eq!(hs.commit, 3);
        }

        // Attempt to open with wrong node_id (should fail)
        {
            let result = RocksDBStorage::open_or_create(&path, 99, ConfState::default());
            assert!(result.is_err(), "Opening with wrong node_id should fail");
        }
    }

    #[test]
    fn test_storage_exists_helper() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().to_path_buf();

        // Fresh directory - storage should not exist
        assert!(!RocksDBStorage::storage_exists(&path).unwrap());

        // Create storage
        {
            let _storage = RocksDBStorage::open_or_create(&path, 1, ConfState::default()).unwrap();
        }

        // Now storage should exist
        assert!(RocksDBStorage::storage_exists(&path).unwrap());
    }
}
