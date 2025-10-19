# Small Management Quorum Design

## Overview

Currently, all nodes in the management cluster are voters. This design document outlines a strategy to maintain a small, fixed-size quorum of voters (e.g., 3 or 5) in the management cluster while keeping additional nodes as learners. This improves write performance and reduces consensus overhead while maintaining fault tolerance.

## Motivation

### Current Architecture Limitations
- **All nodes are voters**: Every node participates in Raft consensus
- **Scalability concerns**: As cluster size grows, consensus becomes slower
- **Write latency**: More voters = more nodes that must acknowledge writes
- **Network overhead**: Larger quorum requires more inter-node communication

### Benefits of Small Management Quorum
- **Faster consensus**: Only 3-5 nodes need to agree for write commits
- **Lower latency**: Fewer network round-trips for proposals
- **Bounded complexity**: Consensus performance doesn't degrade with cluster size
- **Maintained availability**: 3-voter quorum tolerates 1 failure, 5-voter tolerates 2 failures
- **Read scalability**: Learners can still serve reads and execute workflows

## Design Goals

1. **Configurable quorum size**: Support 3, 5, or 7 voters (odd numbers for tie-breaking)
2. **Automatic voter promotion**: When a voter leaves, promote a learner
3. **Automatic voter demotion**: When cluster grows beyond quorum size, demote excess voters
4. **Leader preference**: Management cluster leader should ideally be a voter
5. **Geographic distribution**: Voters should be spread across failure domains (future)
6. **Zero-downtime transitions**: Membership changes should not interrupt service

## Architecture

### Node Roles

#### **Voter Nodes (3-5 nodes)**
- Participate in Raft consensus (vote in elections)
- Required for write commits (majority must acknowledge)
- Can become leader
- Typically more powerful/reliable machines
- Lower latency network connections preferred

#### **Learner Nodes (N nodes)**
- Receive log entries but don't vote
- Can serve read queries (with eventual consistency)
- Participate in workflow execution cluster
- Can be promoted to voter when needed
- Can run on commodity hardware

### Configuration

Configuration belongs in `ManagementClusterConfig` (not replicated state):

```rust
pub struct ManagementClusterConfig {
    /// Target number of voters (3, 5, or 7 recommended)
    pub target_voter_count: usize,
}

impl Default for ManagementClusterConfig {
    fn default() -> Self {
        Self {
            target_voter_count: 3,
        }
    }
}
```

**That's it!** No promotion strategies, no delays, no cooldowns. Just maintain the target count.

### State Tracking

**Key Design Principle**: Raft's `ConfState` is the single source of truth. We don't need to track anything else!

```rust
pub struct ManagementState {
    // ... existing fields ...

    // No voter metadata needed! Raft ConfState has everything.
}
```

**Seriously, we don't need any additional state.**

### Promotion Logic (Simple!)

When a voter is removed from the cluster, the leader immediately promotes an arbitrary learner:

```rust
impl ManagementCommandExecutor {
    fn on_node_removed(&self, removed_node_id: u64, is_leader: bool) {
        // Only leader handles voter replacement
        if !is_leader {
            return;
        }

        let mgmt_cluster = self.management_cluster.lock().unwrap().clone();
        if let Some(cluster) = mgmt_cluster {
            let conf_state = cluster.get_conf_state();

            // Was the removed node a voter?
            let was_voter = conf_state.voters.contains(&removed_node_id);

            if was_voter {
                let current_voter_count = conf_state.voters.len();
                let target_count = self.config.target_voter_count;

                // Do we need a replacement?
                if current_voter_count < target_count && !conf_state.learners.is_empty() {
                    // Pick first available learner (arbitrary)
                    let learner_to_promote = conf_state.learners[0];

                    slog::info!(self.logger, "Voter lost, promoting learner";
                               "removed_voter" => removed_node_id,
                               "promoted_learner" => learner_to_promote);

                    // Spawn async promotion
                    let cluster_clone = cluster.clone();
                    let logger_clone = self.logger.clone();
                    tokio::spawn(async move {
                        if let Err(e) = cluster_clone.promote_learner_to_voter(learner_to_promote).await {
                            slog::error!(logger_clone, "Failed to promote learner";
                                        "node_id" => learner_to_promote,
                                        "error" => %e);
                        }
                    });
                }
            }
        }

        // ... rest of existing cleanup logic ...
    }
}
```

