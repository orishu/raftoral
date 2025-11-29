# Raftoral

A Rust library for building fault-tolerant, distributed workflows using Raft consensus.

## The Problem: Workflow Infrastructure is Getting Complicated

The workflow orchestration landscape is evolving rapidly:

**Traditional Approach (Temporal, AWS Step Functions)**:
- Separate orchestration infrastructure (10+ nodes across multiple systems)
- Dedicated databases, message queues, worker pools
- Complex operational overhead

**Recent Innovation (Vercel's `use workflow` directive)**:
- Elegant developer experience with magical syntax
- But requires serverless platform lock-in (Vercel's infrastructure)
- Limited control over execution environment
- Vendor-specific deployment model

**What's Missing?**
A solution that combines:
- ✅ **Self-hosted**: Full control over your infrastructure
- ✅ **Embedded**: No separate orchestrator to deploy
- ✅ **Type-safe**: Compile-time correctness
- ✅ **Fault-tolerant**: Built-in consensus and automatic failover
- ✅ **Cloud-agnostic**: Works anywhere Rust runs

## The Raftoral Solution: Embedded Orchestration with Raft Consensus

Raftoral eliminates separate orchestration infrastructure by embedding the orchestrator directly into your long-running services using a **novel dual-layer Raft architecture**:

### Core Innovation: Two Layers of Raft

**Management Layer (cluster_id = 0)**:
- Tracks cluster topology and node membership
- Routes workflows to execution clusters
- Lightweight coordination (no workflow state)

**Execution Layer (cluster_id = 1+)**:
- Small clusters (3-5 nodes) that execute workflows
- Independent consensus per cluster
- Workflow state isolated to execution clusters

This architecture enables:
- **Horizontal Scalability**: Add execution clusters as you grow
- **Fault Isolation**: Execution cluster failures don't affect management
- **Efficient Replication**: Checkpoints only replicate within small clusters (5x, not 50x)
- **Zero External Dependencies**: Everything runs in your application process

```
┌──────────────────────────────────────────────────────────┐
│           Management Cluster (cluster_id = 0)            │
│         Coordinates topology across all nodes            │
│            Voters: 3-5  |  Learners: N nodes             │
└────────────────────────┬─────────────────────────────────┘
                         │
        ┌────────────────┼────────────────┐
        │                │                │
┌───────▼──────┐  ┌──────▼──────┐  ┌─────▼───────┐
│ Exec Cluster │  │ Exec Cluster│  │ Exec Cluster│
│   (ID: 1)    │  │   (ID: 2)   │  │   (ID: 3)   │
│  5 nodes     │  │  5 nodes    │  │  5 nodes    │
│              │  │             │  │             │
│ ┌──────────┐ │  │ ┌─────────┐ │  │ ┌─────────┐ │
│ │Workflows │ │  │ │Workflows│ │  │ │Workflows│ │
│ │   +      │ │  │ │   +     │ │  │ │   +     │ │
│ │Checkpts  │ │  │ │Checkpts │ │  │ │Checkpts │ │
│ └──────────┘ │  │ └─────────┘ │  │ └─────────┘ │
└──────────────┘  └─────────────┘  └─────────────┘
```

**Key Benefits:**
- ✅ **No Separate Infrastructure**: Orchestration runs inside your application
- ✅ **Pure Rust Library**: Just add to `Cargo.toml`
- ✅ **Self-Coordinating**: Nodes coordinate via Raft consensus
- ✅ **Automatic Failover**: Workflows survive node failures
- ✅ **Horizontal Scaling**: Add execution clusters as workload grows
- ✅ **Cloud Agnostic**: Deploy anywhere Rust runs

**Requirements:**
- Long-running services (not FaaS/Lambda - workflows need continuous execution)
- 3+ nodes for production fault tolerance (Raft quorum requirement)
- Rust 1.70+

> **⚡ Ready to start?** Jump to the [Quick Start](#quick-start) section below or see the [complete setup guide](docs/QUICKSTART.md).

---

## Architecture Overview

### Layered Architecture: Shared Infrastructure

Raftoral uses a **clean layered architecture** where management and execution clusters share infrastructure but maintain independent consensus:

```
┌───────────────────────────────────────────────────────────┐
│  Layer 7: Application Runtime                             │
│  • ManagementRuntime: add_node(), create_cluster()        │
│  • WorkflowRuntime: start_workflow(), checkpoint()        │
└───────────────────────────────────────────────────────────┘
                         ↓ propose commands
┌───────────────────────────────────────────────────────────┐
│  Layer 6: Proposal Router                                 │
│  • Routes proposals to Raft leader (local or remote)      │
│  • Tracks sync waiters for proposal confirmation          │
└───────────────────────────────────────────────────────────┘
                         ↓ forward to Raft
┌───────────────────────────────────────────────────────────┐
│  Layer 5: Event Bus                                       │
│  • Broadcasts state changes to subscribers                │
│  • Decouples state machine from upper layers              │
└───────────────────────────────────────────────────────────┘
                         ↑ emit events  ↓ subscribe
┌───────────────────────────────────────────────────────────┐
│  Layer 4: State Machine                                   │
│  • Applies committed commands to authoritative state      │
│  • ManagementStateMachine: topology tracking              │
│  • WorkflowStateMachine: workflow execution state         │
└───────────────────────────────────────────────────────────┘
                         ↑ apply(entry)
┌───────────────────────────────────────────────────────────┐
│  Layer 3: Raft Node                                       │
│  • Wraps raft-rs RawNode                                  │
│  • Independent consensus per cluster_id                   │
│  • Applies committed entries to state machine             │
└───────────────────────────────────────────────────────────┘
                         ↓ send  ↑ receive
┌───────────────────────────────────────────────────────────┐
│  Layer 2: Cluster Router                                  │
│  • Routes messages by cluster_id to correct Raft node     │
│  • Shared across all clusters on this node                │
└───────────────────────────────────────────────────────────┘
                         ↓ outbound  ↑ inbound
┌───────────────────────────────────────────────────────────┐
│  Layer 1: Transport Layer                                 │
│  • Protocol-agnostic message sending/receiving            │
│  • Maintains peer registry (node_id → address)            │
│  • Shared by all clusters                                 │
└───────────────────────────────────────────────────────────┘
                         ↓ network I/O
┌───────────────────────────────────────────────────────────┐
│  Layer 0: Server Layer (Protocol Implementation)          │
│  • gRPC: Production deployment                            │
│  • HTTP: Alternative protocol (CORS-friendly)             │
│  • InProcess: Testing without network overhead            │
└───────────────────────────────────────────────────────────┘
```

**Key Design Principles:**
- **Unidirectional Flow**: Commands flow down, events flow up
- **No Callbacks**: Lower layers never call upper layers directly
- **Event-Driven**: State changes propagate via Event Bus
- **Shared Infrastructure**: Transport, Cluster Router, and Server layers shared across all clusters
- **Independent Consensus**: Each cluster has its own Raft log and leader

### Dual-Cluster Architecture on a Single Node

```
    Management Runtime          Workflow Runtime
    (Layer 7)                   (Layer 7)
         │                           │
         ▼                           ▼
    Proposal Router            Proposal Router
    (cluster_id=0)             (cluster_id=1)
         │                           │
         ▼                           ▼
    Management Raft            Workflow Raft
    (Layer 3)                  (Layer 3)
         │                           │
         └───────────┬───────────────┘
                     │
         ┌───────────▼──────────┐
         │   Cluster Router     │
         │      (Layer 2)       │
         │  Routes by cluster_id│
         └───────────┬──────────┘
                     │
         ┌───────────▼──────────┐
         │   Transport Layer    │
         │      (Layer 1)       │
         │   Peer Registry      │
         └───────────┬──────────┘
                     │
         ┌───────────▼──────────┐
         │   gRPC/HTTP Server   │
         │      (Layer 0)       │
         └──────────────────────┘
```

**Novel Aspect**: Management and execution clusters share transport/routing but maintain completely independent Raft consensus. This enables:
- **Scalability**: Management cluster stays small (5 voters) while execution clusters multiply
- **Fault Isolation**: Execution cluster issues don't affect topology management
- **Efficient State**: Management only tracks O(N×C) node-cluster mappings, not O(W) workflow states

### Consensus-Driven Execution with Owner/Wait Pattern

Raftoral uses Raft consensus to coordinate workflow execution across a cluster. The **owner/wait pattern** ensures efficient operation:

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

**How It Works:**
1. **Workflow Start**: Any node proposes a `WorkflowStart` command through Raft
2. **Parallel Execution**: ALL nodes execute the workflow function deterministically
3. **Owner Proposes, Others Wait**:
   - **Owner node** (typically the starter) proposes checkpoint commands
   - **Non-owner nodes** wait for checkpoint events from consensus
   - Eliminates 50-75% of redundant Raft proposals
4. **Automatic Failover**: If owner fails, non-owner detects timeout and takes over

**Benefits:**
- **Load Distribution**: Computation happens on all nodes
- **Fault Tolerance**: Any node can complete a workflow if owner fails
- **Efficient Consensus**: Only owner proposes state changes
- **No External Dependencies**: Everything runs in-process

### Multi-Cluster Scalability

For large deployments, Raftoral's dual-layer architecture prevents checkpoint replication overhead:

**Single 50-node cluster problems**:
- Checkpoint replication: 50x per checkpoint
- State: O(W) workflows tracked globally
- Raft log grows with every workflow checkpoint

**Multi-cluster solution (10 exec clusters × 5 nodes)**:
- Checkpoint replication: 5x per checkpoint (10x reduction!)
- State: O(C×N) clusters×nodes in management
- Each execution cluster: isolated Raft log
- Each node participates in ~2-3 execution clusters

**See [docs/SCALABILITY_ARCHITECTURE.md](docs/SCALABILITY_ARCHITECTURE.md) for detailed architecture.**

---

## Quick Start

> **📚 Full Getting Started Guide**: See [docs/QUICKSTART.md](docs/QUICKSTART.md) for complete setup instructions including:
> - Starting clusters with gRPC or HTTP transport
> - Running workflows via command-line tools
> - Multi-node cluster deployment
> - Production configuration with persistent storage

### Bootstrap a Cluster

```rust
use raftoral::full_node::FullNode;
use raftoral::{checkpoint, checkpoint_compute};
use slog::{Drain, o};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create logger
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    let logger = slog::Logger::root(drain, o!());

    // Bootstrap first node
    let node = FullNode::new(
        1,                          // node_id
        "127.0.0.1:7001".to_string(), // address
        None,                       // storage_path (in-memory)
        logger.clone()
    ).await?;

    // Register workflow with checkpoints
    node.workflow_registry().lock().await.register_workflow_closure(
        "process_order", 1,
        |input: OrderInput, ctx| async move {
            // Regular checkpoint for deterministic state
            let mut status = checkpoint!(ctx, "status", "processing");

            // Computed checkpoint for side effects (e.g., API calls)
            // Executes once on owner, replicated to all nodes
            let inventory = checkpoint_compute!(ctx, "inventory", || async {
                check_inventory_service(input.item_id).await
            });

            if *inventory {
                status.set("confirmed").await?;
            } else {
                status.set("out_of_stock").await?;
            }

            Ok(OrderOutput { status: status.get() })
        }
    ).await?;

    // Node runs indefinitely
    tokio::signal::ctrl_c().await?;
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
        "my-order-1".to_string(),
        "process_order".to_string(),
        1, // version
        input
    )
    .await?;

let output = workflow.wait_for_completion().await?;
println!("Order status: {}", output.status);
```

### Understanding Checkpoints

Raftoral provides two checkpoint macros for different use cases:

#### `checkpoint!` - Deterministic State

Use for values that are computed identically on all nodes:

```rust
use raftoral::checkpoint;

// Simple values
let order_id = checkpoint!(ctx, "order_id", input.order_id);
let amount = checkpoint!(ctx, "amount", input.amount);

// Mutable state
let mut counter = checkpoint!(ctx, "counter", 0);
counter.set(counter.get() + 1).await?;

// Computed deterministically
let total = checkpoint!(ctx, "total", *amount * 2);
```

**Key:** All nodes compute the same value, so only the owner proposes it to Raft.

#### `checkpoint_compute!` - Side Effects (API Calls)

Use for one-time execution of side effects (API calls, external services):

```rust
use raftoral::checkpoint_compute;

// Execute once on owner node, replicate result to all nodes
let payment_result = checkpoint_compute!(ctx, "payment", || async {
    stripe::charge_card(*order_id, *amount).await
});

// Owner executes the closure, gets the result
// Non-owners wait for the checkpoint event from Raft
// All nodes end up with the same payment_result
```

**How it works:**
1. **Owner node**: Executes the closure and proposes result via Raft
2. **Non-owner nodes**: Wait for checkpoint event (don't execute closure)
3. **Result**: All nodes have the same `payment_result` value

**Example - Payment Processing:**

```rust
runtime.register_workflow_closure("process_payment", 1,
    |input: PaymentInput, ctx| async move {
        // Deterministic values (all nodes compute)
        let order_id = checkpoint!(ctx, "order_id", input.order_id);
        let amount = checkpoint!(ctx, "amount", input.amount);

        // Side effect: charge card once (owner-only execution)
        let charge_result = checkpoint_compute!(ctx, "charge", || async {
            stripe::charge_card(*order_id, *amount).await
        });

        // Update based on result (deterministic)
        let status = checkpoint!(ctx, "status",
            if charge_result.success { "completed" } else { "failed" }
        );

        Ok(PaymentOutput { status: status.get() })
    }
)?;
```

### Comparing to Temporal

If you're familiar with Temporal, here's the mapping:

| Temporal | Raftoral |
|----------|----------|
| `workflow.executeActivity('charge', {...})` | `checkpoint_compute!(ctx, "charge", \|\| async { charge_card().await })` |
| Activity result from database | Result from Raft consensus |
| Activity workers (separate process) | Owner node (same process) |
| Activity retries via Temporal | Retry logic in your closure |

**Key Difference**: Raftoral executes everything in your application process. No separate activity workers, no external database.

---

## Dynamic Cluster Management

> **💡 Tip**: See [docs/QUICKSTART.md](docs/QUICKSTART.md) for complete CLI reference and deployment examples.

### Start With Any Size

```bash
# Single node (development)
./raftoral --listen 127.0.0.1:7001 --bootstrap

# Three nodes (production)
./raftoral --listen 127.0.0.1:7001 --bootstrap
./raftoral --listen 127.0.0.1:7002 --peers 127.0.0.1:7001
./raftoral --listen 127.0.0.1:7003 --peers 127.0.0.1:7001
```

### Add Nodes Dynamically

New nodes automatically join and catch up on in-flight workflows via Raft snapshots:

```rust
// New node discovers cluster and joins
let (runtime, node) = ManagementRuntime::new_joining_node(
    config,
    transport,
    mailbox_rx,
    vec![1, 2], // Existing voter IDs
    cluster_router,
    shared_config,
    logger
)?;
```

**What happens:**
1. Node contacts seed nodes for discovery
2. Receives unique node ID
3. Leader proposes ConfChange to add node
4. Raft sends snapshot with active workflow states
5. Node starts executing workflows

---

## Workflow Versioning

Register multiple versions side-by-side for safe rollouts:

```rust
// Version 1 - Keep running for in-flight workflows
runtime.register_workflow_closure("process_order", 1, |input, ctx| async {
    let status = checkpoint!(ctx, "status", "processing");
    // ...original logic...
    Ok(output)
}).await?;

// Version 2 - New workflows use this
runtime.register_workflow_closure("process_order", 2, |input, ctx| async {
    let status = checkpoint!(ctx, "status", "processing");

    // NEW: Fraud detection
    let fraud_check = checkpoint_compute!(ctx, "fraud", || async {
        fraud_service::check(input.order_id).await
    });

    if !*fraud_check {
        status.set("fraud_detected").await?;
        return Ok(output);
    }

    // ...rest of logic...
    Ok(output)
}).await?;
```

**Deployment strategy**: Deploy both versions → wait for v1 to complete → remove v1

---

## Running Examples

> **🚀 Quick Test**: See [docs/QUICKSTART.md](docs/QUICKSTART.md) for ready-to-use scripts to start a cluster and run workflows via gRPC or HTTP.

```bash
# Simple workflow example
cargo run --example typed_workflow_example

# Run tests
cargo test

# Two-node cluster test with workflow execution
./scripts/test_two_node_cluster.sh

# Run ping_pong workflow on a running cluster
./scripts/run_ping_pong.sh 127.0.0.1:7001          # gRPC
./scripts/run_ping_pong_http.sh 127.0.0.1:7001    # HTTP
```

---

## Technical Details

### Performance
- **Command Processing**: 30-171µs (microseconds)
- **Event-Driven**: Zero polling overhead
- **Owner/Wait Pattern**: 50-75% reduction in Raft proposals
- **Multi-Cluster**: 10x reduction in checkpoint replication (50 nodes → 5-node clusters)

### Requirements
- **Rust**: 1.70 or later
- **Deterministic Execution**: Same input → same operation sequence on all nodes
- **Serializable State**: Types must implement `Serialize + Deserialize`
- **Long-Running Services**: Not suitable for FaaS/Lambda (workflows need continuous execution)

### Storage
- **RocksDB**: Persistent storage (enabled by default, `persistent-storage` feature)
- **Node Identity**: Persisted across restarts
- **Crash Recovery**: RocksDB WAL ensures durability

### Current Limitations
- Workflow functions must be registered identically on all nodes
- No built-in compensation/rollback (implement in workflow logic)

---

## Comparison with Other Systems

**See our detailed comparison**: [Raftoral vs. Temporal vs. DBOS](docs/COMPARISON.md)

**Quick Summary**:

| Feature | Raftoral | Temporal | Vercel Workflows |
|---------|----------|----------|------------------|
| **Infrastructure** | Embedded (your nodes) | Separate cluster | Serverless (Vercel) |
| **Type Safety** | Rust compile-time | TypeScript runtime | TypeScript |
| **Deployment** | Self-hosted | Self-hosted | Vercel only |
| **Scaling** | Multi-cluster Raft | Activity workers | Automatic (managed) |
| **State Storage** | Raft consensus | External DB | Managed (Vercel) |
| **Cost** | Compute only | Compute + DB + queues | Per-workflow pricing |

---

## File Organization

```
src/
├── raft/generic/
│   ├── node.rs              # RaftNode with raft-rs integration
│   ├── proposal_router.rs   # Command submission & leader routing
│   ├── transport.rs         # Protocol-agnostic transport
│   ├── cluster_router.rs    # Multi-cluster message routing
│   ├── server/
│   │   ├── in_process.rs    # In-memory testing
│   │   └── network.rs       # gRPC/HTTP network transport
│   └── rocksdb_storage.rs   # Persistent Raft log storage
├── management/
│   ├── runtime.rs           # Management cluster runtime
│   └── state_machine.rs     # Topology management logic
├── workflow/
│   ├── runtime.rs           # Workflow execution runtime
│   ├── state_machine.rs     # Workflow state management
│   ├── context.rs           # WorkflowContext & execution
│   ├── replicated_var.rs    # checkpoint! and checkpoint_compute!
│   └── registry.rs          # Type-safe workflow storage
├── grpc/
│   ├── server.rs            # gRPC server implementation
│   ├── client.rs            # gRPC client (MessageSender)
│   └── bootstrap.rs         # Node discovery
├── http/
│   ├── server.rs            # HTTP/REST server (alternative)
│   ├── client.rs            # HTTP client
│   └── bootstrap.rs         # HTTP-based discovery
├── full_node/
│   └── mod.rs               # Complete node stack (management + execution)
└── lib.rs                   # Public API exports

docs/
├── QUICKSTART.md                # Getting started guide (start here!)
├── SCALABILITY_ARCHITECTURE.md  # Multi-cluster details
├── COMPARISON.md                # Raftoral vs Temporal vs DBOS
└── V2_ARCHITECTURE.md           # Layer architecture reference
```

---

## Contributing

Contributions welcome! Areas of interest:
- Multi-node fault injection testing
- Performance benchmarking
- Advanced workflow patterns
- Documentation improvements
- WebAssembly support (experimental HTTP client)

---

## Author

**Ori Shalev** - [ori.shalev@gmail.com](mailto:ori.shalev@gmail.com)

---

## License

MIT License. See [LICENSE](LICENSE) for details.

---

## Acknowledgments

- Built on [raft-rs](https://github.com/tikv/raft-rs) for Raft consensus
- Inspired by [Temporal](https://temporal.io/) and [Vercel Workflows](https://useworkflow.dev/)
- Uses [Tokio](https://tokio.rs/) for async runtime
- Storage via [RocksDB](https://rocksdb.org/)
