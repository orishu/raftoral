# Raft Dynamic Node Addition Issue

## Context: What We're Trying to Do

We're implementing a distributed workflow execution system using the `raft-rs` (v0.7.0) Rust implementation of the Raft consensus protocol. Our goal is to support **dynamic cluster membership** where nodes can join a running cluster without requiring a restart.

### Current Architecture

- **Bootstrap node**: Starts as a single-node cluster with itself in the configuration
- **Joining nodes**: Start with empty configuration, learn membership from the leader
- **Addition protocol**: Use Raft's `ConfChange` (configuration change) mechanism to add new nodes

### The Problem

When we try to add a brand-new node to a running cluster via `ConfChange`, the log replication between leader and new follower gets stuck. The new node never receives the log entries it needs, so it never applies the `ConfChange` that adds it to the cluster.

**Test Setup**:
```rust
// Step 1: Bootstrap node 1 (leader)
let cluster1 = transport1.create_cluster(1).await?; // Single-node cluster

// Step 2: Node 2 starts with empty config, knows about node 1 via transport
let cluster2 = transport2.create_cluster(2).await?; // Empty config
transport2.add_node(NodeConfig { node_id: 1, address: addr1 }).await?; // Can communicate

// Step 3: Leader proposes adding node 2
cluster1.add_node(2, addr2).await?; // Proposes ConfChangeV2

// Expected: Node 2 receives log entries, applies ConfChange, joins cluster
// Actual: Node 2 never receives entries, stays with empty config
```

## Raft-rs Implementation Details

### How raft-rs Handles Log Replication

1. **Progress Tracker**: Leader maintains a `Progress` struct for each follower:
   ```rust
   pub struct Progress {
       pub matched: u64,     // Highest log index known to be replicated
       pub next_idx: u64,    // Next log index to send
       pub state: ProgressState, // Probe, Replicate, or Snapshot
       // ...
   }
   ```

2. **Sending AppendEntries**: When leader sends entries to a follower:
   ```rust
   // Leader checks prev_log_index = next_idx - 1
   let prev_log_index = pr.next_idx - 1;
   let prev_log_term = self.raft_log.term(prev_log_index)?;

   // Sends: AppendEntries(prev_log_index, prev_log_term, entries[next_idx..])
   ```

3. **Follower Consistency Check**: Follower verifies:
   ```rust
   // Check if follower has entry at prev_log_index with matching term
   if self.raft_log.term(prev_log_index) != msg.log_term {
       // REJECT - log inconsistency
       return reject_append(prev_log_index);
   }
   ```

4. **Handling Rejections**: Leader decrements and retries:
   ```rust
   pub fn maybe_decr_to(&mut self, rejected: u64, ...) -> bool {
       // CRITICAL CHECK: Is this rejection stale?
       if (self.next_idx == 0 || self.next_idx - 1 != rejected) {
           return false;  // Rejection is STALE, ignore it!
       }

       // Rejection is valid, decrement and retry
       self.next_idx = min(rejected, match_hint + 1);
       if self.next_idx < 1 {
           self.next_idx = 1;
       }
       true // Will retry with decremented next_idx
   }
   ```

### The Index Out-of-Range Behavior

When checking terms for non-existent indices:
```rust
// From raft_log.rs
pub fn term(&self, idx: u64) -> Result<u64> {
    let dummy_idx = self.first_index() - 1;
    if idx < dummy_idx || idx > self.last_index() {
        return Ok(0u64);  // Returns 0 for out-of-range, NOT an error!
    }
    // ...
}
```

This means:
- Index 0: Always returns term 0 (dummy index)
- Indices beyond `last_index()`: Return term 0
- New nodes with empty logs: `first_index() = 1`, `last_index() = 0`

## Root Cause Analysis

### The Sequence of Events

1. **Leader proposes ConfChange**:
   ```
   Leader log: [1: term=1 (leader election), 2: term=1 (ConfChange for node 2)]
   Leader applies ConfChange, commits index 2
   ```

2. **apply_conf_change initializes Progress**:
   ```rust
   // raft-rs automatically creates Progress for new node
   let pr = Progress {
       next_idx: last_index() + 1,  // = 3
       matched: 0,
       state: Replicate,
   };
   ```

