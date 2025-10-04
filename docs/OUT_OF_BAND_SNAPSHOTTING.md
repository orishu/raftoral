# Out-of-Band Snapshot Transfer

## Current State: MemStorageWithSnapshot

### What It Does

- Stores snapshot **data** in memory (`Arc<RwLock<Bytes>>`)
- Stores snapshot **metadata** via the underlying MemStorage (also in memory)
- Works perfectly for **single-process, multi-node in-memory clusters** (our test setup)

### Why It Works for InMemoryClusterTransport

All nodes are in the same process, so when the leader calls `storage.snapshot()` to get a snapshot to send, it retrieves the actual data from the shared memory, serializes it into a Raft message, and sends it through tokio channels to the follower in the same process.

## Snapshot Transmission in Real Network Scenarios

In a real distributed system with nodes on different machines connected via gRPC/network:

### 1. Snapshot Creation (Leader Side) ✅ Already Works

```rust
// Leader creates snapshot
let snapshot_data = executor.create_snapshot(index)?;  // Gets workflow state
snapshot.set_data(snapshot_data);                      // Attaches to Raft snapshot
storage.apply_snapshot_with_data(snapshot)?;           // Stores locally
```

This part is fine - the leader can create and store snapshots.

### 2. Snapshot Transmission ⚠️ Needs Work for Large Snapshots

**Current approach (works in-process and for small snapshots):**

```rust
// Leader retrieves snapshot from storage
let snapshot = storage.snapshot(request_index, to_peer)?;  // 114KB+ in our test
// Raft sends entire snapshot in MsgSnapshot message
send_message(MsgSnapshot { snapshot });  // Serializes data into protobuf
```

**Problem with network transmission of large snapshots:**

- Snapshots can be **huge** (GB in production)
- Sending 1GB snapshot in a single RPC message is impractical:
  - Memory spikes
  - Timeout issues
  - Network failures waste progress
  - No resumability

### 3. What Real Systems Do 🏗️

Real Raft implementations (etcd, TiKV, etc.) use **out-of-band snapshot transfer**:

```
┌─────────────────────────────────────────────────────────────┐
│ Current (In-Memory): All in Raft Protocol                  │
├─────────────────────────────────────────────────────────────┤
│ Leader: storage.snapshot() → [114KB data] → MsgSnapshot    │
│ Channel: ──────────[114KB]──────────→                       │
│ Follower: MsgSnapshot → [114KB data] → restore_from_snap() │
└─────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────┐
│ Production (Network): Separate Snapshot Transfer            │
├─────────────────────────────────────────────────────────────┤
│ Step 1: Raft sends MsgSnapshot with metadata only          │
│   Leader: storage.snapshot() → [metadata, no data]          │
│   gRPC: ──[index: 1103, term: 1, size: 114KB]──→           │
│                                                              │
│ Step 2: Separate HTTP/gRPC snapshot transfer stream        │
│   Follower: "I need snapshot 1103"                         │
│   Leader: Reads from disk/S3 → streams in chunks           │
│   HTTP: ─[chunk 1]─[chunk 2]─[chunk 3]─...─→              │
│   Follower: Writes chunks to temp file                      │
│                                                              │
│ Step 3: After transfer completes                            │
│   Follower: Reads temp file → restore_from_snapshot()       │
│   Follower: Sends MsgAppendResponse to continue             │
└─────────────────────────────────────────────────────────────┘
```

## Future Implementation Plan

### Option 1: Chunked Streaming (Recommended for Large Snapshots)

Create a separate snapshot transfer service:

```rust
// New trait for snapshot storage
pub trait SnapshotStore: Send + Sync {
    /// Save snapshot to persistent storage (disk/S3)
    fn save_snapshot(&self, index: u64, data: &[u8]) -> Result<PathBuf>;

    /// Stream snapshot from storage
    fn stream_snapshot(&self, index: u64) -> Result<impl Stream<Item = Bytes>>;

    /// Get snapshot metadata without loading data
    fn snapshot_metadata(&self, index: u64) -> Result<SnapshotMetadata>;

    /// Delete old snapshots (cleanup)
    fn compact_snapshots(&self, keep_count: usize) -> Result<()>;
}

// Disk-based implementation
pub struct DiskSnapshotStore {
    snapshot_dir: PathBuf,
    chunk_size: usize,
}

impl DiskSnapshotStore {
    pub fn new(snapshot_dir: PathBuf) -> Self {
        Self {
            snapshot_dir,
            chunk_size: 1024 * 1024, // 1MB chunks
        }
    }

    fn save_snapshot(&self, index: u64, data: &[u8]) -> Result<PathBuf> {
        let path = self.snapshot_dir.join(format!("snapshot-{:016}.bin", index));
        std::fs::write(&path, data)?;
        Ok(path)
    }

    fn stream_snapshot(&self, index: u64) -> Result<impl Stream<Item = Bytes>> {
        let path = self.snapshot_dir.join(format!("snapshot-{:016}.bin", index));
        let file = tokio::fs::File::open(path).await?;
        // Stream in configurable chunks
        Ok(ReaderStream::with_capacity(file, self.chunk_size))
    }
}

// S3-based implementation for cloud deployments
pub struct S3SnapshotStore {
    bucket: String,
    prefix: String,
    client: S3Client,
}

// GrpcClusterTransport would implement snapshot streaming
service SnapshotTransfer {
    // Stream snapshot data separately from Raft messages
    rpc TransferSnapshot(SnapshotRequest) returns (stream SnapshotChunk);
}

message SnapshotRequest {
    uint64 snapshot_index = 1;
    uint64 snapshot_term = 2;
    uint64 offset = 3;        // For resumability
}

message SnapshotChunk {
    bytes data = 1;
    uint64 offset = 2;
    uint64 total_size = 3;
    bool is_final = 4;
}
```

