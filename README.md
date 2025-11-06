# Raftoral

A Rust library for building fault-tolerant, distributed workflows using the Raft consensus protocol.

## The Problem: Orchestration Infrastructure Overhead

Building long-running, fault-tolerant workflows typically requires deploying and managing separate orchestration infrastructure:

### Traditional Orchestration Challenges

**External Orchestrators (Temporal, AWS Step Functions, etc.):**
- ❌ **Separate Infrastructure**: Dedicated orchestrator servers and databases to deploy and maintain
- ❌ **Operational Overhead**: Another cluster to monitor, scale, backup, and upgrade
- ❌ **Network Latency**: Every workflow step requires round-trips to external orchestrator
- ❌ **Additional Failure Points**: Orchestrator availability becomes critical path

**Example Setup (Temporal):**
```
Your Services (Workers)  →  Temporal Server Cluster  →  Database (Cassandra/Postgres)
   3+ nodes                    3-5+ nodes                    3+ nodes
```

You end up managing 10+ nodes across multiple systems just to orchestrate workflows.

### The Raftoral Solution: Embedded Orchestration

Raftoral eliminates separate orchestration infrastructure by embedding the orchestrator directly into your long-running services using Raft consensus:

- ✅ **No Separate Infrastructure**: Orchestration runs inside your application processes
- ✅ **Pure Rust Library**: Just add it to your dependencies
- ✅ **Self-Coordinating**: Application nodes coordinate via Raft protocol
- ✅ **Unified Operations**: One cluster to monitor, one deployment pipeline
- ✅ **Fault Tolerant**: Automatic failover when nodes fail
- ✅ **Cloud Agnostic**: Works anywhere Rust runs

**The Architecture Difference:**
- **Traditional**: Your Services → Network → Orchestrator Cluster → Database Cluster → Network → Your Services
- **Raftoral**: Your Services (with embedded orchestration) ↔ Peer-to-Peer Coordination

**Requirements:**
- Long-running services (not FaaS/Lambda - workflows need continuous execution)
- 3+ nodes for production fault tolerance (Raft quorum requirement)
- Rust 1.70+

---

### 📊 **Comparing Workflow Systems?**

**See our detailed comparison**: [Raftoral vs. Temporal vs. DBOS](docs/COMPARISON.md)

This guide helps you choose the right workflow orchestration system by comparing architecture, scalability, complexity, use cases, and trade-offs across all three platforms.

---

## Architecture Overview

### Consensus-Driven Execution with Owner/Wait Pattern

Raftoral uses Raft consensus to coordinate workflow execution across a cluster of nodes without requiring external infrastructure. The **owner/wait pattern** ensures efficient operation in multi-node clusters:

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│   Node 1    │────▶│   Node 2    │◀────│   Node 3    │
│  (Leader)   │     │ (Follower)  │     │ (Follower)  │
│   OWNER     │     │   WAITER    │     │   WAITER    │
└─────────────┘     └─────────────┘     └─────────────┘
      ▲                   ▲                   ▲
      │                   │                   │
      └───────────────────┴───────────────────┘
           Raft Consensus Protocol
        (No external database needed)
```

**All nodes execute workflows in parallel, but only the owner proposes state changes:**

1. **Workflow Start**: Any node can initiate a workflow by proposing a `WorkflowStart` command through Raft
2. **Parallel Execution**: Once committed via consensus, ALL nodes execute the workflow function
3. **Owner Proposes, Others Wait**:
   - **Owner node** (typically the starter) proposes checkpoint and completion commands
   - **Non-owner nodes** wait for checkpoint events from Raft consensus
   - Eliminates 50-75% of redundant Raft proposals
4. **Automatic Failover**: If owner fails, non-owner detects timeout and becomes new owner

**Key Benefits:**
- **Load Distribution**: Computation happens on all nodes, not just the leader
- **Fault Tolerance**: Any node can complete a workflow if the owner fails
- **Efficient Consensus**: Only owner proposes state changes, reducing Raft traffic
- **No External Dependencies**: Everything runs in your application process

### Multi-Cluster Scalability

For large deployments (20+ nodes), Raftoral uses a **two-tier architecture** to prevent checkpoint replication from overwhelming the cluster:

```
┌─────────────────────────────────────────────────────────┐
│         Management Cluster (cluster_id = 0)              │
│   Tracks topology & coordinates multiple exec clusters   │
│         Voters: 3-5 nodes  |  Learners: N nodes          │
└─────────────────────────────────────────────────────────┘
                          │
         ┌────────────────┼────────────────┐
         │                │                │