3. **Leader automatically sends AppendEntries**:
   ```
   Message: AppendEntries(
       prev_log_index: 2,  // next_idx - 1 = 3 - 1 = 2
       prev_log_term: 1,   // leader's term at index 2
       entries: [/* starting from index 3 */]
   )
   ```

4. **Follower checks prev_log**:
   ```rust
   // Follower log is EMPTY: first_index=1, last_index=0
   let my_term = self.raft_log.term(2);  // Index 2 doesn't exist
   // Returns: Ok(0) - out of range returns term 0

   // Compare: 0 != 1 (msg.log_term)
   // Result: REJECT at index 2
   ```

5. **Follower sends rejection**:
   ```
   AppendResponse(reject: true, index: 2, reject_hint: 1)
   ```

6. **Leader receives rejection** (assuming transport is set up):
   ```rust
   // Leader's Progress still has next_idx = 3
   maybe_decr_to(rejected: 2, ...) {
       // Check: next_idx - 1 != rejected?
       //        3 - 1 != 2?
       //        2 != 2? NO
       // Rejection is VALID! Should decrement...
   }
   ```

   **BUT** - If we've already modified `next_idx` to 1 (our fix attempt):
   ```rust
   maybe_decr_to(rejected: 2, ...) {
       // Check: next_idx - 1 != rejected?
       //        1 - 1 != 2?
       //        0 != 2? YES - STALE!
       return false; // Rejection ignored!
   }
   ```

### Why Our Fixes Haven't Worked

**Fix Attempt 1**: Reset `next_idx` after `apply_conf_change`
```rust
let cs = self.raft_group.apply_conf_change(&change)?;
// PROBLEM: apply_conf_change already sent AppendEntries with next_idx=3
// Now we reset to next_idx=1
if let Some(pr) = self.raft_group.raft.mut_prs().get_mut(node_id) {
    pr.next_idx = 1;  // Too late! Message already sent with index 2
}
// When rejection arrives, next_idx=1, rejected=2 → considered stale!
```

**Fix Attempt 2**: Call `send_append` manually after reset
```rust
pr.next_idx = 1;
self.raft_group.raft.send_append(node_id); // Doesn't generate new message
```
- Called during `handle_committed_entries` (inside `on_ready()` processing)
- May be suppressed due to ready state already being processed
- `send_append` only queues messages, doesn't force immediate send

**Fix Attempt 3**: Set Progress to Probe state
```rust
pr.next_idx = 1;
pr.become_probe(); // Probe state should enable retries
```
- In Probe state, only sends on heartbeat if `paused=false` AND follower behind
- Our manual setting doesn't trigger the retry logic

**Fix Attempt 4**: Set Progress to Probe + Paused
```rust
pr.become_probe();
pr.pause();  // Pause should trigger send on next heartbeat
```
- Heartbeats don't carry entries unless specific conditions met
- Pause/resume logic doesn't trigger for our case

**Fix Attempt 5**: Force Snapshot state
```rust
pr.become_snapshot(0);  // Request snapshot
```
- Snapshot requires actual snapshot generation
- Empty log doesn't trigger snapshot threshold
- No snapshot data exists to send

## What Raft-rs Examples Do

The `five_mem_node` example creates a 5-node cluster but does it differently:

```rust
// All 5 nodes start simultaneously with FULL configuration
for i in 1..=5 {
    let config = if i == 1 {
        // Leader starts knowing about all 5 nodes
        ConfState { voters: vec![1,2,3,4,5] }
    } else {
        // Followers start knowing about all 5 nodes
        ConfState { voters: vec![1,2,3,4,5] }
    };
    // Node i is created with full config from the start
}
```

**This is NOT dynamic membership!** All nodes know the full configuration from initialization. There's no example of adding a node to a running cluster via `ConfChange`.

## Current State of Our Code

### What's Working
✅ **Bidirectional transport**: Leader can send to follower AND receive responses
```rust
// In Message::AddNode handler - BEFORE proposing ConfChange
self.transport.add_peer(node_id, address.clone())?; // ← KEY FIX
// Now leader can receive rejection responses
```

✅ **Progress correction attempt**: We reset next_idx after apply
```rust
if change.change_type == ConfChangeType::AddNode && self.is_leader() {
    let node_id_to_add = change.node_id;
    if let Some(pr) = self.raft_group.raft.mut_prs().get_mut(node_id_to_add) {
        pr.next_idx = 1;  // Reset to start
        pr.become_snapshot(0); // Latest attempt: force snapshot
    }
}
```

