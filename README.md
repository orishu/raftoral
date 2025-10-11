# Raftoral

A Rust library for building fault-tolerant, distributed workflows using the Raft consensus protocol.

## The Problem: Serverless Without Servers

Modern serverless platforms (AWS Lambda, Google Cloud Functions, etc.) are great for stateless execution, but orchestrating complex, long-running workflows is challenging:

### Traditional Serverless Limitations

**External Orchestrators (Temporal, AWS Step Functions, etc.):**
- ❌ **Infrastructure Overhead**: Requires running dedicated servers/databases for orchestration
- ❌ **Vendor Lock-in**: Cloud-specific or hosted service dependencies
- ❌ **Network Latency**: Every workflow step requires network round-trips to orchestrator
- ❌ **Operational Complexity**: Another service to monitor, scale, and maintain

**Why This Is a Problem:**
The promise of serverless is "no servers to manage," but workflow orchestration brings them back. You end up running Temporal servers, managing databases, and dealing with all the operational overhead you wanted to avoid.

### The Raftoral Solution: Truly Serverless Workflows

Raftoral eliminates the need for external orchestration infrastructure by embedding the orchestrator directly into your application using Raft consensus:

- ✅ **No External Services**: No separate orchestrator servers or databases to run
- ✅ **Pure Rust Library**: Just add it to your dependencies and run
- ✅ **Self-Coordinating**: Cluster nodes coordinate via Raft protocol
- ✅ **Dynamic Scaling**: Start with any number of nodes, add/remove as needed
- ✅ **Fault Tolerant**: Automatic failover when nodes fail
- ✅ **Cloud Agnostic**: Works anywhere Rust runs

**The Architecture Difference:**
- **Traditional**: Your Code → Network → Orchestrator Servers → Database → Network → Your Code
- **Raftoral**: Your Code (with embedded orchestration) ↔ Other Nodes (peer-to-peer)

## Architecture Overview

### Consensus-Driven Execution

Raftoral uses Raft consensus to coordinate workflow execution across a cluster of nodes without requiring external infrastructure:

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│   Node 1    │────▶│   Node 2    │◀────│   Node 3    │
│  (Leader)   │     │ (Follower)  │     │ (Follower)  │
└─────────────┘     └─────────────┘     └─────────────┘
      ▲                   ▲                   ▲
      │                   │                   │
      └───────────────────┴───────────────────┘
           Raft Consensus Protocol
        (No external database needed)
```

**All nodes execute workflows in parallel:**

1. **Workflow Start**: Any node can initiate a workflow by proposing a `WorkflowStart` command through Raft
2. **Parallel Execution**: Once committed via consensus, ALL nodes execute the workflow function
3. **Checkpoint Synchronization**: The leader's execution creates checkpoints that followers consume from queues
4. **Natural Completion**: Leader finishes first and proposes `WorkflowEnd`, followers exit gracefully

**Key Benefits:**
- **Load Distribution**: Computation happens on all nodes, not just the leader
- **Fault Tolerance**: Any node can complete a workflow if the leader fails
- **No External Dependencies**: Everything runs in your application process

### Checkpoints & Replicated Variables vs. Temporal "Activities"

If you're familiar with Temporal, Raftoral's **checkpoints** serve a similar purpose to **Activities**, but with a different philosophy:

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
let amount = checkpoint!(ctx, "charge_amount", 100);
let result = checkpoint_compute!(ctx, "payment_result", || async {
    charge_card(*amount).await  // External call executed once
});
```

**Key Differences:**

| Aspect | Temporal Activities | Raftoral Checkpoints |
|--------|---------------------|---------------------|
| **Execution Model** | Separate worker pools | Same process, all nodes execute |
| **State Storage** | External database | Raft consensus (in-memory + snapshots) |
| **Side Effects** | Activity-specific retry logic | `checkpoint_compute!` for one-time execution |
| **Network Overhead** | Every activity call | Only during checkpoint creation |
| **Determinism** | Activities can be non-deterministic | Workflow code must be deterministic |

