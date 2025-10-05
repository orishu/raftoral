# Raftoral

A Rust library for building fault-tolerant, distributed workflows using the Raft consensus protocol.

## Overview

Raftoral provides a distributed workflow orchestration engine where workflows execute in parallel across all cluster nodes. Using Raft consensus for coordination, it enables seamless failover and recovery without centralized databases.

**Key Innovation**: All nodes (leader + followers) execute workflows simultaneously. Checkpoint queues handle late followers, allowing cluster progress even when some nodes lag behind. This respects Raft's consensus principles while maximizing distributed execution.

## Current Status

**Production-Ready Features:**
- ✅ Multi-node Raft cluster with full consensus
- ✅ Parallel workflow execution on all nodes
- ✅ Type-safe workflow registry with closure-based registration
- ✅ Replicated variables for checkpointing workflow state
- ✅ Late follower catch-up via checkpoint queues
- ✅ Raft snapshots for new node state recovery
- ✅ Leadership transition support
- ✅ Transport abstraction (InMemoryClusterTransport, GrpcClusterTransport)
- ✅ Universal workflow initiation (any node can start)
- ✅ Automatic leader discovery for node operations
- ✅ Graceful node join and leave operations
- ✅ Event-driven architecture (no polling)
- ✅ Comprehensive test coverage (22 library tests + 5 multi-node tests)
- ✅ Clean modular code organization

## Quick Start

### Multi-Node Cluster Setup

```rust
use raftoral::raft::generic::transport::{ClusterTransport, InMemoryClusterTransport};
use raftoral::workflow::{WorkflowCommandExecutor, WorkflowRuntime, WorkflowContext};

// 1. Create transport for 3-node cluster
let transport = InMemoryClusterTransport::<WorkflowCommandExecutor>::new(vec![1, 2, 3]);
transport.start().await?;

// 2. Create runtimes (executors and self-registration automatic)
let runtime1 = WorkflowRuntime::new(transport.create_cluster(1).await?);
let runtime2 = WorkflowRuntime::new(transport.create_cluster(2).await?);
let runtime3 = WorkflowRuntime::new(transport.create_cluster(3).await?);

// 3. Register workflow on all nodes
let workflow = |input: OrderInput, ctx: WorkflowContext| async move {
    let status = ctx.create_replicated_var("status", "processing").await?;
    // ... business logic ...
    Ok(OrderOutput { id: input.id })
};

runtime1.register_workflow_closure("process_order", 1, workflow.clone())?;
runtime2.register_workflow_closure("process_order", 1, workflow.clone())?;
runtime3.register_workflow_closure("process_order", 1, workflow)?;

// 4. Start from any node - Raft handles routing
let run = runtime1.start_workflow_typed("process_order", 1, input).await?;
let result = run.wait_for_completion().await?;
```

### Single-Node Setup (for testing)

```rust
use raftoral::{WorkflowRuntime, WorkflowContext};

let runtime = WorkflowRuntime::new_single_node(1).await?;
tokio::time::sleep(Duration::from_millis(500)).await; // Wait for leadership

runtime.register_workflow_closure("my_workflow", 1,
    |input: MyInput, ctx: WorkflowContext| async move {
        Ok(MyOutput { /* ... */ })
    }
)?;

let run = runtime.start_workflow_typed("my_workflow", 1, input).await?;
let result = run.wait_for_completion().await?;
```

## Key Features

### 1. Consensus-Driven Parallel Execution

All nodes execute workflows when they see `WorkflowStart` through Raft consensus:

- **Leader**: Executes and proposes checkpoints/completion via Raft
- **Followers**: Execute in parallel, consuming checkpoints from queue
- **Load Distribution**: Computation spread across entire cluster
- **Fault Tolerance**: Any node can complete if leader fails

### 2. Type-Safe Workflow Registration

```rust
runtime.register_workflow_closure(
    "fibonacci",
    1,
    |input: FibonacciInput, context: WorkflowContext| async move {
        let mut a = 0u64;
        let mut b = 1u64;
        for _ in 2..=input.n {
            let temp = a + b;
            a = b;
            b = temp;
        }
        Ok(FibonacciOutput { result: b })
    }
)?;
```

