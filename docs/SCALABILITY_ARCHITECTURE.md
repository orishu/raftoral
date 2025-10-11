# Scalability Architecture: Management + Execution Clusters

## Overview

This document describes Raftoral's scalability architecture designed to support large auto-scaled deployments (dozens to hundreds of nodes) without overwhelming the cluster with checkpoint replication traffic.

### The Problem

**Current Architecture Limitations:**
- Single Raft cluster includes all nodes
- Every checkpoint is replicated to all nodes via Raft consensus
- Auto-scaled deployment with 50 nodes → every checkpoint broadcasts to 50 nodes
- Checkpoint throughput inversely proportional to cluster size
- Large quorums (26/50 nodes) slow down consensus

**Example Scenario:**
```
Auto-scaled deployment: 50 pods
Workflows per second: 100
Checkpoints per workflow: 10
Total checkpoint broadcasts: 100 * 10 * 50 = 50,000 messages/second
```

This doesn't scale.

### The Solution: Two-Tier Cluster Architecture

**Management Cluster:**
- Single global cluster including ALL nodes in the deployment
- Manages virtual execution cluster membership
- Tracks workflow lifecycle (start/end) across all execution clusters
- Small voter set (3-5 voters) + many learners → fast quorum

**Execution Clusters:**
- Multiple small virtual clusters (3-5 nodes each)
- Handle workflow execution and checkpoint replication
- Isolated consensus domains → checkpoints don't cross cluster boundaries
- Dynamically created and destroyed based on workload

**Key Insight:** Decouple cluster size from checkpoint throughput by partitioning workflows across small execution clusters.

---

## Architecture Components

### 1. Management Cluster

**Type:** `RaftCluster<ManagementCommandExecutor>`

**Responsibilities:**
- Track all nodes in the deployment
- Create/destroy virtual execution clusters
- Assign nodes to execution clusters
- Monitor workflow lifecycle globally
- Coordinate cluster rebalancing