### What's Not Working
❌ **Log replication never starts**: Only one AppendEntries sent (the automatic one with wrong index)
❌ **Rejections ignored as stale**: Because we changed next_idx after the message was sent
❌ **No retry mechanism triggered**: Manual sends, Probe state, Snapshot state all ineffective

### Test Evidence
```
# Leader log after bootstrap + ConfChange
Oct 08 17:46:xx.xxx DEBG persisted index 2, raft_id: 1  # ConfChange committed
Oct 08 17:46:xx.xxx INFO Set Progress to Snapshot state for new node, node_id: 2

# Only ONE AppendEntries sent to node 2
Oct 08 17:46:xx.xxx DEBG Sending from 1 to 2, msg: msg_type: MsgAppend to: 2
    log_term: 1 index: 2 entries {entry_type: EntryConfChangeV2 term: 1 index: 3 ...}

# Follower rejects (but this only shows in follower logs, leader may not process it)
Oct 08 17:46:xx.xxx DEBG rejected msgApp [logterm: 1, index: 2] from 1,
    logterm: Ok(0), index: 2  # Follower's index 2 has term 0 (doesn't exist)

# Then ONLY heartbeats, no more AppendEntries
Oct 08 17:46:xx.xxx DEBG Sending from 1 to 2, msg: msg_type: MsgHeartbeat
Oct 08 17:46:xx.xxx DEBG Sending from 1 to 2, msg: msg_type: MsgHeartbeat
... (continues forever)

# Final state: Node 2 never joins
Node 1 sees: [1, 2]  # Applied ConfChange locally
Node 2 sees: []      # Never received the ConfChange entry
```

## Questions for Investigation

1. **Timing of apply_conf_change send**: Can we prevent the automatic send, or must we work with it?
   - Is there a way to set Progress BEFORE calling `apply_conf_change`?
   - Can we call `apply_conf_change` without triggering immediate sends?

2. **Manual send_append not working**: Why doesn't manual `send_append` after Progress reset generate messages?
   - Is it suppressed during `on_ready()` processing?
   - Do we need to call it from outside the ready handling?
   - Should we trigger it on the next event loop iteration?

3. **Probe state mechanics**: What exactly triggers sends in Probe state?
   - Does `pause()` actually trigger sends on heartbeat?
   - What conditions must be met for heartbeat to carry entries?

4. **Snapshot mechanism**: How to properly trigger snapshot sending?
   - Must a snapshot exist before `become_snapshot()` works?
   - How to generate a snapshot for a small/empty log?
   - Is snapshot the only production-ready way to bootstrap new nodes?

5. **Alternative approaches**:
   - Should new nodes start with a minimal dummy log entry?
   - Should we use learner nodes first, then promote to voters?
   - Is there a way to add nodes without ConfChange (pre-configuration)?

## Desired Solution Properties

1. **No pre-join snapshot protocol**: Prefer to use raft-rs's built-in mechanisms
2. **Works with raft-rs 0.7.0**: Current version, stable API
3. **Handles empty logs**: Should work even with just a few entries
4. **Production-ready**: Reliable for real distributed systems
5. **Standard Raft**: Follows the Raft paper's membership change protocol

## Code Locations

- **Progress tracker**: `~/.cargo/registry/src/.../raft-0.7.0/src/tracker/progress.rs`
- **Rejection handling**: `~/.cargo/registry/src/.../raft-0.7.0/src/raft.rs` (line ~1758)
- **Log term lookup**: `~/.cargo/registry/src/.../raft-0.7.0/src/raft_log.rs` (line ~122)
- **Our ConfChange handling**: `/Users/ori/git/raftoral/src/raft/generic/node.rs` (lines ~430-445)

## Update: Attempted Solutions

### Solution 1: Add as Learner First (Grok's Recommendation)

**Approach**: Use `AddLearnerNode` instead of `AddNode` to add nodes as non-voting learners first, then promote to voters once caught up.

**Rationale**: Learners don't participate in quorum, so the first ConfChange commits with old majority. This should avoid deadlocks and allow natural Raft backtracking.

**Implementation**:
```rust
// Changed from:
change.change_type = ConfChangeType::AddNode;

// To:
change.change_type = ConfChangeType::AddLearnerNode;
```

