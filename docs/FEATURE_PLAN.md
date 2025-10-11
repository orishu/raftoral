# Raftoral Feature Roadmap

This document outlines planned features and enhancements for Raftoral, organized by priority and complexity.

## 1. Workflow Management APIs

**Priority:** High
**Complexity:** Low
**Estimated Effort:** 1-2 weeks

### Goals
Provide runtime APIs for observing and controlling active workflows.

### Features

#### 1.1 List Running Workflows
```rust
pub async fn list_workflows(&self) -> Vec<WorkflowInfo>;

pub struct WorkflowInfo {
    pub workflow_id: String,
    pub workflow_type: String,
    pub version: u32,
    pub status: WorkflowStatus,
    pub started_at: SystemTime,
    pub owner_node: u64,
}
```

**Implementation Notes:**
- Query `WorkflowCommandExecutor` state for all workflows with status `Running`
- Include metadata: type, version, start time, owner node
- Filter by status, type, or time range (optional)

#### 1.2 Terminate Workflow
```rust
pub async fn terminate_workflow(
    &self,
    workflow_id: &str,
    reason: Option<String>
) -> Result<(), WorkflowError>;
```

**Implementation Notes:**
- Add new `WorkflowCommand::WorkflowTerminate` variant
- Propose termination command through Raft consensus
- On apply: mark workflow as `Terminated`, broadcast event
- Running workflow functions should check termination status periodically
- Add `WorkflowContext::is_terminated()` helper for graceful cancellation

**Design Considerations:**
- Should termination be immediate or graceful?
- How do we handle in-flight checkpoint proposals?
- Should terminated workflows be removable from state?

**Testing:**
- Test termination during various workflow stages
- Test termination on leader vs. follower nodes
- Test termination with pending checkpoint operations

---

## 2. Persistent Storage

**Priority:** High
**Complexity:** Medium
**Estimated Effort:** 3-4 weeks

### Goals
Enable nodes to restart and rejoin clusters without losing identity or requiring full snapshot replay.

### Features

#### 2.1 Persistent Raft Log Storage
**Current State:** In-memory `MemStorage` only
**Target:** Pluggable storage backend with RocksDB implementation

```rust
pub trait PersistentStorage: Storage {
    fn persist_entries(&mut self, entries: &[Entry]) -> Result<()>;
    fn persist_hard_state(&mut self, hs: HardState) -> Result<()>;
    fn persist_conf_state(&mut self, cs: ConfState) -> Result<()>;
    fn load_node_id(&self) -> Result<Option<u64>>;
    fn persist_node_id(&mut self, node_id: u64) -> Result<()>;
}

pub struct RocksDBStorage {
    db: rocksdb::DB,
    // Column families: entries, hard_state, conf_state, node_metadata
}
```

**Implementation Plan:**
1. Add `rocksdb` dependency (optional feature flag)
2. Implement `RocksDBStorage` with proper column families
3. Update `RaftNode` to accept generic `Storage` trait
4. Store node ID persistently on first bootstrap/join
5. On restart: load node_id, entries, hard_state, conf_state
6. Rejoin cluster using persisted node_id

**Design Considerations:**
- Storage path configuration (default: `./raftoral_data/{node_id}/`)
- Snapshot integration (see Section 5)
- Compaction strategy for old log entries
- Migration path from in-memory to persistent storage

**Testing:**
- Restart node mid-workflow execution
- Restart node after leadership change
- Restart all nodes sequentially
- Corrupt storage recovery scenarios

#### 2.2 Configuration
```rust
pub struct RaftoralConfig {
    // ... existing fields ...
    pub storage_path: Option<PathBuf>,  // None = in-memory
    pub enable_persistence: bool,
}
```

---

## 3. Scalability: Management + Execution Clusters

**Priority:** High
**Complexity:** Very High
**Estimated Effort:** 14 weeks (3 months)

### Overview

**Problem:** Current single-cluster architecture doesn't scale beyond ~20 nodes because every checkpoint is replicated to all nodes.

**Solution:** Two-tier cluster architecture:
- **Management Cluster:** Single global cluster (all nodes) managing execution cluster membership and workflow lifecycle tracking
- **Execution Clusters:** Multiple small virtual clusters (3-5 nodes each) handling workflow execution and checkpoint replication

**Key Benefit:** Decouple cluster size from checkpoint throughput. 50-node deployment with 10 execution clusters = **10x reduction** in checkpoint replication traffic.

### Architecture Reference

**See detailed design:** [docs/SCALABILITY_ARCHITECTURE.md](./SCALABILITY_ARCHITECTURE.md)