┌─────────────┐   ┌─────────────┐   ┌─────────────┐
│ Exec Cluster│   │ Exec Cluster│   │ Exec Cluster│
│   (ID: 1)   │   │   (ID: 2)   │   │   (ID: 3)   │
│  5 nodes    │   │  5 nodes    │   │  5 nodes    │
│ ┌─────────┐ │   │ ┌─────────┐ │   │ ┌─────────┐ │
│ │Workflows│ │   │ │Workflows│ │   │ │Workflows│ │
│ │ + Chkpts│ │   │ │ + Chkpts│ │   │ │ + Chkpts│ │
│ └─────────┘ │   │ └─────────┘ │   │ └─────────┘ │
└─────────────┘   └─────────────┘   └─────────────┘
```

**How It Works:**
- **Management Cluster**: Tracks which nodes belong to which execution clusters (O(N×C) state)
- **Execution Clusters**: Small clusters (3-5 nodes) that execute workflows independently
- **Round-Robin Selection**: Workflows distributed across execution clusters for load balancing
- **No Global Workflow Tracking**: Execution clusters own their workflows (no O(W) state in management)
- **Request Forwarding**: Automatic forwarding of queries to nodes with the target execution cluster

**Scalability Benefits:**
```
Single 50-node cluster:
  - Checkpoint replication: 50x per checkpoint
  - State: O(W) workflows tracked globally

Multi-cluster (10 exec clusters × 5 nodes):
  - Checkpoint replication: 5x per checkpoint  (10x reduction!)
  - State: O(C×N) clusters×nodes  (massive reduction for high workflow count)
  - Each node in ~2-3 execution clusters
```

**See [docs/SCALABILITY_ARCHITECTURE.md](docs/SCALABILITY_ARCHITECTURE.md) for detailed architecture.**

### Replicated Variables vs. Temporal "Activities"

If you're familiar with Temporal, Raftoral's **replicated variables** serve a similar purpose to **Activities**, but with a different philosophy:

#### Temporal Activities
```typescript
// External service call with retries
const result = await workflow.executeActivity('chargeCard', {
  amount: 100,
  retries: 3
});
```
- Separate execution contexts (workflow vs. activity workers)
- Network calls to external services with retry policies
- Activity results stored in Temporal's database

#### Raftoral Replicated Variables
```rust
// Deterministic computation with consensus-backed checkpoints
let mut amount = ReplicatedVar::with_value("charge_amount", &ctx, 100).await?;