**That's the entire voter management logic!** No complex strategies, no pending queues, no metadata.

## Implementation Plan

### Phase 1: Configuration (1 field!)

**Goal**: Add simple config for target voter count

**Tasks**:
1. Create `ManagementClusterConfig` with single field: `target_voter_count`
2. Add config parameter to `NodeManager::new()`
3. Store config in `ManagementCommandExecutor`

**Files to modify**:
- `src/nodemanager/node_manager.rs` - Accept config parameter
- `src/nodemanager/management_executor.rs` - Store config
- `src/nodemanager/mod.rs` - Export config type

**Deliverable**: Can configure target voter count (default: 3)

---

### Phase 2: Automatic Voter Replacement on Node Loss

**Goal**: When a voter is removed, immediately promote a learner

**Tasks**:
1. Modify `on_node_removed()` in `ManagementCommandExecutor`
2. Check if removed node was a voter (query `ConfState`)
3. If voter count < target, promote first available learner
4. Add unit test for voter replacement

**Files to modify**:
- `src/nodemanager/management_executor.rs` - Modify `on_node_removed()`

**Deliverable**: Voter count automatically maintained on node failures

---

### Phase 3: Raft Cluster Support for Promotion

**Goal**: Add `promote_learner_to_voter()` method to RaftCluster

**Tasks**:
1. Implement `promote_learner_to_voter()` in RaftCluster
2. Handle `ConfChangeType::AddNode` correctly
3. Add unit test for promotion

**Implementation**:
```rust
impl<E: CommandExecutor> RaftCluster<E> {
    /// Promote a learner to voter status
    pub async fn promote_learner_to_voter(&self, node_id: u64) -> Result<(), Box<dyn std::error::Error>> {
        use raft::prelude::ConfChangeType;

        // Create ConfChange to promote learner
        let mut cc = raft::prelude::ConfChange::default();
        cc.set_change_type(ConfChangeType::AddNode);
        cc.set_node_id(node_id);

        // Propose the change
        self.propose_conf_change(cc).await?;

        Ok(())
    }
}
```

**Files to modify**:
- `src/raft/generic/cluster.rs` - Add promotion method

**Deliverable**: Can promote learners to voters via ConfChange

---

### Phase 4: Bootstrap with Voter List

**Goal**: Allow bootstrapping with specific voter nodes

**Tasks**:
1. Add `--voter` CLI flag
2. Bootstrap first N nodes as voters
3. Additional nodes join as learners
4. Test with 3 voters + 2 learners

**Files to modify**:
- `src/main.rs` - Add `--voter` CLI flag
- `src/runtime.rs` - Handle voter vs learner join

**Deliverable**: Can bootstrap 3-voter cluster with learners

---

### Phase 5: Testing & Validation

**Goal**: Verify voter management works correctly

**Test Scenarios**:
```rust
#[tokio::test]
async fn test_voter_replacement_on_failure() {
    // Start 3-voter + 2-learner cluster
    // Kill one voter
    // Verify learner is promoted
    // Verify cluster remains available
}

#[tokio::test]
async fn test_bootstrap_with_voters() {
    // Bootstrap with --voter flag on first 3 nodes
    // Join 2 more nodes without flag (learners)
    // Verify ConfState: 3 voters, 2 learners
}
```

**Deliverable**: High confidence in voter management

---

## Summary

The simplified design is extremely minimal:

### What We Need
- **1 config field**: `target_voter_count` (default: 3)
- **0 additional state**: Raft `ConfState` is the single source of truth
- **1 event handler**: `on_node_removed()` checks if voter was lost and promotes first available learner
- **1 new method**: `promote_learner_to_voter()` in RaftCluster

### Implementation Complexity
- ~50 lines of code total
- No complex strategies
- No pending queues
- No metadata tracking
- No background tasks

### Future Enhancement
Add affinity rules to choose which learner to promote:
- Zone-aware (spread voters across availability zones)
- Load-based (promote least loaded learner)
- Pinned nodes (admin can mark specific nodes as voter-eligible)

But for now, **first available learner is fine**. Simplicity wins!

## References

- [Raft Paper Section 6: Cluster Membership Changes](https://raft.github.io/raft.pdf)
- [etcd Learner Design](https://etcd.io/docs/v3.5/learning/design-learner/)