The architecture document covers:
- Management cluster command set (`CreateExecutionCluster`, `AssociateNode`, etc.)
- Execution cluster lifecycle and workflow assignment
- Unified transport layer routing messages to correct cluster
- Dynamic rebalancing as deployment scales
- Performance analysis (10-20x throughput improvement)
- Concerns and open issues (failover complexity, cross-cluster queries, etc.)

### Implementation Phases

**Phase 1: Management Cluster Foundation (4 weeks)**
- Implement `ManagementCommandExecutor`
- Dual-cluster node (management + execution)
- Voter/learner configuration for management cluster

**Phase 2: Unified Transport (3 weeks)**
- Update protobuf schema with cluster routing
- Route messages based on cluster type and ID
- Integration testing with both cluster types

**Phase 3: Workflow Lifecycle Integration (3 weeks)**
- Execution cluster selection strategies
- Lifecycle reporting to management cluster
- End-to-end testing with 50-node deployment

**Phase 4: Dynamic Rebalancing (4 weeks)**
- Load monitoring and metrics
- Automatic cluster creation/destruction
- Scale testing and chaos engineering

---

## 4. Out-of-Band Snapshots

**Priority:** Medium
**Complexity:** Medium
**Estimated Effort:** 4-5 weeks

### Goals
Enable efficient snapshot transfer without blocking Raft consensus.

### Background
**Current State:** Raft snapshots block log compaction
**Problem:** Large snapshots (GB+) stall consensus during transfer
**Solution:** Transfer snapshots via separate gRPC stream

### Design Reference
See `docs/OUT_OF_BAND_SNAPSHOTS.md` for detailed design.

### Implementation Plan

#### 4.1 Snapshot Metadata in Raft Log
```rust
pub struct SnapshotMetadata {
    pub snapshot_id: String,
    pub index: u64,
    pub term: u64,
    pub size_bytes: u64,
    pub checksum: String,
}

// Propose via Raft (small metadata only)
WorkflowCommand::SnapshotAvailable(SnapshotMetadata)
```

#### 4.2 Separate Snapshot Transfer Service
```protobuf
service SnapshotTransfer {
  rpc FetchSnapshot(SnapshotRequest) returns (stream SnapshotChunk);
}

message SnapshotChunk {
  bytes data = 1;
  uint64 offset = 2;
  bool is_final = 3;
}
```

#### 4.3 Snapshot Storage
```rust
pub struct SnapshotStore {
    base_path: PathBuf,  // e.g., ./snapshots/
}

impl SnapshotStore {
    pub fn save(&self, snapshot_id: &str, data: Vec<u8>) -> Result<()>;
    pub fn load(&self, snapshot_id: &str) -> Result<Vec<u8>>;
    pub fn stream(&self, snapshot_id: &str) -> Result<impl Stream<Item=Bytes>>;
}
```

#### 4.4 Integration Flow
1. Leader creates snapshot: serialize `WorkflowState` → file
2. Leader proposes `SnapshotAvailable` metadata via Raft (fast)
3. Followers receive metadata, fetch snapshot via gRPC stream (parallel)
4. Follower validates checksum, applies snapshot
5. Raft log compaction proceeds independently

**Benefits:**
- Non-blocking snapshot transfer
- Parallel multi-follower downloads
- Resume interrupted transfers (chunked streaming)
- Consensus continues during large snapshot transfers

**Testing:**
- Transfer 100MB snapshot
- Interrupt transfer mid-stream, resume
- Multiple followers fetch simultaneously

---

## 5. Persistent Checkpoint History

**Priority:** Low
**Complexity:** Medium
**Estimated Effort:** 3-4 weeks

### Goals
Move checkpoint history from memory to persistent storage to support long-running workflows with thousands of checkpoints.

### Current State
```rust
pub struct WorkflowState {
    pub workflows: HashMap<String, WorkflowStatus>,
    pub results: HashMap<String, Vec<u8>>,
    pub checkpoint_queues: HashMap<(String, String), VecDeque<Vec<u8>>>,
    // All in-memory!
}
```

### Target Architecture
```rust
pub trait CheckpointStore {
    fn append(&mut self, wf_id: &str, key: &str, value: Vec<u8>) -> Result<u64>;
    fn get_latest(&self, wf_id: &str, key: &str) -> Result<Option<Vec<u8>>>;
    fn get_history(&self, wf_id: &str, key: &str) -> Result<Vec<Vec<u8>>>;
    fn prune(&mut self, wf_id: &str, keep_last_n: usize) -> Result<()>;
}

pub struct RocksDBCheckpointStore {
    db: rocksdb::DB,
    // Key format: {workflow_id}:{checkpoint_key}:{sequence}
}
```