### 3. Replicated Variables with Automatic Checkpointing

```rust
// Create checkpointed variable
let counter = context.create_replicated_var("counter", 0).await?;

// Atomic updates
counter.update(|val| val + 1).await?;

// Computed values (for external API calls)
let api_result = context.create_replicated_var_with_computation(
    "api_result",
    || async { call_external_api().await }
).await?;
```

### 4. Late Follower Catch-Up (Checkpoint Queues)

**Problem**: Follower receives checkpoint before execution reaches that point → deadlock

**Solution**: Queue checkpoint values, pop on first access
- FIFO queue per (workflow_id, checkpoint_key)
- Deterministic execution ensures correctness
- Leader cleanup prevents self-consumption
- Cluster progresses even with slow followers

### 5. Raft Snapshots for State Recovery

**Problem**: New nodes need complete checkpoint history, but queues are consumed during execution

**Solution**: Dual storage - queues for execution, history for snapshots
- Checkpoint history tracks all updates with log indices
- Snapshots capture only active workflows
- Queue reconstruction from history on snapshot restore
- Enables new node catch-up without full log replay

### 6. Automatic Leader Discovery & Node Management

**No manual leader tracking required** - nodes automatically find the leader:

```rust
// Add node from ANY node in the cluster
cluster.add_node(4).await?;  // Automatically routes to leader

// Remove node gracefully
cluster.remove_node(4).await?;  // Works from any node
```

**Hybrid discovery strategy:**
1. Check cached leader ID (fast path)
2. Scan all nodes to find current leader
3. Subscribe to role changes for new leader
4. Exponential backoff retry

**Production node lifecycle:**
```rust
// main.rs automatically handles join and leave
if !args.bootstrap {
    cluster.add_node(node_id).await?;  // Join on startup
}

signal::ctrl_c().await?;
cluster.remove_node(node_id).await?;   // Clean shutdown
```

## Architecture

### Execution Flow

1. Any node proposes `WorkflowStart(type, version, input)` via Raft
2. Consensus applies WorkflowStart on ALL nodes → all spawn execution
3. Leader's execution completes → proposes `WorkflowEnd(result)`
4. Consensus applies WorkflowEnd → followers exit gracefully

### Transport Abstraction

```rust
pub trait ClusterTransport<E: CommandExecutor> {
    fn create_cluster(&self, node_id: u64)
        -> Future<Output = Arc<RaftCluster<E>>>;
    fn node_ids(&self) -> Vec<u64>;
    fn start(&self) -> Future<Output = Result<()>>;
    fn shutdown(&self) -> Future<Output = Result<()>>;
}
```

- `InMemoryClusterTransport` - Local testing via tokio channels
- `GrpcClusterTransport` - Network deployment via generic gRPC (fully decoupled from workflow system)

### Event-Driven Coordination

No polling - all synchronization via Tokio broadcast channels:

```rust
pub enum WorkflowEvent {
    Started { workflow_id: String },
    Completed { workflow_id: String },
    Failed { workflow_id: String },
    CheckpointSet { workflow_id: String, key: String },
}
```

## Running Examples

```bash
# Multi-node fibonacci example
cargo test test_three_node_cluster_workflow_execution -- --nocapture

# Typed workflow with checkpoints
cargo run --example typed_workflow_example

# Run all tests
cargo test

# With logging
RUST_LOG=info cargo test
```

## Working Example