let result = ReplicatedVar::with_computation("payment_result", &ctx, || async {
    charge_card(*amount).await  // External call executed once (owner only)
}).await?;
```

**Key Differences:**

| Aspect | Temporal Activities | Raftoral Replicated Variables |
|--------|---------------------|-------------------------------|
| **Execution Model** | Separate worker pools | Same process, all nodes execute |
| **State Storage** | External database | Raft consensus (in-memory + snapshots) |
| **Side Effects** | Activity-specific retry logic | `with_computation()` for one-time execution |
| **Network Overhead** | Every activity call | Only during checkpoint creation (owner-only) |
| **Determinism** | Activities can be non-deterministic | Workflow code must be deterministic |

**When to use `with_value()` vs `with_computation()`:**
- **`ReplicatedVar::with_value("key", &ctx, value)`**: For deterministic state (counters, status, computed values)
- **`ReplicatedVar::with_computation("key", &ctx, || async { ... })`**: For side effects (API calls, external services)
  - Executes the computation **once** (on the owner node only)
  - Result is replicated to all nodes via Raft
  - Non-owner nodes wait for the checkpoint event
  - Subsequent accesses use the cached result

**Example - Payment Processing:**
```rust
runtime.register_workflow_closure("process_payment", 1,
    |input: PaymentInput, ctx: WorkflowContext| async move {
        // Deterministic state
        let order_id = ReplicatedVar::with_value("order_id", &ctx, input.order_id).await?;
        let amount = ReplicatedVar::with_value("amount", &ctx, input.amount).await?;

        // Side effect: charge card once (owner-only execution)
        let charge_result = ReplicatedVar::with_computation("charge", &ctx, || async {
            stripe::charge_card(*order_id, *amount).await
        }).await?;

        // Update based on result
        let mut status = ReplicatedVar::with_value("status", &ctx,
            if charge_result.is_ok() { "completed" } else { "failed" }
        ).await?;

        Ok(PaymentOutput { status: status.get() })
    }
)?;
```

**Why This Matters:**
- **No Activity Workers**: No separate processes to manage
- **No Task Queues**: No polling infrastructure needed
- **All-in-One**: Orchestration and execution in the same binary
- **Type Safety**: Rust's type system ensures correctness at compile time
- **Efficient**: Owner/wait pattern minimizes redundant Raft proposals

## Quick Start

### Bootstrap a Cluster

```rust
use raftoral::workflow2::{WorkflowRuntime, WorkflowContext, ReplicatedVar};
use raftoral::raft::generic2::{RaftNodeConfig, TransportLayer, GrpcTransport};
use tokio::signal;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // 1. Create transport and runtime
    let config = RaftNodeConfig {
        node_id: 1,
        cluster_id: 1,
        ..Default::default()
    };

    let transport = Arc::new(TransportLayer::new(Arc::new(
        GrpcTransport::new("127.0.0.1:7001".to_string())
    )));

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let logger = create_logger();

    let (runtime, node) = WorkflowRuntime::new(config, transport, rx, logger)?;
    let runtime = Arc::new(runtime);

    // 2. Register workflow with replicated variables
    runtime.register_workflow_closure(
        "process_order", 1,
        |input: OrderInput, ctx: WorkflowContext| async move {
            // Regular replicated variable for deterministic state
            let mut status = ReplicatedVar::with_value("status", &ctx, "processing").await?;

            // Computed replicated variable for side effects (API calls)
            let inventory_check = ReplicatedVar::with_computation(
                "inventory",
                &ctx,
                || async {
                    check_inventory_service(input.item_id).await
                }
            ).await?;

            if *inventory_check {
                status.set("confirmed").await?;
            } else {
                status.set("out_of_stock").await?;
            }

            Ok(OrderOutput { status: status.get() })
        }
    ).await?;

    // 3. Run node and wait for shutdown
    tokio::spawn(async move {
        let _ = RaftNode::run_from_arc(node).await;
    });

    signal::ctrl_c().await?;
    Ok(())
}
```

### Execute a Workflow

```rust
// From ANY node in the cluster
let input = OrderInput {
    order_id: "ORD-123".to_string(),
    item_id: "ITEM-456".to_string(),
};

let workflow = runtime
    .start_workflow::<OrderInput, OrderOutput>(
        "my-workflow-1".to_string(),
        "process_order".to_string(),
        1,
        input
    )
    .await?;

let output = workflow.wait_for_completion().await?;
println!("Order status: {}", output.status);
```

## Dynamic Cluster Management

One of Raftoral's key strengths is **dynamic cluster membership** - you can start with any cluster size and safely add or remove nodes at runtime.

### Start With Any Size

```bash
# Start with a single node (development)
./raftoral --listen 127.0.0.1:7001 --bootstrap

# Or start with 3 nodes (production)
./raftoral --listen 127.0.0.1:7001 --bootstrap
./raftoral --listen 127.0.0.1:7002 --peers 127.0.0.1:7001
./raftoral --listen 127.0.0.1:7003 --peers 127.0.0.1:7001
```

### Add Nodes Dynamically

New nodes can join a running cluster and **automatically catch up** on in-flight workflows:

```rust
// New node joins cluster
let (runtime, node) = WorkflowRuntime::new_joining_node(
    config,
    transport,
    rx,
    vec![1, 2],  // Initial voter IDs
    logger
)?;