**Result**: ❌ **Same core issue persists**
- Learners ARE added to configuration: `learners: {2}`
- AppendEntries ARE sent to learners
- Progress initialized slightly better (checking index 1 instead of 2)
- BUT: Still only ONE AppendEntries, then only heartbeats
- Rejection still doesn't trigger retry

**Evidence**:
```
Oct 08 18:15:03.524 DEBG Sending from 1 to 2, msg: msg_type: MsgAppend to: 2
    log_term: 1 index: 1 entries {...} commit: 2
Oct 08 18:15:03.605 DEBG Sending from 2 to 1, msg: msg_type: MsgAppendResponse to: 1
    index: 1 reject: true
# Then only heartbeats - no retry
Oct 08 18:15:04.003 DEBG Sending from 1 to 2, msg: msg_type: MsgHeartbeat
```

**Conclusion**: Learner vs. Voter distinction doesn't matter. The backtracking mechanism is broken regardless of node type.

### Solution 2: Force Snapshot with Many Entries

**Approach**: Add >1000 dummy entries to trigger automatic snapshot creation, hoping raft-rs would send snapshot to new nodes.

**Rationale**: If the log is large enough to have snapshots, maybe raft-rs's snapshot mechanism would kick in for new nodes.

**Implementation**:
```rust
// Added 1500 dummy checkpoint entries before adding learner
for i in 0..1500 {
    cluster1.propose_command(dummy_checkpoint).await?;
}
// Snapshot created at index 1000
// Then add learner
```

**Result**: ❌ **Snapshot created but NOT sent**
- Automatic snapshot created: `Created automatic snapshot, snapshot_index: 1000`
- Learner added successfully: `learners: {2}`
- AppendEntries sent checking index 1501 (beyond snapshot!)
- No `MsgSnapshot` messages sent
- Same pattern: one AppendEntries, rejection, then only heartbeats

**Evidence**:
```
Oct 08 18:38:14.196 INFO Created automatic snapshot, snapshot_index: 1000
Oct 08 18:37:49.978 DEBG Sending from 1 to 2, msg: msg_type: MsgAppend to: 2
    log_term: 1 index: 1501 entries {entry_type: EntryConfChangeV2 term: 1 index: 1502}
# Rejection (not shown in filtered logs)
# Then only heartbeats - no snapshot fallback
```

**Conclusion**: Having a snapshot available doesn't help. raft-rs doesn't fall back to snapshot sending because the retry mechanism never triggers.

## The Core Problem Confirmed

After testing multiple approaches, the root cause is definitively identified:

**raft-rs's `maybe_decr_to` rejection handling does NOT work for brand-new nodes with empty logs.**

The issue is NOT:
- ❌ Voter vs. Learner distinction
- ❌ Lack of snapshots
- ❌ Initial ConfState configuration
- ❌ Transport setup

The issue IS:
- ✅ `apply_conf_change` initializes `Progress.next_idx = last_index() + 1`
- ✅ First AppendEntries uses this wrong index
- ✅ Rejection is sent back but **NEVER processed correctly**
- ✅ No "received msgAppend rejection" or "decreased progress" logs appear
- ✅ No retry happens, no snapshot fallback triggers

**The mystery**: Why doesn't `handle_append_response` process the rejection?

Possible causes:
1. **Debug log level**: The "received msgAppend rejection" log is at DEBUG level, might not be enabled for raft-rs crate
2. **Silent filtering**: Some other check is silently dropping the rejection before `maybe_decr_to`
3. **State machine issue**: The rejection arrives but some state prevents processing
4. **Timing issue**: The rejection arrives after some state change that makes it invalid

## Request for Ideas

Given this detailed context, what approaches could resolve the log replication deadlock when dynamically adding nodes to a raft-rs cluster via ConfChange?

**What we've tried**:
1. ❌ Manual Progress manipulation (violates raft-rs internals)
2. ❌ AddLearnerNode instead of AddNode (same issue)
3. ❌ Forcing snapshot creation with many entries (snapshot not sent)
4. ❌ Dummy log entries at initialization (shifts problem but doesn't solve it)

**What we need**:
- A way to make raft-rs's built-in rejection handling work for empty-log nodes
- OR a way to force snapshot-based bootstrapping for new nodes
- OR understanding of why `handle_append_response` isn't processing rejections

Prefer solutions that work within raft-rs's existing mechanisms rather than building custom pre-join protocols.