### Implementation Plan
1. Add `CheckpointStore` abstraction
2. Implement RocksDB backend with time-series schema
3. Modify `WorkflowCommandExecutor::apply()` to write checkpoints to store
4. Keep in-memory cache for latest checkpoint (hot path optimization)
5. Add pruning policy: keep last N checkpoints per workflow
6. Integrate with snapshots: include checkpoint store in snapshot data

### Design Considerations
- **Retention Policy:** Keep last 100 checkpoints? All checkpoints?
- **Compression:** Use RocksDB compression (Snappy/LZ4)
- **Query API:** Support replay/debugging by fetching historical checkpoints
- **Memory Usage:** LRU cache for frequently accessed checkpoints

### Benefits
- Support workflows with 10,000+ checkpoints
- Enable checkpoint history replay for debugging
- Reduce memory footprint for long-running workflows

---

## 6. Language Bindings (Native)

**Priority:** Low
**Complexity:** Very High
**Estimated Effort:** 12-16 weeks (per language)

### Goals
Enable Raftoral usage from Python, Go, and Java with native workflow definitions.

### Constraints
- **Homogeneous clusters:** All nodes run same language (no mixing Rust + Python)
- **Native workflows:** Workflow functions written in target language, not Rust
- **Full feature parity:** Checkpoints, versioning, all core features supported

### Architecture Options

#### Option A: Rust Core + FFI Bridge
```
┌─────────────────────────────────────┐
│  Python/Go/Java Application         │
│  - Register workflows (native code) │
│  - Call APIs via bindings           │
└──────────────┬──────────────────────┘
               │ FFI/JNI/ctypes
┌──────────────▼──────────────────────┐
│  Raftoral Core (Rust)               │
│  - Raft consensus                   │
│  - Workflow execution orchestration │
│  - Storage layer                    │
└─────────────────────────────────────┘
```

**Implementation:**
- Expose C-compatible API via `extern "C"` + cbindgen
- Python: Use `ctypes` or `pyo3` for bindings
- Go: Use `cgo`
- Java: Use JNI (Java Native Interface)

**Challenges:**
- Async/await mismatch (Rust futures ≠ Python asyncio)
- Memory management across language boundary
- Error handling translation
- Serialization overhead (JSON or protobuf)

#### Option B: gRPC-Based Language Runtime
```
┌────────────────────┐      gRPC      ┌────────────────────┐
│ Python Runtime     │ ←────────────→ │  Raftoral Cluster  │
│ - Workflow registry│                 │  (Rust nodes)      │
│ - Execution engine │                 │  - Raft consensus  │
└────────────────────┘                 └────────────────────┘
```

**Implementation:**
- Raftoral cluster runs as Rust processes
- Language-specific runtime registers workflows via gRPC
- Workflow execution happens in language runtime
- Checkpoints synchronized via gRPC calls

**Challenges:**
- Network overhead for checkpoints
- More complex deployment (multiple process types)
- Requires homogeneous language runtimes across cluster

#### Option C: Pure Rewrite per Language
Reimplement Raftoral in each target language using native Raft libraries.

**Not Recommended:** Massive maintenance burden, diverging implementations.

### Recommended Approach: Option A (FFI Bridge)

#### Phase 1: Python Bindings (12-16 weeks)

**Milestone 1: Core FFI Layer (4 weeks)**
```rust
// src/ffi/mod.rs
#[repr(C)]
pub struct RaftoralHandle {
    _private: [u8; 0],
}

#[no_mangle]
pub extern "C" fn raftoral_start(
    config_json: *const c_char,
    out_handle: *mut *mut RaftoralHandle
) -> i32;

#[no_mangle]
pub extern "C" fn raftoral_register_workflow(
    handle: *mut RaftoralHandle,
    name: *const c_char,
    version: u32,
    callback: extern "C" fn(*const u8, usize, *mut u8, *mut usize) -> i32
) -> i32;
```

**Milestone 2: Python Wrapper (4 weeks)**
```python
# raftoral/__init__.py
from ctypes import CDLL, c_char_p, c_uint32, CFUNCTYPE

class RaftoralRuntime:
    def __init__(self, config: dict):
        self._lib = CDLL("libraftoral.so")
        self._handle = self._lib.raftoral_start(json.dumps(config))

    def register_workflow(self, name: str, version: int, func: Callable):
        # Convert Python async function to C callback
        wrapper = self._create_callback_wrapper(func)
        self._lib.raftoral_register_workflow(
            self._handle, name.encode(), version, wrapper
        )

    async def start_workflow(self, workflow_type: str, input_data: Any):
        # Serialize input, call FFI, wait for completion
        ...
```