// Node discovers cluster configuration, gets assigned node ID,
// and receives Raft snapshot to catch up on running workflows
```

**What Happens During Join:**
1. **Discovery**: New node contacts seed nodes to discover cluster
2. **Node ID Assignment**: Receives unique ID (highest known + 1)
3. **Configuration Update**: Leader proposes ConfChange to add node as voter
4. **Snapshot Transfer**: Leader sends Raft snapshot containing:
   - Active workflow states
   - Checkpoint queues for in-flight workflows (late follower catch-up)
   - Cluster configuration
5. **Sync**: New node applies snapshot and starts executing workflows

**Raft's Native Snapshot Mechanism:**
- No custom state transfer protocol needed
- Works for **any** workflow state, regardless of size
- Handles network failures with automatic retries
- Consistent snapshots (point-in-time cluster state)

### The Catch-Up Problem (Solved)

**Challenge**: What if a node joins while workflows are running with lots of checkpoints?

**Solution: Checkpoint Queues + Owner/Wait Pattern**

```rust
// Workflow running on nodes 1, 2, 3:
for i in 0..1000 {
    counter.set(i).await?;  // Creates 1000 checkpoints
}

// Node 4 joins after 500 iterations:
// - Receives snapshot with checkpoint queues containing values 0-500
// - Starts executing at iteration 0
// - Pops from queue instead of waiting for owner: instant catch-up!
// - Joins live execution at iteration 500+
```

**Technical Details:**
- **Checkpoint History**: Owner tracks all checkpoints with log indices
- **Queue Reconstruction**: Snapshot includes queues for active workflows
- **FIFO Ordering**: Deterministic execution ensures queue order matches execution order
- **Lazy Consumption**: Values only popped when workflow execution reaches that point
- **Owner-Only Cleanup**: Owner cleans its own queued values to prevent self-consumption

**Result**: New nodes can join a cluster with running workflows and seamlessly catch up without blocking the cluster or missing state.

## Workflow Versioning

Workflows evolve over time - you add features, fix bugs, change behavior. Raftoral handles this through **explicit versioning** with a migration path for long-running workflows.

### The Problem

```rust
// Version 1 (deployed in production with running workflows)
runtime.register_workflow_closure("process_order", 1, |input, ctx| async {
    let status = ReplicatedVar::with_value("status", &ctx, "processing").await?;
    // ...original logic...
});

// Later: You want to add fraud detection
// But some workflows started with v1 and are still running!
```

### The Solution: Side-by-Side Versions

**Best Practice**: Register both old and new versions during rollout:

```rust
// Version 1 - Keep running for in-flight workflows
runtime.register_workflow_closure("process_order", 1, |input, ctx| async {
    let status = ReplicatedVar::with_value("status", &ctx, "processing").await?;
    // ...original logic...
    Ok(OrderOutput { status: status.get() })
}).await?;

// Version 2 - New workflows use this
runtime.register_workflow_closure("process_order", 2, |input, ctx| async {
    let status = ReplicatedVar::with_value("status", &ctx, "processing").await?;

    // NEW: Fraud detection
    let fraud_check = ReplicatedVar::with_computation("fraud_check", &ctx, || async {
        fraud_service::check(input.order_id).await
    }).await?;

    if !*fraud_check {
        status.set("fraud_detected").await?;
        return Ok(OrderOutput { status: status.get() });
    }

    // ...rest of logic...
    Ok(OrderOutput { status: status.get() })
}).await?;
```

**Deployment Strategy:**

1. **Phase 1 - Deploy with Both Versions**:
   ```bash
   # All nodes run with v1 and v2 registered
   # New workflows use v2, old workflows continue with v1
   ```

2. **Phase 2 - Wait for v1 Workflows to Complete**:
   ```bash
   # Monitor running workflows
   # Wait for all v1 instances to finish naturally
   ```

3. **Phase 3 - Remove v1**:
   ```rust
   // Only register v2 in new deployments
   runtime.register_workflow_closure("process_order", 2, /* ... */).await?;
   ```

**Why Explicit Versioning:**
- ✅ **Safe Rollouts**: Old workflows unaffected by new code
- ✅ **Clear Intent**: Version numbers make upgrade paths obvious
- ✅ **Gradual Migration**: No "big bang" deployments required
- ✅ **Rollback Support**: Can revert to old version if issues arise

## Running Examples

```bash
# Simple workflow example
cargo run --example typed_workflow_example

# Run tests
cargo test

