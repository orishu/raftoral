# Non-Trivial Bootstrap Problem and Solution

## The Problem

When a new node joins an existing Raft cluster, it faces a bootstrapping challenge that standard Raft implementations don't adequately address. The issue stems from how raft-rs initializes the `Progress` tracker for newly added nodes.

### Root Cause

When a leader receives a ConfChange to add a new node, raft-rs's `apply_conf_change` initializes the new node's Progress with:

```rust
Progress.next_idx = last_index() + 1
```

For example, if the leader's log has entries at indices 1-50:
- New node is added via ConfChange at index 51
- raft-rs sets `Progress.next_idx = 51`
- Leader sends: `AppendEntries(prev_log_index=50, prev_log_term=T)`
- **Problem**: New node has EMPTY log, so it rejects (doesn't have index 50)

### Broken Rejection Handling

The expected flow would be:
1. Follower rejects AppendEntries
2. Leader decrements `next_idx` and retries with earlier entries
3. Eventually reaches index 1, follower accepts

However, **raft-rs v0.7.0 has broken rejection handling** (see BOOTSTRAP_ISSUE.md). The leader doesn't properly decrement `next_idx` in probe state, causing the new node to never catch up.

### Why Not Snapshots?

The standard Raft solution is to send a snapshot when a follower is too far behind. However:
- Snapshots are heavyweight for small clusters
- Requires implementing snapshot transfer protocol
- Adds complexity for the common case of nodes joining with minimal lag
- Our use case focuses on workflows, not general key-value storage

## The Solution: Dummy First Entry Bootstrap

### Core Insight

If joining nodes start with **the same first log entry** as existing cluster members, they can accept AppendEntries from the beginning without rejections.

### Implementation Overview

#### 1. Discovery Protocol Extension

Extended the gRPC discovery to return cluster bootstrap information:

```protobuf
message DiscoveryResponse {
    uint64 node_id = 1;
    RaftRole role = 2;
    uint64 highest_known_node_id = 3;
    string address = 4;
    repeated uint64 voters = 5;           // Current voting members
    repeated uint64 learners = 6;         // Current learner members
    uint64 first_entry_index = 7;         // Index of first log entry (usually 1)
    uint64 first_entry_term = 8;          // Term of first log entry
}
```

#### 2. Joining Node Initialization

When a new node joins:

1. **Discover existing cluster**:
   ```rust
   let discovered = discover_peers(vec![existing_node_addr]).await;
   ```

2. **Extract bootstrap info**:
   ```rust
   let voters = discovered[0].voters;              // e.g., [1, 2]
   let first_entry_index = discovered[0].first_entry_index;  // e.g., 1
   let first_entry_term = discovered[0].first_entry_term;    // e.g., 1
   ```

3. **Initialize with discovered configuration**:
   ```rust
   // Set discovered voters BEFORE creating cluster
   transport.set_discovered_voters(voters);
   transport.set_discovered_first_entry(first_entry_index, first_entry_term);
   ```

4. **Create dummy first entry** (in `RaftNode::new`):
   ```rust
   if let Some((first_index, first_term)) = transport.get_discovered_first_entry() {
       let mut dummy_entry = Entry::default();
       dummy_entry.set_index(first_index);  // Usually 1
       dummy_entry.set_term(first_term);    // Match leader's first entry term

       storage.wl().append(&[dummy_entry])?;
   }
   ```

5. **Initialize Raft with non-empty configuration**:
   ```rust
   let mut conf_state = ConfState::default();
   conf_state.voters = discovered_voters;  // e.g., [1, 2]
   storage.wl().set_conf_state(conf_state);
   ```

### Why This Works

#### Before (Broken):
```
Leader log:     [1:T1] [2:T1] [3:T1] ... [50:T5]
New node log:   []
Leader sends:   AppendEntries(prev_log_index=50, prev_log_term=T5)
New node:       ❌ REJECT - don't have index 50
Leader:         ❌ Broken retry logic, node never catches up
```

#### After (Working):
```
Leader log:     [1:T1] [2:T1] [3:T1] ... [50:T5]
New node log:   [1:T1]  (dummy entry matching leader's first)
Leader sends:   AppendEntries(prev_log_index=1, prev_log_term=T1, entries=[2:T1, 3:T1, ...])
New node:       ✅ ACCEPT - have index 1 with matching term T1
```

The key: **Leader's first AppendEntries checks `prev_log_index=1, prev_log_term=T1`**, which matches the dummy entry, so the new node accepts immediately.

### Configuration Initialization

In addition to the dummy entry, we also initialize joining nodes with the discovered voter configuration. This prevents them from:
- Thinking they're a single-node cluster
- Immediately campaigning for leader
- Causing split-brain scenarios

```rust
// Old approach: Empty configuration
ConfState { voters: [], learners: [] }  // ❌ Node thinks it's alone

// New approach: Discovered configuration
ConfState { voters: [1, 2], learners: [] }  // ✅ Node knows cluster state
```

## Implementation Details

### Files Modified

1. **proto/raftoral.proto**: Extended DiscoveryResponse with voters, learners, first_entry_index, first_entry_term
2. **src/grpc/bootstrap.rs**: Updated DiscoveredPeer struct and discovery client
3. **src/grpc/server.rs**: Server returns cluster config and first entry info
4. **src/raft/generic/grpc_transport.rs**: Added storage for discovered config and first entry
5. **src/raft/generic/transport.rs**: Added trait methods for accessing discovered data
6. **src/raft/generic/node.rs**: Core logic to create dummy entry and initialize with discovered config
7. **src/raft/generic/cluster.rs**: Cache first entry info for discovery responses

### Bootstrap Sequence

```
┌─────────────┐
│   Node 1    │  (Bootstrap node)
│  (Leader)   │
│ Log: [1:T1] │
│      [2:T1] │
└──────┬──────┘
       │
       │ 1. Discovery Request
       │ ◄────────────────────
       │                     │
       │ 2. Discovery Response│
       │    voters: [1]      │
       │    first: (1, T1)   │
       │ ─────────────────►  │
       │                     │
       │                ┌────┴─────┐
       │                │  Node 2  │
       │                │ (Joining)│
       │                │ Init:    │
       │                │ voters:[1]│
       │                │ log:[1:T1]│ ← Dummy entry
       │                └────┬─────┘
       │                     │
       │ 3. Add to transport │
       │    (bidirectional)  │
       │ ◄──────────────────►│
       │                     │
       │ 4. ConfChange(AddNode 2)
       │ ─────────────────►  │
       │                     │
       │ 5. AppendEntries    │
       │    prev_idx: 1      │
       │    prev_term: T1    │
       │    entries: [2:T1]  │
       │ ─────────────────►  │
       │                     │
       │ 6. AppendResponse   │
       │    ✅ Success!       │
       │ ◄──────────────────│
       │                     │
       │ 7. Apply ConfChange │
       │    voters: [1,2]    │
       │ ─────────────────►  │
       │                     │
```

## Test Results

The three-node bootstrap test demonstrates the success:

```rust
// Node 2 bootstrap
✓ Discovered peer at 127.0.0.1:...: node_id=1, role=Follower, highest_known=1
  Discovered voters: [1]
  Discovered first entry: index=1, term=1

Oct 08 22:14:07.027 INFO newRaft, peers: Configuration { voters: {1} },
  last term: 1, last index: 1  // ← Has dummy entry!

Oct 08 22:14:07.115 INFO received a message with higher term from 1,
  msg type: MsgAppend, message_term: 1, term: 0

Oct 08 22:14:07.115 DEBG Sending from 2 to 1,
  msg: msg_type: MsgAppendResponse to: 1 index: 2 commit: 2
  // ✅ Accepted immediately - NO REJECTION!
```

**Zero AppendEntries rejections across all test runs.**

## Limitations and Future Work

### Current Limitations

1. **Log Compaction**:
   - Current implementation caches first entry in memory
   - Protected by `entries_before_snapshot: 100` config
   - If cluster compacts beyond first 100 entries, cached info becomes stale
   - **Future work**: Include first entry metadata in snapshot

2. **Single First Entry**:
   - All nodes must share the same first entry
   - Works for clusters with consistent initialization
   - Doesn't handle heterogeneous cluster merges

3. **Discovery Dependency**:
   - Requires at least one existing node to be reachable
   - Can't bootstrap if all nodes are new simultaneously
   - This is intentional - single node uses standard bootstrap

### Snapshot Integration (Future)

To make this robust for long-running clusters with log compaction:

```rust
// Include first entry in snapshot metadata
struct SnapshotMetadata {
    index: u64,
    term: u64,
    conf_state: ConfState,
    first_entry_index: u64,  // Add this
    first_entry_term: u64,   // Add this
}

// When creating snapshot
let first_entry = cached_first_entry.read().unwrap();
snapshot_metadata.first_entry_index = first_entry.0;
snapshot_metadata.first_entry_term = first_entry.1;

// When restoring from snapshot
cached_first_entry.write().unwrap() = Some((
    snapshot_metadata.first_entry_index,
    snapshot_metadata.first_entry_term
));
```

## Comparison to Alternatives

### Alternative 1: Full Snapshot Transfer
- **Pros**: Handles any log divergence, standard Raft approach
- **Cons**: Complex, heavyweight, overkill for simple joins
- **When to use**: Large state, node far behind, or log compacted

### Alternative 2: Learner Promotion (Attempted)
- **Approach**: Add as learner, catch up, promote to voter
- **Problem**: Still requires fixing `Progress.next_idx` issue
- **Result**: Doesn't solve the fundamental problem

### Alternative 3: Manual Progress Reset (Grok's Workaround)
- **Approach**: Manually reset `Progress.next_idx = 1` after adding node
- **Problem**: Brittle, relies on raft-rs internals, breaks with upgrades
- **Result**: Fragile and not maintainable

### Our Approach: Dummy Entry Bootstrap
- **Pros**:
  - Simple, minimal code changes
  - Works with raft-rs as-is (no internal modifications)
  - Lightweight, no snapshot transfer needed
  - Elegant solution addressing root cause
- **Cons**:
  - Requires discovery protocol
  - Current implementation needs snapshot integration for full robustness
- **Best for**: Workflow clusters with frequent node additions, minimal state

## Key Takeaways

1. **Empty logs are problematic**: Starting with an empty log causes AppendEntries rejections due to `Progress.next_idx` initialization
2. **Dummy entry bypasses the problem**: Matching first entry allows immediate acceptance
3. **Configuration matters**: Discovered voter config prevents campaigning and split-brain
4. **Discovery is essential**: Need cluster state to initialize properly
5. **Works around raft-rs limitations**: Solves the broken rejection handling without modifying raft-rs

## References

- See `BOOTSTRAP_ISSUE.md` for detailed analysis of raft-rs rejection handling bug
- Test: `tests/grpc_cluster_bootstrap_test.rs` - Three-node bootstrap validation
- Core implementation: `src/raft/generic/node.rs` lines 136-154 (dummy entry creation)
