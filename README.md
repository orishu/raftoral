# Raftoral

A Rust library for building fault-tolerant, distributed workflows using the Raft consensus protocol.

## Overview

Raftoral provides a simplified approach to distributed workflow orchestration, focusing on long-running programs that require reliability across node failures. Instead of relying on centralized databases for event sourcing, Raftoral replicates workflow state via Raft consensus, enabling seamless failover and recovery.

**Key Innovation**: Workflows execute on all nodes (leader and followers), with a queue-based catch-up mechanism allowing late followers to synchronize without blocking cluster progress. This enables true distributed execution that respects Raft's consensus principles.

## Current Status

**Production-Ready Features:**
- ✅ Single-node Raft cluster with full consensus
- ✅ Type-safe workflow registry with closure-based registration
- ✅ Automatic workflow execution on all nodes
- ✅ Replicated variables for checkpointing workflow state
- ✅ Late follower catch-up via checkpoint queues
- ✅ Leadership transition support
- ✅ Event-driven architecture (no polling)
- ✅ Comprehensive test coverage (11 unit tests)

**Architecture Ready for Multi-Node:**
- Queue-based checkpoint synchronization
- Leader/follower execution coordination
- Dynamic leadership transition handling
- Full Raft consensus integration

## Key Features

### Type-Safe Workflow Registration

Register workflows using simple closures with full type safety:

```rust
use raftoral::{WorkflowRuntime, WorkflowContext};

let workflow_runtime = WorkflowRuntime::new_single_node(1).await?;

// Register a typed workflow
workflow_runtime.register_workflow_closure(
    "process_order",
    1,
    |input: OrderInput, context: WorkflowContext| async move {
        // Workflow logic with automatic checkpointing
        let status = context.create_replicated_var("status", "pending").await?;

        // ... business logic ...

        Ok(OrderResult { order_id: input.id })
    }
)?;
```

### Replicated Variables

Checkpoint workflow state with automatic serialization and replication:

```rust
// Direct value initialization
let counter = ReplicatedVar::with_value("counter", &workflow_run, 42).await?;

// Computed values (for side effects)
let api_result = ReplicatedVar::with_computation(
    "api_result",
    &workflow_run,
    || async {
        // Side effect - only executed once, result replicated
        call_external_api().await
    }
).await?;

// Updates
counter.update(|val| val + 1).await?;
```

### Automatic Workflow Execution

Workflows execute automatically in the background on all nodes:

```rust
// Start a typed workflow
let workflow_run = workflow_runtime
    .start_workflow::<Input, Output>("process_order", 1, input)
    .await?;

// Wait for natural completion (no manual coordination needed)
let result = workflow_run.wait_for_completion().await?;
```

### Late Follower Catch-Up

**The Innovation**: Followers can lag behind the leader without blocking cluster progress.

When a follower's Raft log receives checkpoint commands before execution reaches those points, values are queued. When execution catches up, it consumes from the queue. Simple FIFO semantics with deterministic execution guarantee correctness.

**Leader queue cleanup**: After proposing a checkpoint, the leader pops its own value from the queue, preventing self-consumption while allowing followers to catch up independently.

## Architecture

### Execution Model

**All nodes execute workflows**, but with different timing:
- **Leader**: Executes in real-time, proposing checkpoints via consensus
- **Followers**: May lag behind, consuming checkpoints from queue as they catch up
- **New Leaders**: Finish consuming queued checkpoints before proposing new ones

### Consensus-Driven Workflow Execution

1. **Workflow Registration**: All nodes register the same workflow functions
2. **Initiation**: Leader receives start request, proposes `WorkflowStart` event
3. **Execution**: All nodes spawn workflow execution upon seeing `WorkflowStart`
4. **Checkpointing**:
   - Leader proposes `SetCheckpoint` commands
   - All nodes enqueue checkpoints in `apply()`
   - Leader cleans up its own values
   - Followers consume from queue
5. **Completion**: Leader's execution calls `finish_with()`, proposing `WorkflowEnd`

### Event-Driven Coordination

No polling or heuristic delays - all synchronization via Tokio broadcast channels:

```rust
pub enum WorkflowEvent {
    Started { workflow_id: String },
    Completed { workflow_id: String, result: Vec<u8> },
    Failed { workflow_id: String, error: String },
    CheckpointSet { workflow_id: String, key: String, value: Vec<u8> },
}
```

## Working Example

See `examples/typed_workflow_example.rs` for a complete working example:

