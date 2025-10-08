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

### Bootstrap Node (gRPC Production Setup)

```rust
use raftoral::runtime::{RaftoralConfig, RaftoralGrpcRuntime};
use raftoral::workflow::WorkflowContext;
use tokio::signal;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // 1. Configure and start the runtime (bootstrap mode)
    let config = RaftoralConfig::bootstrap("127.0.0.1:7001".to_string(), Some(1));
    let runtime = RaftoralGrpcRuntime::start(config).await?;

    // 2. Register workflow (using checkpoint! macro for clean syntax)
    runtime.workflow_runtime().register_workflow_closure(
        "process_order", 1,
        |input: OrderInput, ctx: WorkflowContext| async move {
            let status = checkpoint!(ctx, "status", "processing");
            // ... business logic ...
            Ok(OrderOutput { id: input.id })
        }
    )?;

    // 3. Wait for shutdown
    signal::ctrl_c().await?;
    runtime.shutdown().await?;
    Ok(())
}
```

### Join Existing Cluster

```rust
// Node joining an existing cluster
let config = RaftoralConfig::join(
    "127.0.0.1:7002".to_string(),
    vec!["127.0.0.1:7001".to_string()]
);

let runtime = RaftoralGrpcRuntime::start(config).await?;

// Register same workflows as other nodes
runtime.workflow_runtime().register_workflow_closure(
    "process_order", 1,
    |input: OrderInput, ctx: WorkflowContext| async move {
        // Same workflow implementation
        Ok(OrderOutput { id: input.id })
    }
)?;

signal::ctrl_c().await?;
runtime.shutdown().await?;
```

### Advanced Configuration (TLS, Timeouts, etc.)

```rust
use raftoral::grpc::client::ChannelBuilder;
use tonic::transport::Channel;

// Custom channel builder for TLS/authentication
let channel_builder = Arc::new(|address: String| {
    Box::pin(async move {
        Channel::from_shared(format!("https://{}", address))?
            .tls_config(tls_config)?
            .connect_timeout(Duration::from_secs(10))
            .connect()
            .await
    })
}) as ChannelBuilder;

let config = RaftoralConfig::bootstrap("127.0.0.1:7001".to_string(), Some(1))
    .with_channel_builder(channel_builder)
    .with_advertise_address("192.168.1.10:7001".to_string());

let runtime = RaftoralGrpcRuntime::start(config).await?;
```

### In-Memory Transport (for testing)

```rust
use raftoral::raft::generic::transport::{ClusterTransport, InMemoryClusterTransport};
use raftoral::workflow::{WorkflowCommandExecutor, WorkflowRuntime};

// Create transport for 3-node cluster
let transport = InMemoryClusterTransport::<WorkflowCommandExecutor>::new(vec![1, 2, 3]);
transport.start().await?;

// Create runtimes (executors and self-registration automatic)
let runtime1 = WorkflowRuntime::new(transport.create_cluster(1).await?);
let runtime2 = WorkflowRuntime::new(transport.create_cluster(2).await?);
let runtime3 = WorkflowRuntime::new(transport.create_cluster(3).await?);

// Register and execute workflows
let run = runtime1.start_workflow_typed("my_workflow", 1, input).await?;
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

### 3. Replicated Variables with Clean Macro Syntax

```rust
use raftoral::{checkpoint, checkpoint_compute};

// Create checkpointed variables with explicit keys
let mut counter = checkpoint!(ctx, "counter", 0);
let mut history = checkpoint!(ctx, "history", Vec::<i32>::new());

// Read with deref
let value = *counter;

// Direct updates
counter.set(value + 1).await?;

// Functional updates
history.update(|mut h| { h.push(*counter); h }).await?;