**Command Set:**

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ManagementCommand {
    /// Create a new virtual execution cluster
    CreateExecutionCluster(CreateExecutionClusterData),

    /// Destroy an execution cluster (must be empty of workflows)
    DestroyExecutionCluster(ExecutionClusterId),

    /// Associate a node with an execution cluster
    AssociateNode(AssociateNodeData),

    /// Disassociate a node from an execution cluster
    DisassociateNode(DisassociateNodeData),

    /// Report workflow started on an execution cluster
    ReportWorkflowStarted(WorkflowLifecycleData),

    /// Report workflow ended on an execution cluster
    ReportWorkflowEnded(WorkflowLifecycleData),

    /// Promote learner to voter or vice versa
    ChangeNodeRole(ChangeNodeRoleData),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateExecutionClusterData {
    pub cluster_id: Uuid,
    pub initial_node_ids: Vec<u64>,  // 3-5 nodes
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AssociateNodeData {
    pub cluster_id: Uuid,
    pub node_id: u64,
    pub role: NodeRole,  // Voter or Learner
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DisassociateNodeData {
    pub cluster_id: Uuid,
    pub node_id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowLifecycleData {
    pub workflow_id: Uuid,
    pub cluster_id: Uuid,
    pub workflow_type: String,
    pub version: u32,
    pub timestamp: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangeNodeRoleData {
    pub node_id: u64,
    pub new_role: NodeRole,
}

pub type ExecutionClusterId = Uuid;
```

**State Management:**

```rust
pub struct ManagementState {
    /// All execution clusters
    pub execution_clusters: HashMap<Uuid, ExecutionClusterInfo>,

    /// Node membership: node_id → set of execution cluster IDs
    pub node_memberships: HashMap<u64, HashSet<Uuid>>,

    /// Workflow registry: workflow_id → cluster_id
    pub workflow_locations: HashMap<Uuid, Uuid>,

    /// Node roles in management cluster
    pub node_roles: HashMap<u64, NodeRole>,
}

pub struct ExecutionClusterInfo {
    pub cluster_id: Uuid,
    pub node_ids: Vec<u64>,
    pub active_workflows: HashSet<Uuid>,
    pub created_at: u64,
}
```

**Voter/Learner Configuration:**
```
Example with 50 nodes:
- Voters: 5 nodes (quorum = 3)
- Learners: 45 nodes

Management command commits require 3/5 voters (fast!)
All 50 nodes receive log for observability
```

---

### 2. Execution Clusters

**Type:** `RaftCluster<WorkflowCommandExecutor>`

**Responsibilities:**
- Execute workflows assigned to this cluster
- Replicate checkpoints within cluster only (3-5 nodes)
- Report workflow lifecycle to management cluster
- Handle workflow failover within cluster

**Command Set:**

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WorkflowCommand {
    WorkflowStart(WorkflowStartData),
    WorkflowEnd(WorkflowEndData),
    SetCheckpoint(CheckpointData),
}

// Same as current implementation, but workflow execution
// happens only on nodes in this execution cluster
```

**Cluster Lifecycle:**

1. **Creation:**
   - Management cluster proposes `CreateExecutionCluster` with 3-5 node IDs
   - Selected nodes initialize local `RaftCluster<WorkflowCommandExecutor>` instance
   - Cluster ID used to identify this virtual cluster

2. **Workflow Assignment:**
   - New workflow request arrives at any node
   - Node queries management cluster for available execution cluster
   - Workflow started on selected execution cluster
   - Management cluster records `ReportWorkflowStarted`

3. **Execution:**
   - Only nodes in the execution cluster participate in consensus
   - Checkpoints replicate to 3-5 nodes (not entire deployment!)
   - Workflow completes, reports `ReportWorkflowEnded` to management cluster

4. **Destruction:**
   - When execution cluster has zero active workflows
   - Management cluster proposes `DestroyExecutionCluster`
   - Nodes clean up local `RaftCluster` instance

---

### 3. Unified Transport Layer

**Challenge:** Support multiple cluster types over same gRPC connections.

**Solution:** Extend RaftService to route messages based on cluster type and ID.

#### Updated Protobuf Schema

```protobuf
service RaftService {
  // Existing workflow messages
  rpc SendMessage(RaftMessage) returns (google.protobuf.Empty);

  // NEW: Management cluster messages
  rpc SendManagementMessage(ManagementRaftMessage) returns (google.protobuf.Empty);
}

message RaftMessage {
  bytes message = 1;  // Serialized raft::prelude::Message
  string execution_cluster_id = 2;  // UUID of target execution cluster
}

message ManagementRaftMessage {
  bytes message = 1;  // Serialized raft::prelude::Message
  // No cluster_id needed - always routes to management cluster
}
```

#### Transport Routing Logic

```rust
pub struct UnifiedTransport {
    /// Management cluster (single instance per node)
    management_cluster: Arc<RaftCluster<ManagementCommandExecutor>>,

    /// Execution clusters this node participates in
    /// Key: ExecutionClusterId (Uuid)
    execution_clusters: Arc<RwLock<HashMap<Uuid, Arc<RaftCluster<WorkflowCommandExecutor>>>>>,

    /// gRPC clients to other nodes
    peer_clients: Arc<RwLock<HashMap<u64, RaftServiceClient<Channel>>>>,
}

impl UnifiedTransport {
    /// Route workflow message to appropriate execution cluster
    async fn handle_workflow_message(
        &self,
        cluster_id: Uuid,
        raft_message: raft::prelude::Message,
    ) -> Result<(), TransportError> {
        let clusters = self.execution_clusters.read().await;

        if let Some(cluster) = clusters.get(&cluster_id) {
            cluster.step(raft_message).await?;
            Ok(())
        } else {
            Err(TransportError::ClusterNotFound(cluster_id))
        }
    }

    /// Route management message to management cluster
    async fn handle_management_message(
        &self,
        raft_message: raft::prelude::Message,
    ) -> Result<(), TransportError> {
        self.management_cluster.step(raft_message).await?;
        Ok(())
    }

    /// Send workflow message to peer node
    async fn send_workflow_message(
        &self,
        to_node_id: u64,
        cluster_id: Uuid,
        message: raft::prelude::Message,
    ) -> Result<(), TransportError> {
        let client = self.get_peer_client(to_node_id).await?;

        let raft_msg = RaftMessage {
            message: message.encode_to_vec(),
            execution_cluster_id: cluster_id.to_string(),
        };

        client.send_message(raft_msg).await?;
        Ok(())
    }

    /// Send management message to peer node
    async fn send_management_message(
        &self,
        to_node_id: u64,
        message: raft::prelude::Message,
    ) -> Result<(), TransportError> {
        let client = self.get_peer_client(to_node_id).await?;

        let mgmt_msg = ManagementRaftMessage {
            message: message.encode_to_vec(),
        };

        client.send_management_message(mgmt_msg).await?;
        Ok(())
    }
}
```

**Key Design Point:** Same TCP connections, same node IDs, different Raft state machines.

---

### 4. Workflow Lifecycle Flow

#### End-to-End Example: Starting a Workflow

```
1. Application calls: runtime.start_workflow("order_processing", input)
   ↓
2. Local node queries management cluster state:
   - Find execution cluster with capacity (e.g., cluster_abc has 2/10 workflows)
   - If no cluster available, create new one via ManagementCommand::CreateExecutionCluster
   ↓
3. Node proposes WorkflowStart to execution cluster_abc:
   - Only 3-5 nodes in cluster_abc participate in consensus
   - Checkpoint replication limited to cluster_abc
   ↓
4. Node reports to management cluster:
   ManagementCommand::ReportWorkflowStarted {
       workflow_id: wf_123,
       cluster_id: cluster_abc,
       ...
   }
   ↓
5. Workflow executes with checkpoints:
   - checkpoint!(ctx, "step1", value) → replicated to 3-5 nodes in cluster_abc
   - NOT replicated to all 50 nodes in deployment!
   ↓
6. Workflow completes, proposes WorkflowEnd to cluster_abc
   ↓
7. Node reports to management cluster:
   ManagementCommand::ReportWorkflowEnded {
       workflow_id: wf_123,
       cluster_id: cluster_abc,
       ...
   }
```

**Message Counts:**
- **Old approach:** 100 workflows * 10 checkpoints * 50 nodes = 50,000 messages
- **New approach:** 100 workflows * 10 checkpoints * 5 nodes = 5,000 messages
- **10x reduction in checkpoint traffic!**

---

### 5. Node Structure

Each node in the deployment runs:

```rust
pub struct RaftoralNode {
    /// Node ID (unique across deployment)
    node_id: u64,

    /// Management cluster (participates as voter or learner)
    management_cluster: Arc<RaftCluster<ManagementCommandExecutor>>,

    /// Execution clusters this node is part of
    /// Updated dynamically via ManagementCommand::AssociateNode/DisassociateNode
    execution_clusters: Arc<RwLock<HashMap<Uuid, Arc<RaftCluster<WorkflowCommandExecutor>>>>>,

    /// Unified transport layer
    transport: Arc<UnifiedTransport>,

    /// Workflow runtime (public API)
    workflow_runtime: Arc<WorkflowRuntime>,
}

impl RaftoralNode {
    /// Handle management command application
    async fn on_management_command(&self, cmd: ManagementCommand) {
        match cmd {
            ManagementCommand::AssociateNode(data) if data.node_id == self.node_id => {
                // Join a new execution cluster
                let cluster = RaftCluster::join_existing(
                    data.cluster_id,
                    self.node_id,
                    self.transport.clone(),
                ).await?;

                self.execution_clusters.write().await
                    .insert(data.cluster_id, Arc::new(cluster));
            }

            ManagementCommand::DisassociateNode(data) if data.node_id == self.node_id => {
                // Leave an execution cluster
                let mut clusters = self.execution_clusters.write().await;
                if let Some(cluster) = clusters.remove(&data.cluster_id) {
                    cluster.shutdown().await?;
                }
            }

            _ => {
                // Other commands just update management state
            }
        }
    }
}
```

---

### 6. Execution Cluster Selection Strategy

**Goal:** Balance workflow load across execution clusters.

**Strategies:**

#### Strategy 1: Round-Robin
```rust
fn select_execution_cluster(state: &ManagementState) -> Option<Uuid> {
    state.execution_clusters
        .values()
        .filter(|ec| ec.active_workflows.len() < MAX_WORKFLOWS_PER_CLUSTER)
        .min_by_key(|ec| ec.active_workflows.len())
        .map(|ec| ec.cluster_id)
}
```

#### Strategy 2: Locality-Aware
```rust
fn select_local_execution_cluster(
    state: &ManagementState,
    local_node_id: u64,
) -> Option<Uuid> {
    // Prefer execution clusters this node is already part of
    state.node_memberships
        .get(&local_node_id)?
        .iter()
        .filter_map(|cluster_id| state.execution_clusters.get(cluster_id))
        .filter(|ec| ec.active_workflows.len() < MAX_WORKFLOWS_PER_CLUSTER)
        .min_by_key(|ec| ec.active_workflows.len())
        .map(|ec| ec.cluster_id)
}
```

#### Strategy 3: Workload-Type Affinity
```rust
fn select_by_workflow_type(
    state: &ManagementState,
    workflow_type: &str,
) -> Option<Uuid> {
    // Colocate similar workflow types for cache locality
    // (workflow registry, code paths, etc.)
    state.execution_clusters
        .values()
        .filter(|ec| {
            // Check if cluster already runs this workflow type
            ec.active_workflows.iter().any(|wf_id| {
                state.workflow_locations.get(wf_id)
                    .map(|loc| loc.workflow_type == workflow_type)
                    .unwrap_or(false)
            })
        })
        .min_by_key(|ec| ec.active_workflows.len())
        .map(|ec| ec.cluster_id)
}
```

**Configuration:**
```rust
pub struct ExecutionClusterPolicy {
    pub max_workflows_per_cluster: usize,  // e.g., 20
    pub target_cluster_size: usize,        // e.g., 5 nodes
    pub selection_strategy: SelectionStrategy,
    pub rebalance_threshold: f64,          // e.g., 0.3 (30% imbalance triggers rebalance)
}
```

---

### 7. Dynamic Rebalancing

**Scenario:** Deployment scales from 10 nodes to 50 nodes.

**Challenge:** Existing execution clusters stuck with original nodes, new nodes idle.

**Solution:** Gradual rebalancing via management cluster.

#### Rebalancing Algorithm

```rust
async fn rebalance_execution_clusters(
    state: &ManagementState,
    all_node_ids: &[u64],
) -> Vec<ManagementCommand> {
    let mut commands = Vec::new();

    // 1. Identify overloaded execution clusters
    let overloaded: Vec<_> = state.execution_clusters
        .values()
        .filter(|ec| ec.active_workflows.len() > MAX_WORKFLOWS_PER_CLUSTER)
        .collect();

    // 2. Find underutilized nodes
    let node_load: HashMap<u64, usize> = all_node_ids.iter().map(|&node_id| {
        let cluster_count = state.node_memberships
            .get(&node_id)
            .map(|clusters| clusters.len())
            .unwrap_or(0);
        (node_id, cluster_count)
    }).collect();

    let idle_nodes: Vec<_> = node_load.iter()
        .filter(|(_, &count)| count == 0)
        .map(|(&node_id, _)| node_id)
        .collect();

    // 3. Create new execution clusters with idle nodes
    if !idle_nodes.is_empty() && !overloaded.is_empty() {
        let new_cluster_id = Uuid::new_v4();
        let selected_nodes = idle_nodes.iter().take(5).copied().collect();

        commands.push(ManagementCommand::CreateExecutionCluster(
            CreateExecutionClusterData {
                cluster_id: new_cluster_id,
                initial_node_ids: selected_nodes,
            }
        ));
    }

    commands
}
```

**Trigger:** Periodic background task (every 30s) or on deployment scale events.

---

## Implementation Plan

### Phase 1: Management Cluster Foundation (4 weeks)

**Milestone 1.1: ManagementCommandExecutor**
- Define `ManagementCommand` enum
- Implement `CommandExecutor` for management commands
- Basic state management (execution clusters, node memberships)

**Milestone 1.2: Dual-Cluster Node**
- Extend `RaftoralNode` to run both management + execution clusters
- Implement dynamic execution cluster join/leave
- Unit tests: create cluster, associate/disassociate nodes

**Milestone 1.3: Voter/Learner Configuration**
- Extend `RaftoralConfig` to specify management cluster role
- Bootstrap with 3 voters + N learners
- Test: 10-node management cluster with 3 voters

### Phase 2: Unified Transport (3 weeks)

**Milestone 2.1: Update Protobuf Schema**
- Add `execution_cluster_id` to `RaftMessage`
- Add `ManagementRaftMessage` type
- Regenerate gRPC code

**Milestone 2.2: UnifiedTransport Implementation**
- Route messages to correct cluster based on type + ID
- Maintain single set of gRPC connections per peer
- Error handling for unknown cluster IDs

**Milestone 2.3: Integration Testing**
- 3-node cluster: send both management and workflow messages
- Verify routing correctness
- Performance test: measure routing overhead (<1% acceptable)

### Phase 3: Workflow Lifecycle Integration (3 weeks)

**Milestone 3.1: Execution Cluster Selection**
- Implement round-robin selection strategy
- Auto-create execution clusters when none available
- Update `WorkflowRuntime::start_workflow` to select cluster

**Milestone 3.2: Lifecycle Reporting**
- Report `WorkflowStarted` to management cluster
- Report `WorkflowEnded` to management cluster
- Query management state for workflow location

**Milestone 3.3: End-to-End Testing**
- Start workflow on 50-node deployment
- Verify checkpoint replication limited to 5-node execution cluster
- Measure message count reduction vs. single-cluster baseline

### Phase 4: Dynamic Rebalancing (4 weeks)

**Milestone 4.1: Load Monitoring**
- Track workflows per execution cluster
- Track node membership counts
- Expose metrics via management state

**Milestone 4.2: Rebalancing Logic**
- Implement periodic rebalancing task
- Create new clusters with idle nodes
- Avoid rebalancing during high load

**Milestone 4.3: Testing & Validation**
- Scale deployment 10 → 50 nodes, verify rebalancing
- Scale down 50 → 10 nodes, verify cluster consolidation
- Chaos testing: random node failures during rebalancing

---

## Configuration Example

```rust
// Bootstrap a 50-node deployment with management cluster

// Voters (3 nodes)
let voter_config = RaftoralConfig::bootstrap("127.0.0.1:5001".to_string(), Some(1))
    .with_management_role(NodeRole::Voter)
    .with_execution_cluster_policy(ExecutionClusterPolicy {
        max_workflows_per_cluster: 20,
        target_cluster_size: 5,
        selection_strategy: SelectionStrategy::RoundRobin,
        rebalance_threshold: 0.3,
    });

let voter1 = RaftoralGrpcRuntime::start(voter_config).await?;

// Learners (47 nodes)
let learner_config = RaftoralConfig::join(
    "127.0.0.1:5047".to_string(),
    vec!["127.0.0.1:5001".to_string()],
)
    .with_management_role(NodeRole::Learner);

let learner47 = RaftoralGrpcRuntime::start(learner_config).await?;

// Start workflow - automatically creates execution cluster
let workflow_run = voter1
    .workflow_runtime()
    .start_workflow::<OrderInput, OrderOutput>("order_processing", 1, input)
    .await?;

// Behind the scenes:
// 1. Management cluster creates execution cluster (5 random nodes)
// 2. Workflow starts on execution cluster
// 3. Checkpoints replicate to 5 nodes only
// 4. Management cluster tracks workflow lifecycle
```

---

## Performance Analysis

### Scalability Metrics

| Deployment Size | Single Cluster | Management + Execution | Improvement |
|-----------------|----------------|------------------------|-------------|
| 10 nodes        | 10x replication| 5x replication         | 2x          |
| 20 nodes        | 20x replication| 5x replication         | 4x          |
| 50 nodes        | 50x replication| 5x replication         | 10x         |
| 100 nodes       | 100x replication| 5x replication        | 20x         |

**Checkpoint Throughput:**
- Single 50-node cluster: ~1,000 checkpoints/sec (limited by broadcast)
- Management + execution: ~10,000 checkpoints/sec (10x improvement)

**Quorum Latency:**
- Management cluster (3 voters): ~5ms quorum
- Execution cluster (5 nodes): ~3ms quorum
- Single 50-node cluster: ~15ms quorum (26/50 nodes)

### Memory Footprint

**Per Node:**
- Management cluster state: ~10 MB (tracks all execution clusters)
- Execution cluster state (each): ~5 MB (workflows + checkpoints)
- Node in 3 execution clusters: 10 + (3 * 5) = 25 MB

**Total Deployment (50 nodes, 10 execution clusters):**
- Management state replicated: 50 * 10 MB = 500 MB
- Execution state: 10 clusters * 5 nodes * 5 MB = 250 MB
- **Total: 750 MB** (vs. 50 nodes * 50 MB = 2.5 GB for single cluster)

---

## Concerns and Open Issues

### 1. Workflow Failover Complexity

**Issue:** When an execution cluster loses quorum (e.g., 2/5 nodes fail), how do we recover workflows?

**Options:**

**A) Migrate workflows to different execution cluster:**
- Management cluster detects failed execution cluster
- Selects new execution cluster with capacity
- Problem: Workflow state (checkpoints) lost! Need to replay from start.
- Requires persistent storage (see FEATURE_PLAN.md #2)

**B) Add nodes to failed execution cluster:**
- Management cluster adds healthy nodes to restore quorum
- New nodes catch up via Raft snapshots
- Workflows resume after quorum restored
- Preferred approach for now

**Recommendation:** Implement option B first, option A requires persistent storage.

### 2. Cross-Cluster Queries

**Issue:** How to query "list all running workflows" when they're spread across 10+ execution clusters?

**Solution:**
- Management cluster maintains `workflow_locations` map
- Query management state for workflow IDs + cluster IDs
- Fan out queries to specific execution clusters for details
- Eventual consistency acceptable (management cluster may lag)

### 3. Execution Cluster Lifecycle Management

**Issue:** When to destroy empty execution clusters?

**Options:**
- Immediate: destroy as soon as last workflow ends
  - Pro: Minimal resource usage
  - Con: Churn if workflows come in bursts

- Delayed: keep cluster alive for N minutes after idle
  - Pro: Avoids create/destroy churn
  - Con: Idle resources

**Recommendation:** Configurable TTL (default: 5 minutes idle before destroy)

### 4. Snapshot Coordination

**Issue:** Management cluster snapshots are separate from execution cluster snapshots.

**Scenario:**
- Node restarts, loads management snapshot
- Management state says "node is in execution cluster_xyz"
- But node has no local state for cluster_xyz (not in execution snapshot)

**Solution:**
- On restart, node loads management snapshot
- Discovers execution cluster memberships
- Re-joins each execution cluster (fetches snapshot from peers)
- Similar to new node joining existing cluster

### 5. Network Partition Handling

**Issue:** Management cluster and execution cluster may see different partitions.

**Scenario:**
```
Nodes 1-3: Management voters (partition A)
Nodes 4-8: Execution cluster voters (partition B)
Network partition splits A and B
```

**Behavior:**
- Management cluster continues (has quorum 2/3)
- Execution cluster continues (has quorum 3/5)
- Workflows proceed, but management cluster can't track lifecycle

**Mitigation:**
- Eventual consistency model: when partition heals, catch up on lifecycle events
- Management cluster state is advisory, not authoritative
- Execution clusters are source of truth for active workflows

### 6. Workflow Routing Overhead

**Issue:** Starting a workflow requires:
1. Query management cluster for available execution cluster
2. Possibly create new execution cluster (another round-trip)
3. Start workflow on selected cluster

**Latency:** ~3 round-trips vs. 1 for single-cluster model

**Mitigation:**
- Cache execution cluster availability locally (refresh every 1s)
- Optimistic start: assume cluster has capacity, retry if full
- Pre-create execution clusters during idle periods

### 7. Execution Cluster Size Trade-offs

**Question:** Why 3-5 nodes? Why not 2 or 7?

**Analysis:**
- 2 nodes: No fault tolerance (quorum = 2/2)
- 3 nodes: Tolerates 1 failure (quorum = 2/3) ✓
- 5 nodes: Tolerates 2 failures (quorum = 3/5) ✓
- 7 nodes: Tolerates 3 failures (quorum = 4/7) but more replication overhead

**Recommendation:** Default to 5 nodes (2-failure tolerance), configurable via policy.

### 8. Management Cluster Voter Selection

**Issue:** Which nodes should be management voters?

**Options:**

**A) Static assignment:**
- First 3 nodes in deployment are voters
- Simple but fragile (if those 3 fail, cluster stuck)

**B) Dynamic election:**
- Raft's normal leader election picks voters
- Problem: How to ensure odd number?

**C) External coordination:**
- Deployment manager (K8s operator) designates voters
- Voters labeled via pod annotations

**Recommendation:** Option C for production, option A for development.

### 9. Monotonic Workflow ID Assignment

**Issue:** Workflow IDs are UUIDs (random). Hard to correlate with time or order.

**Alternative:**
- Management cluster assigns sequential IDs
- Requires consensus for every workflow start (slower)

**Recommendation:** Keep UUIDs for now, add timestamp to `WorkflowLifecycleData`.

### 10. Metrics and Observability

**Issue:** With 10+ execution clusters, how to monitor health?

**Requirements:**
- Per-cluster metrics: workflow count, checkpoint rate, quorum latency
- Per-node metrics: execution cluster memberships, message rate
- Global metrics: total workflows, cluster utilization

**Solution:**
- Expose Prometheus metrics from each node
- Aggregate by cluster_id label
- Management cluster state provides global view

---

## Migration Path from Single-Cluster

**For existing deployments running single-cluster architecture:**

### Phase 1: Introduce Management Cluster (backward compatible)
1. Deploy management cluster alongside existing workflow cluster
2. Management cluster initially empty (no execution clusters)
3. Existing workflows continue on legacy single cluster

### Phase 2: Migrate Workflows to Execution Clusters
1. Create execution clusters via management commands
2. New workflows start on execution clusters
3. Legacy workflows drain naturally (complete on single cluster)
4. Once all legacy workflows done, shut down single cluster

### Phase 3: Full Management Mode
1. All workflows on execution clusters
2. Management cluster handles all routing
3. Remove single-cluster code paths

**Timeline:** 3-6 months for large production deployments

---

## Alternatives Considered

### Alternative 1: Hierarchical Raft (Tree Structure)

```
       Management Cluster (root)
         /      |       \
    Region1  Region2  Region3
    /    \   /    \   /    \
  EC1   EC2 EC3  EC4 EC5  EC6
```

**Pros:**
- Scales to 1000+ nodes
- Regional isolation

**Cons:**
- Much more complex
- Multi-hop latency for cross-region workflows
- Harder to reason about consistency

**Verdict:** Overkill for current use case (50-100 nodes). Revisit if we need >500 nodes.

### Alternative 2: Consistent Hashing for Workflow Assignment

**Idea:** Hash workflow_id → execution cluster (no management cluster needed)

**Pros:**
- Simpler: no management cluster
- Deterministic assignment

**Cons:**
- No load balancing (hash might create hotspots)
- Rigid cluster membership (resharding is hard)
- No observability of workflow distribution

**Verdict:** Works for stateless services, not for stateful workflows.

### Alternative 3: Per-Workflow-Type Clusters

**Idea:** Each workflow type gets dedicated execution cluster

**Pros:**
- Resource isolation by workflow type
- Predictable performance

**Cons:**
- Wastes resources (cluster per type, even if 1 workflow)
- Doesn't scale with many workflow types

**Verdict:** Useful for very high-priority workflows, but not general solution.

---

## Summary

The **Management + Execution Clusters** architecture provides:

✅ **10-20x improvement in checkpoint throughput** for large deployments
✅ **Decouples cluster size from consensus latency** via small execution clusters
✅ **Dynamic scaling** via execution cluster creation/destruction
✅ **Backward compatible** with migration path from single-cluster
✅ **Flexible voter/learner configuration** for management cluster efficiency

**Trade-offs:**
❌ Additional complexity (two cluster types)
❌ Workflow failover requires coordination
❌ Slight latency overhead for workflow routing

**Recommended for:** Auto-scaled deployments with 20+ nodes and high workflow throughput.

**Not recommended for:** Small deployments (<10 nodes) - stick with single-cluster architecture.

---

**Last Updated:** 2025-10-10