```rust
use raftoral::{WorkflowContext, WorkflowRuntime};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ComputationInput {
    base_value: i32,
    multiplier: i32,
    iterations: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ComputationOutput {
    final_result: i32,
    intermediate_values: Vec<i32>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workflow_runtime = WorkflowRuntime::new_single_node(1).await?;

    // Wait for leadership
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Register workflow
    workflow_runtime.register_workflow_closure(
        "computation",
        1,
        |input: ComputationInput, context: WorkflowContext| async move {
            let mut progress = context.create_replicated_var("progress", 0u32).await?;
            let mut current_value = input.base_value;
            let mut results = Vec::new();

            for i in 0..input.iterations {
                progress.update(|p| p + 1).await?;
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

    // Execute workflow
    let input = ComputationInput { base_value: 2, multiplier: 3, iterations: 4 };
    let workflow_run = workflow_runtime
        .start_workflow("computation", 1, input)
        .await?;

    let result = workflow_run.wait_for_completion().await?;
    println!("Result: {}", result.final_result); // 162

    Ok(())
}
```

## Getting Started

### Prerequisites

- Rust 1.70 or later
- Tokio async runtime

### Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
raftoral = "0.1.0"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
```

### Basic Usage

```rust
use raftoral::{WorkflowRuntime, WorkflowContext};

// 1. Create a workflow runtime
let runtime = WorkflowRuntime::new_single_node(1).await?;
tokio::time::sleep(Duration::from_millis(500)).await; // Wait for leadership

// 2. Register your workflow
runtime.register_workflow_closure(
    "my_workflow",
    1,
    |input: MyInput, context: WorkflowContext| async move {
        // Your workflow logic here
        Ok(MyOutput { /* ... */ })
    }
)?;

// 3. Start execution
let workflow_run = runtime
    .start_workflow("my_workflow", 1, input)
    .await?;

// 4. Wait for completion
let result = workflow_run.wait_for_completion().await?;
```

### Running Examples

```bash
# Build and run the typed workflow example
cargo run --example typed_workflow_example

# Run tests
cargo test

# Run with logging
RUST_LOG=info cargo run --example typed_workflow_example
```

## Development Status

### Completed Milestones

**✅ Milestone 1**: Single-node Raft with command proposal and application
- Proper raft-rs integration
- Command serialization and consensus
- Event-driven state machine

**✅ Milestone 2**: Event-driven workflow execution
- WorkflowRuntime singleton pattern
- Race condition-free architecture
- Microsecond-level performance (30-171µs command completion)

**✅ Milestone 3**: Workflow registry with type-safe execution
- Closure-based workflow registration
- Generic input/output types with serialization
- Automatic background execution

**✅ Milestone 4-7**: Architecture refinements
- Clean ownership hierarchies
- Simplified APIs
- Code quality improvements
- Removed deprecated methods

**✅ Milestone 8**: Late follower catch-up with checkpoint queues
- Queue-based synchronization
- Leader queue cleanup
- Leadership transition support
- Simple FIFO deterministic execution

### Upcoming Work

**Next: Multi-Node Testing**
- 3+ node cluster tests
- Actual leader election during workflows
- Network partition simulation
- Failover and recovery validation

**Future Enhancements**
- Child workflows
- Parallel execution branches
- Compensation/saga patterns
- Observability and metrics
- Production hardening

## Technical Details

### Performance

- **Command Processing**: 30-171µs (microseconds)
- **Event-Driven**: Zero polling overhead
- **Optimized for**: Orchestration-heavy workflows, not compute-intensive tasks

### Requirements

- **Deterministic Execution**: Same input produces same sequence of operations
- **Serializable State**: All checkpointed types must implement `Serialize + Deserialize`
- **Type Safety**: Full compile-time type checking for workflow inputs/outputs

### Limitations

- Currently single-node testing only (multi-node architecture ready)
- Workflow functions must be registered on all nodes identically
- No built-in compensation/rollback (can be implemented in workflow logic)

## Author

**Ori Shalev** - [ori.shalev@gmail.com](mailto:ori.shalev@gmail.com)

## Contributing

Contributions welcome! Areas of interest:
- Multi-node cluster testing
- Advanced workflow patterns
- Performance benchmarking
- Documentation improvements

## License

MIT License. See [LICENSE](LICENSE) for details.

## Acknowledgments

- Built on [raft-rs](https://github.com/tikv/raft-rs) for Raft consensus
- Inspired by [Temporal](https://temporal.io/) workflow orchestration concepts
- Uses [Tokio](https://tokio.rs/) for async runtime