**When to use `checkpoint!` vs `checkpoint_compute!`:**
- **`checkpoint!(ctx, "key", value)`**: For deterministic state (counters, status, computed values)
- **`checkpoint_compute!(ctx, "key", || async { ... })`**: For side effects (API calls, external services)
  - Executes the computation **once** (on the leader)
  - Result is replicated to all nodes
  - Subsequent node executions use the cached result from checkpoint queue

**Example - Payment Processing:**
```rust
runtime.register_workflow_closure("process_payment", 1,
    |input: PaymentInput, ctx: WorkflowContext| async move {
        // Deterministic state
        let order_id = checkpoint!(ctx, "order_id", input.order_id);
        let amount = checkpoint!(ctx, "amount", input.amount);

        // Side effect: charge card once
        let charge_result = checkpoint_compute!(ctx, "charge", || async {
            stripe::charge_card(*order_id, *amount).await
        });

        // Update based on result
        let status = checkpoint!(ctx, "status",
            if charge_result.is_ok() { "completed" } else { "failed" }
        );

        Ok(PaymentOutput { status: status.clone() })
    }
)?;
```

**Why This Matters:**
- **No Activity Workers**: No separate processes to manage
- **No Task Queues**: No polling infrastructure needed
- **All-in-One**: Orchestration and execution in the same binary
- **Type Safety**: Rust's type system ensures correctness at compile time

## Quick Start

### Bootstrap a Cluster

```rust
use raftoral::runtime::{RaftoralConfig, RaftoralGrpcRuntime};
use raftoral::workflow::WorkflowContext;
use raftoral::{checkpoint, checkpoint_compute};
use tokio::signal;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // 1. Bootstrap the first node
    let config = RaftoralConfig::bootstrap("127.0.0.1:7001".to_string(), Some(1));
    let runtime = RaftoralGrpcRuntime::start(config).await?;

    // 2. Register workflow with checkpoints
    runtime.workflow_runtime().register_workflow_closure(
        "process_order", 1,
        |input: OrderInput, ctx: WorkflowContext| async move {
            // Regular checkpoint for deterministic state
            let mut status = checkpoint!(ctx, "status", "processing");

            // Computed checkpoint for side effects (API calls)
            let inventory_check = checkpoint_compute!(ctx, "inventory", || async {
                check_inventory_service(input.item_id).await
            });

            if *inventory_check {
                status.set("confirmed").await?;
            } else {
                status.set("out_of_stock").await?;
            }

            Ok(OrderOutput { status: status.clone() })
        }
    )?;

    // 3. Wait for shutdown
    signal::ctrl_c().await?;
    runtime.shutdown().await?;
    Ok(())
}
```

### Join an Existing Cluster

```rust
// Node joining an existing cluster
let config = RaftoralConfig::join(
    "127.0.0.1:7002".to_string(),
    vec!["127.0.0.1:7001".to_string()]  // Seed nodes for discovery
);

let runtime = RaftoralGrpcRuntime::start(config).await?;

// Register same workflows as other nodes
runtime.workflow_runtime().register_workflow_closure(
    "process_order", 1,
    |input: OrderInput, ctx: WorkflowContext| async move {
        // Same implementation as bootstrap node
        Ok(OrderOutput { /* ... */ })
    }
)?;

signal::ctrl_c().await?;
runtime.shutdown().await?;
```

### Execute a Workflow

```rust
// From ANY node in the cluster
let input = OrderInput {
    order_id: "ORD-123".to_string(),
    item_id: "ITEM-456".to_string(),
};

let workflow_run = runtime.workflow_runtime()
    .start_workflow::<OrderInput, OrderOutput>("process_order", 1, input)
    .await?;

let output = workflow_run.wait_for_completion().await?;
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
let config = RaftoralConfig::join(
    "127.0.0.1:7004".to_string(),
    vec!["127.0.0.1:7001".to_string(), "127.0.0.1:7002".to_string()]
);

let runtime = RaftoralGrpcRuntime::start(config).await?;
// Node discovers cluster configuration, gets assigned node ID,
// and receives Raft snapshot to catch up on running workflows
```