# Two-node cluster test
./scripts/test_two_node_cluster.sh
```

## Advanced Configuration

### In-Memory Network (Testing)

```rust
use raftoral::raft::generic2::{InProcessNetwork, InProcessNetworkSender, TransportLayer};
use raftoral::workflow2::WorkflowRuntime;

// Create shared network
let network = Arc::new(InProcessNetwork::new());

// Create transport for node 1
let (tx1, rx1) = mpsc::channel(100);
network.register_node(1, tx1.clone()).await;

let transport1 = Arc::new(TransportLayer::new(Arc::new(InProcessNetworkSender::new(
    network.clone(),
))));

let config1 = RaftNodeConfig {
    node_id: 1,
    cluster_id: 1,
    ..Default::default()
};

let (runtime1, node1) = WorkflowRuntime::new(config1, transport1, rx1, logger)?;

// Execute workflows in-memory (no network)
let workflow = runtime1.start_workflow("wf-1", "my_workflow", 1, input).await?;
let result = workflow.wait_for_completion().await?;
```

## Technical Details

### Performance
- **Command Processing**: 30-171µs (microseconds)
- **Event-Driven**: Zero polling overhead
- **Owner/Wait Pattern**: 50-75% reduction in Raft proposals
- **Optimized For**: Orchestration-heavy workflows (not high-frequency trading)

### Requirements
- **Rust**: 1.70 or later
- **Deterministic Execution**: Same input → same operation sequence on all nodes
- **Serializable State**: Types must implement `Serialize + Deserialize`
- **Type Safety**: Full compile-time checking

### Current Limitations
- In-memory storage only (persistent storage planned)
- No built-in compensation/rollback (implement in workflow logic)
- Workflow functions must be registered identically on all nodes

## File Organization

```
src/
├── raft/generic2/
│   ├── node.rs                # RaftNode with raft-rs integration
│   ├── proposal_router.rs     # ProposalRouter for command submission
│   ├── transport.rs           # Transport abstraction (Layer 2-3)
│   ├── server/
│   │   ├── in_process.rs      # InProcessNetwork for testing
│   │   └── grpc.rs            # gRPC transport implementation
│   ├── message.rs             # Message types & CommandExecutor trait
│   ├── errors.rs              # Error types
│   ├── cluster_router.rs      # Multi-cluster message routing
│   └── integration_tests.rs   # Two-node KV cluster tests
├── workflow2/
│   ├── mod.rs                 # Public API exports
│   ├── runtime.rs             # WorkflowRuntime with owner/wait pattern
│   ├── state_machine.rs       # WorkflowStateMachine & commands
│   ├── context.rs             # WorkflowContext & WorkflowRun
│   ├── registry.rs            # Type-safe workflow storage
│   ├── replicated_var.rs      # ReplicatedVar with with_value/with_computation
│   ├── event.rs               # WorkflowEvent definitions
│   ├── error.rs               # Error types
│   └── ownership.rs           # Workflow ownership tracking
├── nodemanager/
│   ├── mod.rs                 # NodeManager (dual-cluster coordination)
│   ├── node_manager.rs        # Owns management + execution clusters
│   ├── management_command.rs  # Management cluster commands
│   └── management_executor.rs # Management state & execution
├── grpc2/
│   └── server.rs              # gRPC service implementation
└── lib.rs                     # Public API exports

examples/
├── typed_workflow_example.rs  # Complete workflow example
└── ...

docs/
├── SCALABILITY_ARCHITECTURE.md  # Multi-cluster architecture details
└── COMPARISON.md                # Raftoral vs Temporal vs DBOS
```

## Contributing

Contributions welcome! Areas of interest:
- Multi-node fault injection testing
- Persistent storage backend integration
- Advanced workflow patterns
- Performance benchmarking
- Documentation improvements

## Author

**Ori Shalev** - [ori.shalev@gmail.com](mailto:ori.shalev@gmail.com)

## License

MIT License. See [LICENSE](LICENSE) for details.

## Acknowledgments

- Built on [raft-rs](https://github.com/tikv/raft-rs) for Raft consensus
- Inspired by [Temporal](https://temporal.io/) workflow orchestration
- Uses [Tokio](https://tokio.rs/) for async runtime