### Option 2: Keep MemStorageWithSnapshot for Small Snapshots

For scenarios where snapshots are small (< 10MB), the current approach works fine even over network:

```rust
// GrpcClusterTransport can continue using MemStorageWithSnapshot
// Just serialize the snapshot in the protobuf message
let snapshot = storage.snapshot(index, peer)?;
let msg = RaftMessage {
    snapshot: Some(snapshot),  // Protobuf handles serialization
};
grpc_client.send_raft_message(msg).await?;
```

**This works when:**
- Workflow state is small
- Network is reliable
- Snapshots are infrequent

## Recommended Architecture

```rust
pub enum SnapshotTransferMode {
    /// Embed snapshot data in Raft messages (current behavior)
    /// Good for: in-memory clusters, small snapshots (< 10MB)
    Embedded,

    /// Stream snapshots separately from Raft protocol
    /// Good for: production, large snapshots, unreliable networks
    Streaming {
        store: Arc<dyn SnapshotStore>,
        chunk_size: usize,
    },
}

pub struct GrpcClusterTransport {
    mode: SnapshotTransferMode,
    // ...
}

impl GrpcClusterTransport {
    pub fn new_with_embedded_snapshots(nodes: Vec<NodeConfig>) -> Self {
        Self {
            mode: SnapshotTransferMode::Embedded,
            // ...
        }
    }

    pub fn new_with_streaming_snapshots(
        nodes: Vec<NodeConfig>,
        store: Arc<dyn SnapshotStore>,
    ) -> Self {
        Self {
            mode: SnapshotTransferMode::Streaming {
                store,
                chunk_size: 1024 * 1024, // 1MB
            },
            // ...
        }
    }
}
```

## Implementation Phases

### Phase 1: Disk-Based Snapshots (Foundation)

**Goal**: Enable snapshot persistence to disk for recovery after process restart.

**Changes needed:**
1. Implement `DiskSnapshotStore`
2. Modify `MemStorageWithSnapshot` to optionally persist to disk
3. Add snapshot metadata file (index, term, timestamp)
4. Implement snapshot cleanup/compaction

**Benefits:**
- Survive process restarts
- Reduce memory usage
- Foundation for streaming

### Phase 2: gRPC Snapshot Streaming

**Goal**: Enable efficient snapshot transfer over network for large snapshots.

**Changes needed:**
1. Add `SnapshotTransfer` gRPC service
2. Implement chunked upload/download
3. Add progress tracking and resumability
4. Handle partial transfers gracefully

**Benefits:**
- Support large snapshots (GB+)
- Better network resilience
- Progress visibility

### Phase 3: Cloud Storage Integration

**Goal**: Enable snapshot storage in S3/GCS for distributed deployments.

**Changes needed:**
1. Implement `S3SnapshotStore`
2. Implement `GCSSnapshotStore`
3. Add authentication/credentials management
4. Add snapshot replication across regions

**Benefits:**
- Unlimited storage capacity
- Cross-region disaster recovery
- Separation of compute and storage

## Current Limitations and When to Upgrade

### MemStorageWithSnapshot Works Well For:

- ✅ In-memory multi-node clusters (testing)
- ✅ Small workflows (< 10MB state)
- ✅ Development and testing
- ✅ Initial gRPC implementation with small state

### Upgrade to Streaming When:

- ⚠️ Snapshots exceed 100MB
- ⚠️ Frequent network timeouts during snapshot transfer
- ⚠️ Memory pressure from large snapshots
- ⚠️ Need to survive process restarts (persistence required)

### Upgrade to Cloud Storage When:

- ⚠️ Snapshots exceed 1GB
- ⚠️ Need cross-region replication
- ⚠️ Deploying in cloud environments
- ⚠️ Cost of local disk becomes significant

## Testing Strategy

### Unit Tests
```rust
#[test]
fn test_disk_snapshot_store_save_and_load() {
    // Test saving snapshot to disk
    // Test loading snapshot from disk
}

#[test]
fn test_snapshot_chunking() {
    // Test streaming large snapshot in chunks
    // Verify chunk boundaries and reassembly
}
```

### Integration Tests
```rust
#[tokio::test]
async fn test_snapshot_transfer_over_grpc() {
    // Create 3-node cluster with gRPC transport
    // Generate large snapshot (100MB+)
    // Add 4th node
    // Verify snapshot streams correctly
}

#[tokio::test]
async fn test_snapshot_transfer_resumability() {
    // Start snapshot transfer
    // Simulate network failure mid-transfer
    // Resume from offset
    // Verify successful completion
}
```

## Performance Targets

| Metric | Target | Notes |
|--------|--------|-------|
| Chunk size | 1-4 MB | Balance memory vs. overhead |
| Transfer rate | > 100 MB/s | Over local network |
| Max snapshot size | 10 GB | Without streaming issues |
| Recovery time | < 5 min | For 1GB snapshot |
| Disk I/O | < 50% CPU | During streaming |

## References

- [etcd Snapshot Design](https://etcd.io/docs/v3.5/op-guide/recovery/)
- [TiKV Snapshot Implementation](https://github.com/tikv/tikv/tree/master/components/raftstore)
- [raft-rs Storage Trait](https://docs.rs/raft/latest/raft/storage/trait.Storage.html)