**What Happens During Join:**
1. **Discovery**: New node contacts seed nodes to discover cluster
2. **Node ID Assignment**: Receives unique ID (highest known + 1)
3. **Configuration Update**: Leader proposes ConfChange to add node as voter
4. **Snapshot Transfer**: Leader sends Raft snapshot containing:
   - Active workflow states
   - Checkpoint queues for in-flight workflows
   - Cluster configuration
5. **Sync**: New node applies snapshot and starts executing workflows

**Raft's Native Snapshot Mechanism:**
- No custom state transfer protocol needed
- Works for **any** workflow state, regardless of size
- Handles network failures with automatic retries
- Consistent snapshots (point-in-time cluster state)

### Remove Nodes Gracefully

```rust
// On shutdown, node removes itself from cluster
signal::ctrl_c().await?;
runtime.shutdown().await?;  // Automatically proposes ConfChange to remove node
```

**Why This Matters:**
- **Zero Downtime Scaling**: Add nodes without restarting the cluster
- **Gradual Rollouts**: Add one node at a time to test changes
- **Cost Optimization**: Scale down during low traffic periods
- **Maintenance**: Remove nodes for updates, bring them back when ready

### The Catch-Up Problem (Solved)

**Challenge**: What if a node joins while workflows are running with lots of checkpoints?

**Solution: Checkpoint Queues + Raft Snapshots**

```rust
// Workflow running on nodes 1, 2, 3:
for i in 0..1000 {
    counter.set(i).await?;  // Creates 1000 checkpoints
}

// Node 4 joins after 500 iterations:
// - Receives snapshot with checkpoint queues containing values 0-500
// - Starts executing at iteration 0
// - Pops from queue instead of computing: instant catch-up!
// - Joins live execution at iteration 500+
```

**Technical Details:**
- **Checkpoint History**: Leader tracks all checkpoints with log indices
- **Queue Reconstruction**: Snapshot includes queues for active workflows
- **FIFO Ordering**: Deterministic execution ensures queue order matches execution order
- **Lazy Consumption**: Values only popped when workflow execution reaches that point

**Result**: New nodes can join a cluster with running workflows and seamlessly catch up without blocking the cluster or missing state.

## Workflow Versioning

Workflows evolve over time - you add features, fix bugs, change behavior. Raftoral handles this through **explicit versioning** with a migration path for long-running workflows.

### The Problem

```rust
// Version 1 (deployed in production with running workflows)
runtime.register_workflow_closure("process_order", 1, |input, ctx| async {
    let status = checkpoint!(ctx, "status", "processing");
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
    let status = checkpoint!(ctx, "status", "processing");
    // ...original logic...
    Ok(OrderOutput { status: status.clone() })
})?;

// Version 2 - New workflows use this
runtime.register_workflow_closure("process_order", 2, |input, ctx| async {
    let status = checkpoint!(ctx, "status", "processing");

    // NEW: Fraud detection
    let fraud_check = checkpoint_compute!(ctx, "fraud_check", || async {
        fraud_service::check(input.order_id).await
    });

    if !*fraud_check {
        status.set("fraud_detected").await?;
        return Ok(OrderOutput { status: status.clone() });
    }

    // ...rest of logic...
    Ok(OrderOutput { status: status.clone() })
})?;
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
   runtime.register_workflow_closure("process_order", 2, /* ... */)?;
   ```

### Migration Paths

**Option A: Natural Completion** (Recommended)
- Keep old version registered
- Wait for workflows to finish
- Remove old version in next deployment

**Option B: Forced Migration** (Advanced)
- Implement state migration in v2
- Check checkpoint keys to detect v1 vs v2
- Transform v1 state to v2 format

```rust
runtime.register_workflow_closure("process_order", 2, |input, ctx| async {
    // Detect v1 workflow by checking for old checkpoint keys
    let is_v1_migration = ctx.runtime.checkpoint_exists(&ctx.workflow_id, "old_key");

    if is_v1_migration {
        // Migrate v1 state to v2 format
        let old_status = checkpoint!(ctx, "old_key", "unknown");
        let new_status = checkpoint!(ctx, "status", migrate_status(*old_status));
        // Continue with v2 logic
    } else {
        // Fresh v2 workflow
        let status = checkpoint!(ctx, "status", "processing");
    }
    // ...
})?;
```