**Milestone 3: Async Integration (4 weeks)**
```python
import asyncio

class WorkflowContext:
    async def create_replicated_var(self, key: str, value: Any):
        # Bridge Python asyncio to Rust tokio via callback loop
        future = asyncio.Future()
        self._lib.raftoral_set_checkpoint(
            self._handle, key.encode(),
            pickle.dumps(value),
            self._make_callback(future)
        )
        return await future
```

**Milestone 4: Testing & Documentation (4 weeks)**
- Port examples to Python
- Comprehensive unit tests
- Performance benchmarks (Python vs. Rust overhead)
- API documentation (Sphinx)

#### Phase 2: Go Bindings (12-16 weeks)
Similar structure using `cgo`:
```go
// #cgo LDFLAGS: -L. -lraftoral
// #include "raftoral.h"
import "C"

type RaftoralRuntime struct {
    handle *C.RaftoralHandle
}

func (r *RaftoralRuntime) RegisterWorkflow(
    name string,
    version uint32,
    fn func([]byte) ([]byte, error)
) error {
    // Convert Go function to C callback
    ...
}
```

#### Phase 3: Java Bindings (16-20 weeks)
Most complex due to JNI overhead:
```java
public class RaftoralRuntime {
    private long nativeHandle;

    static {
        System.loadLibrary("raftoral_jni");
    }

    public native void registerWorkflow(
        String name,
        int version,
        WorkflowFunction function
    );

    public CompletableFuture<byte[]> startWorkflow(
        String workflowType,
        byte[] input
    ) {
        return CompletableFuture.supplyAsync(() ->
            startWorkflowNative(nativeHandle, workflowType, input)
        );
    }

    private native byte[] startWorkflowNative(
        long handle,
        String workflowType,
        byte[] input
    );
}
```

### Common Challenges Across Languages

1. **Async/Await Translation**
   - Rust: `async fn` with `tokio::spawn`
   - Python: `async def` with `asyncio.create_task`
   - Go: goroutines with channels
   - Java: `CompletableFuture`
   - **Solution:** Abstract event loop bridge in FFI layer

2. **Memory Management**
   - Who owns serialized data?
   - When to free C strings?
   - **Solution:** Clear ownership rules + RAII wrappers

3. **Error Handling**
   - Rust: `Result<T, E>`
   - Python: exceptions
   - Go: `(T, error)` tuples
   - Java: exceptions
   - **Solution:** Map Rust errors to language-native errors

4. **Serialization**
   - Need language-agnostic format (JSON or protobuf)
   - Performance overhead vs. type safety trade-off
   - **Solution:** Use `serde_json` + language-specific JSON libraries

### Testing Strategy
- **Unit tests:** FFI layer correctness
- **Integration tests:** Port existing Rust examples
- **Cross-language tests:** Ensure homogeneous cluster behavior
- **Performance tests:** Measure FFI overhead (<10% acceptable)

---

## Priorities Summary

| Feature | Priority | Complexity | Dependencies | Target Release |
|---------|----------|------------|--------------|----------------|
| Workflow Management APIs | High | Low | None | v0.2.0 |
| Persistent Storage | High | Medium | None | v0.3.0 |
| Management + Execution Clusters | High | Very High | Persistent Storage recommended | v0.4.0 |
| Out-of-Band Snapshots | Medium | Medium | Persistent Storage | v0.5.0 |
| Checkpoint History | Low | Medium | Persistent Storage | v0.5.0 |
| Python Bindings | Low | Very High | All core features stable | v1.0.0 |
| Go Bindings | Low | Very High | Python bindings complete | v1.1.0 |
| Java Bindings | Low | Very High | Go bindings complete | v1.2.0 |

---

## Open Questions

1. **Workflow Management APIs:**
   - Should terminated workflows be prunable from state?
   - How long should workflow history be retained?

2. **Persistent Storage:**
   - Default storage path: `~/.raftoral/` or `./raftoral_data/`?
   - Automatic migration from in-memory to persistent?

3. **Scalability (Management + Execution Clusters):**
   - Execution cluster size: default to 5 nodes or make configurable?
   - Workflow failover strategy: migrate to new cluster vs. add nodes to restore quorum?
   - Execution cluster lifecycle: immediate destruction when empty or delayed (TTL)?
   - Management cluster voter selection: static assignment vs. dynamic election?
   - See detailed concerns in [SCALABILITY_ARCHITECTURE.md](./SCALABILITY_ARCHITECTURE.md)

4. **Snapshots:**
   - Snapshot compression (gzip, zstd)?
   - Incremental snapshots (delta-based)?

5. **Language Bindings:**
   - Which language to prioritize first (Python vs. Go)?
   - Should we support mixed-language clusters eventually?

---

## Contributing

This roadmap is subject to change based on community feedback and priorities. Please open GitHub issues to discuss specific features or propose alternatives.

**Last Updated:** 2025-10-10