// Computed values (for side effects like API calls)
let api_result = checkpoint_compute!(ctx, "api_result", || async {
    call_external_api().await
});
```

**Key benefits of explicit keys:**
- ✅ Stable across code changes (adding/removing lines)
- ✅ Version-safe (same keys work in v1 and v2)
- ✅ Self-documenting in logs
- ✅ Editor type inference (`counter` is `ReplicatedVar<i32>`)

### 4. Checkpoint Macro Demo

See `examples/checkpoint_macro_demo.rs` for a complete demonstration of both macros.

### 5. Late Follower Catch-Up (Checkpoint Queues)

**Problem**: Follower receives checkpoint before execution reaches that point → deadlock

**Solution**: Queue checkpoint values, pop on first access
- FIFO queue per (workflow_id, checkpoint_key)
- Deterministic execution ensures correctness
- Leader cleanup prevents self-consumption
- Cluster progresses even with slow followers

### 6. Raft Snapshots for State Recovery

**Problem**: New nodes need complete checkpoint history, but queues are consumed during execution

**Solution**: Dual storage - queues for execution, history for snapshots
- Checkpoint history tracks all updates with log indices
- Snapshots capture only active workflows
- Queue reconstruction from history on snapshot restore
- Enables new node catch-up without full log replay

### 7. Automatic Leader Discovery & Node Management

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
# Simple runtime example (production-style gRPC usage)
cargo run --example simple_runtime

# Checkpoint macros demonstration
cargo run --example checkpoint_macro_demo

# Typed workflow with checkpoints
cargo run --example typed_workflow_example

# Run the main binary (bootstrap node)
RUST_LOG=info cargo run -- --listen 127.0.0.1:7001 --bootstrap

# Run second node joining cluster
RUST_LOG=info cargo run -- --listen 127.0.0.1:7002 --peers 127.0.0.1:7001

# Run all tests
cargo test

# Multi-node integration tests with logging
RUST_LOG=info cargo test test_three_node_cluster_workflow_execution -- --nocapture
```

## Working Example

See `examples/simple_runtime.rs` for production-style gRPC usage:

```rust
use raftoral::runtime::{RaftoralConfig, RaftoralGrpcRuntime};
use raftoral::workflow::WorkflowContext;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ComputeInput { value: i32 }

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ComputeOutput { result: i32 }

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // Create configuration for a new cluster
    let config = RaftoralConfig::bootstrap("127.0.0.1:7001".to_string(), Some(1));

    // Start the runtime - handles all initialization
    let runtime = RaftoralGrpcRuntime::start(config).await?;

    // Register a simple computation workflow
    let compute_fn = |input: ComputeInput, _ctx: WorkflowContext| async move {
        // Simple computation: multiply by 2
        let result = input.value * 2;
        Ok::<ComputeOutput, WorkflowError>(ComputeOutput { result })
    };

    runtime.workflow_runtime()
        .register_workflow_closure("compute", 1, compute_fn)?;

    println!("✓ Registered 'compute' workflow");

    // Execute the workflow
    let input = ComputeInput { value: 21 };
    let workflow_run = runtime.workflow_runtime()
        .start_workflow::<ComputeInput, ComputeOutput>("compute", 1, input)
        .await?;

    let output = workflow_run.wait_for_completion().await?;
    println!("✓ Workflow completed: {} * 2 = {}", 21, output.result);

    // Wait for shutdown signal
    signal::ctrl_c().await?;

    // Gracefully shutdown
    runtime.shutdown().await?;
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

**Milestone 14: Production-Ready Runtime API** ✅
- RaftoralGrpcRuntime high-level API for easy cluster setup
- RaftoralConfig builder pattern (bootstrap, join, with_channel_builder, etc.)
- Automatic peer discovery and node ID assignment
- Graceful shutdown with cluster membership updates
- Standard logging via log crate facade
- Production configurability (TLS, timeouts, compression, server limits)
- Clean 3-step pattern: start → register → shutdown

### Next Steps 🚀

**Production gRPC Cluster Testing**
- Multi-node gRPC cluster integration tests
- TLS/authentication validation
- Network partition and recovery testing
- Snapshot-based new node joining

**Future Enhancements**
- Advanced workflow patterns (child workflows, parallel execution, compensation)
- Persistent storage backend integration
- Observability and metrics (Prometheus, OpenTelemetry)
- Performance benchmarking and optimization

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
├── checkpoint_macro_demo.rs    # Demonstrates checkpoint! and checkpoint_compute!
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