See `examples/typed_workflow_example.rs`:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ComputationInput {
    base_value: i32,
    multiplier: i32,
    iterations: u32,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = WorkflowRuntime::new_single_node(1).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    runtime.register_workflow_closure(
        "computation", 1,
        |input: ComputationInput, context: WorkflowContext| async move {
            let mut current_value = input.base_value;
            let mut results = Vec::new();

            for i in 0..input.iterations {
                current_value *= input.multiplier;
                results.push(current_value);

                // Checkpoint each step
                context.create_replicated_var(
                    &format!("step_{}", i),
                    current_value
                ).await?;
            }

            Ok(ComputationOutput {
                final_result: current_value,
                intermediate_values: results,
            })
        }
    )?;

    let input = ComputationInput {
        base_value: 2,
        multiplier: 3,
        iterations: 4
    };

    let run = runtime.start_workflow_typed("computation", 1, input).await?;
    let result = run.wait_for_completion().await?;

    println!("Result: {}", result.final_result); // 162
    Ok(())
}
```

## Development Milestones

### Completed ✅

**Milestone 8: Checkpoint Queues for Late Followers**
- Queue-based catch-up mechanism
- Leader cleanup to prevent self-consumption
- Deterministic FIFO ordering

**Milestone 9: Multi-Node Cluster Transport**
- ClusterTransport trait abstraction
- InMemoryClusterTransport implementation
- Universal workflow initiation
- Proper leader election across nodes

**Milestone 10: API Simplification**
- Automatic executor creation via Default trait
- WorkflowRuntime::new() returns Arc<Self> and auto-registers
- Clean 3-line setup (down from ~15 lines)

**Milestone 11: Raft Snapshots** ✅
- Dual storage: checkpoint queues + checkpoint history
- CheckpointHistory tracks complete log with indices
- Snapshot creation and application methods
- On-demand snapshots for new node catch-up

**Milestone 12: Code Organization Refactoring** ✅
- Modular architecture (split 1622-line file into 5 focused modules)
- Clear separation: commands, errors, executor, runtime, context
- All tests passing after refactoring
- Improved maintainability and readability

**Milestone 13: Generic gRPC Transport** ✅
- Completely decoupled gRPC layer from workflow system
- Proto file reduced to 41 lines of pure infrastructure (from 82 lines)
- SerializableMessage<C> architecture for network serialization
- Single generic send_message() for all message types
- Works with ANY command type implementing standard Rust traits
- Custom ChannelBuilder support for TLS/authentication
- All 23 library tests + examples passing

### Next Steps 🚀

**Enhanced Snapshot Testing**
- Multi-node snapshot integration tests
- New node joining via snapshot restoration
- Snapshot during active workflow execution

**Future Enhancements**
- Multi-node gRPC cluster testing (infrastructure is ready)
- Advanced workflow patterns (child workflows, compensation)
- Observability and metrics
- Performance benchmarking

## Technical Details

### Performance
- **Command Processing**: 30-171µs (microseconds)
- **Event-Driven**: Zero polling overhead
- **Optimized For**: Orchestration-heavy workflows

### Requirements
- **Rust**: 1.70 or later
- **Deterministic Execution**: Same input → same operation sequence
- **Serializable State**: Types must implement `Serialize + Deserialize`
- **Type Safety**: Full compile-time checking

### Current Limitations
- Workflow functions must be registered identically on all nodes
- No built-in compensation/rollback (implement in workflow logic)
- In-memory storage only (persistent storage coming later)

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
raftoral = { path = "path/to/raftoral" }  # or git/version when published
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
```

## File Organization

```
src/
├── raft/generic/
│   ├── cluster.rs      # RaftCluster coordination
│   ├── node.rs         # RaftNode raft-rs integration
│   ├── transport.rs    # ClusterTransport abstraction
│   └── message.rs      # Message types
├── workflow/
│   ├── commands.rs     # Command definitions
│   ├── error.rs        # Error types and status
│   ├── executor.rs     # CommandExecutor implementation
│   ├── runtime.rs      # WorkflowRuntime and subscriptions
│   ├── context.rs      # WorkflowContext and WorkflowRun
│   ├── registry.rs     # Type-safe workflow storage
│   ├── replicated_var.rs # Checkpointed variables
│   ├── snapshot.rs     # Snapshot data structures
│   └── ownership.rs    # Workflow ownership tracking
└── lib.rs              # Public API

tests/
├── multi_node_test.rs       # 3-node integration tests
└── node_failure_test.rs     # Failover and reassignment tests

examples/
├── typed_workflow_example.rs
├── simple_workflow.rs
└── scoped_workflow.rs
```

## Contributing

Contributions welcome! Areas of interest:
- Multi-node fault injection testing
- Multi-node gRPC cluster testing
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