**Why Explicit Versioning:**
- ✅ **Safe Rollouts**: Old workflows unaffected by new code
- ✅ **Clear Intent**: Version numbers make upgrade paths obvious
- ✅ **Gradual Migration**: No "big bang" deployments required
- ✅ **Rollback Support**: Can revert to old version if issues arise

**Current Limitation:**
- Workflows must be registered identically on all nodes
- No automatic schema migration (implement in workflow logic)
- Version cleanup is manual (remove old versions after workflows complete)

## Running Examples

```bash
# Checkpoint macros demonstration
cargo run --example checkpoint_macro_demo

# Simple runtime (production-style gRPC)
cargo run --example simple_runtime

# gRPC client example
cargo run --example grpc_workflow_client

# Run main binary (bootstrap node)
RUST_LOG=info cargo run -- --listen 127.0.0.1:7001 --bootstrap

# Second node joining cluster
RUST_LOG=info cargo run -- --listen 127.0.0.1:7002 --peers 127.0.0.1:7001

# Run all tests
cargo test
```

## Advanced Configuration

### TLS and Authentication

```rust
use raftoral::grpc::client::ChannelBuilder;
use tonic::transport::{Channel, ClientTlsConfig};

let channel_builder = Arc::new(|address: String| {
    Box::pin(async move {
        let tls = ClientTlsConfig::new()
            .domain_name("example.com")
            .ca_certificate(Certificate::from_pem(ca_cert));

        Channel::from_shared(format!("https://{}", address))?
            .tls_config(tls)?
            .connect_timeout(Duration::from_secs(10))
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .connect()
            .await
    })
}) as ChannelBuilder;

let config = RaftoralConfig::bootstrap("127.0.0.1:7001".to_string(), Some(1))
    .with_channel_builder(channel_builder)
    .with_advertise_address("192.168.1.10:7001".to_string());

let runtime = RaftoralGrpcRuntime::start(config).await?;
```

### In-Memory Transport (Testing)

```rust
use raftoral::raft::generic::transport::{ClusterTransport, InMemoryClusterTransport};
use raftoral::workflow::{WorkflowCommandExecutor, WorkflowRuntime};

// Create transport for 3-node cluster
let transport = InMemoryClusterTransport::<WorkflowCommandExecutor>::new(vec![1, 2, 3]);
transport.start().await?;

// Create runtimes
let runtime1 = WorkflowRuntime::new(transport.create_cluster(1).await?);
let runtime2 = WorkflowRuntime::new(transport.create_cluster(2).await?);
let runtime3 = WorkflowRuntime::new(transport.create_cluster(3).await?);

// Execute workflows in-memory (no network)
let run = runtime1.start_workflow_typed("my_workflow", 1, input).await?;
let result = run.wait_for_completion().await?;
```

## Technical Details

### Performance
- **Command Processing**: 30-171µs (microseconds)
- **Event-Driven**: Zero polling overhead
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
├── raft/generic/
│   ├── cluster.rs        # RaftCluster coordination
│   ├── node.rs           # RaftNode raft-rs integration
│   ├── transport.rs      # Transport abstraction
│   ├── grpc_transport.rs # gRPC implementation
│   └── message.rs        # Message types
├── workflow/
│   ├── commands.rs       # WorkflowCommand definitions
│   ├── executor.rs       # Command application
│   ├── runtime.rs        # WorkflowRuntime API
│   ├── context.rs        # WorkflowContext helpers
│   ├── registry.rs       # Type-safe workflow storage
│   ├── replicated_var.rs # Checkpoint variables
│   └── snapshot.rs       # Snapshot structures
├── runtime/
│   └── grpc.rs           # RaftoralGrpcRuntime high-level API
├── grpc/
│   ├── server.rs         # gRPC service implementation
│   ├── client.rs         # gRPC client helpers
│   └── bootstrap.rs      # Peer discovery
└── lib.rs                # Public API exports

examples/
├── checkpoint_macro_demo.rs  # Demonstrates checkpoint! and checkpoint_compute!
├── simple_runtime.rs         # Production-style gRPC usage
└── grpc_workflow_client.rs   # External client example
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
