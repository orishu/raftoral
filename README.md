# Raftoral

## Overview

Raftoral is a Rust library for building fault-tolerant, distributed workflows using the Raft consensus protocol. It provides a simplified alternative to complex orchestration frameworks like Temporal, focusing on long-running programs that require reliability across node failures. Instead of relying on a centralized database for event sourcing and replay, Raftoral replicates key state checkpoints across a cluster, allowing seamless failover.

Workflows are executed actively on the Raft leader node, with followers running in a passive simulation mode to shadow progress. Side-effecting operations (e.g., external API calls) are performed only by the leader, while followers treat them as no-ops. Checkpoints are replicated via Raft, ensuring the cluster maintains consistent state.

This library is designed for microservice environments where nodes can form virtual Raft clusters via a client library. Workflows are written in Rust, with abstractions to hide distribution details, making the code resemble single-threaded logic.

## Key Features

- **Fault Tolerance:** Survives leader failures by promoting followers with up-to-date checkpointed state.
- **Deterministic Execution:** Enforces determinism by replicating side-effect results as checkpoints.
- **Minimal Overhead:** Suitable for orchestration-heavy workflows, not compute-intensive ones.
- **Rust Abstractions:** Uses traits and (eventually) procedural macros for clean workflow definitions.
- **Integration:** Builds on existing Raft implementations (e.g., `raft-rs`).

## Architecture

Raftoral leverages Raft for consensus among a cluster of nodes (which can be threads in tests or real microservice instances). A distributed workflow is replicated to all nodes upon initiation.

- **Active Execution Mode (Leader):** The Raft leader executes the workflow, performing side-effecting actions and replicating checkpoint assignments via Raft proposals.
- **Passive Simulation Mode (Followers):** Followers execute the same code but treat side-effecting actions as no-ops. They pause after each checkpoint replication and resume upon the next, maintaining simulated state.
- **Replicated Checkpoints:** Designated state variables (e.g., `ReplicatedVar<T>`) whose assignments are consensus-backed events. These serve as recovery points.
- **Side-Effecting Actions:** Operations with external effects, abstracted via a trait to handle mode-specific behavior (real execution vs. no-op).
- **Failover:** On leader failure, a new leader resumes from the last replicated checkpoint using in-memory state.

Workflows must be deterministic: avoid non-replicated randomness (e.g., `random()`). Side effects should be idempotent or compensatable where possible.

### Terminology

- **Distributed Workflow:** A long-running, fault-tolerant script or program orchestrated across the cluster.
- **Replicated Checkpoint:** A consensus-backed state variable (e.g., `ReplicatedVar<T>`) that persists assignments via Raft.
- **Checkpoint Commitment:** The act of assigning to a replicated checkpoint, triggering Raft replication.
- **Side-Effecting Action:** A non-idempotent operation (e.g., API call) executed only in active mode.
- **Active Execution Mode:** Leader's real execution, including side effects.
- **Passive Simulation Mode:** Follower's no-op shadowing, pausing at checkpoints.
- **Consensus-Driven Orchestration:** The use of Raft for coordinating workflow progress.

## How It Works

1. **Workflow Initiation:** Propose a "start" event with the workflow code or identifier to the Raft cluster.
2. **Execution:**
   - Leader runs the code, committing checkpoints for key states (e.g., side-effect results as `Result<T, E>`).
   - Followers simulate, updating local replicated checkpoints upon Raft commits.
3. **Side Effects:** Wrapped in abstractions; results are immediately checkpointed to ensure determinism.
4. **Error Handling:** Use replicated checkpoints for retry counts and error states (e.g., `ReplicatedVar<Result<T, E>>`). Retries persist across failovers.
5. **Completion:** Propose an "end" event upon workflow finish.
6. **Failover:** New leader resumes using replicated checkpoints; no full replay needed.

## Development Plan and Milestones

The library will be developed incrementally, starting with core Raft integration and building up to user-friendly abstractions. Use `raft-rs` as the base Raft implementation.

### Milestone 1: In-Memory Raft Client with Basic Events

- Set up a single-node, in-memory Raft setup (inspired by https://github.com/tikv/raft-rs/blob/master/examples/single_mem_node/main.rs).
- Implement a Raft client that can propose events with payloads:
  - Workflow start: Initiates a new workflow instance.
  - Workflow end: Signals completion.
  - Set replicated checkpoint: Proposes a key-value update for a checkpoint.
- Focus on event serialization and basic proposal/apply logic.
- Unit tests: Verify events are applied correctly in-memory.

### Milestone 2: Single-Node Workflow Execution

- Run a full workflow on a single-node (local) Raft "cluster".
- Implement active execution and passive simulation modes (though in single-node, modes are simulated).
- Checkpoint events for replicated variables.
- Handle side-effecting actions with mode-aware traits.
- Unit tests: Execute sample workflows, simulate "shadowing" (e.g., via threads), verify state consistency.

### Milestone 3: Multi-Node Consensus

- Extend to a 3-node cluster (simulated as local threads in unit tests).
- Test full consensus: Propose checkpoints, handle leader election/failover.
- Verify workflow resumption on new leader after simulated failure.
- Tests: Ensure passive simulation keeps followers in sync; validate determinism and error retries across nodes.

### Milestone 4: Syntactic Sugar with Macros

- Introduce procedural macros for minimal annotations:
  - `#[distributed_workflow]` for workflow functions.
  - `#[replicated]` for checkpoint variables.
  - `#[side_effect]` for functions needing mode-specific handling.
- Macros generate boilerplate for replication, mode checks, and pauses.
- Final tests: End-to-end workflows with clean syntax.

## Sample Code

Below is a conceptual example of a workflow before macros (Milestone 2/3 style). After Milestone 4, macros will simplify this.

```rust
use raftoral::{ReplicatedVar, SideEffect, RaftCluster};

// Example side-effect.
struct ApiCall { url: String }

impl SideEffect for ApiCall {
    type Output = Result<String, Error>;

    fn active(&self) -> Self::Output {
        // Real API call logic...
        Ok(format!("Response from {}", self.url))
    }

    fn no_op(&self) -> Self::Output {
        // Placeholder for simulation.
        Err(Error::SimulationMode)
    }
}

fn sample_workflow(cluster: Arc<RaftCluster>) -> Result<(), Error> {
    let retry_count = ReplicatedVar::new("retry_count", cluster.clone(), 0);
    let result = ReplicatedVar::new("result", cluster.clone(), None::<Result<String, Error>>);

    while retry_count.get() < 3 {
        let api = ApiCall { url: "https://example.com" };
        let response = api.execute(&cluster);
        result.set(Some(response.clone()));

        if response.is_ok() {
            break;
        }
        retry_count.set(retry_count.get() + 1);
    }

    if let Some(Ok(res)) = result.get() {
        // Use res...
        Ok(())
    } else {
        Err(Error::Failed)
    }
}
```

## Potential Issues and Mitigations

- **Non-Determinism:** Mitigated by checkpointing all side-effect results as `ReplicatedVar<Result<T, E>>`. Disallow non-deterministic APIs via lints/macros.
- **Partial Side Effects:** Assume idempotency; add compensation if needed via trait methods.
- **Performance:** Optimized for orchestration; use fast-forward simulation if needed.
- **State Serialization:** Require `Serialize/Deserialize` for checkpoint types.
- **Errors/Faults:** Replicated retries; rely on Raft for consensus issues.
- **Abstractions:** Keep annotations minimal; macros reduce boilerplate.
- **Security:** Runs in trusted microservice nodes; no arbitrary code.

## Getting Started

1. Clone the repo: `git clone https://github.com/yourusername/raftoral.git`
2. Build: `cargo build`
3. Run tests: `cargo test`
4. Implement milestones sequentially.

## Contributing

Contributions welcome! Focus on milestones, tests, and documentation.

## License

MIT License. See [LICENSE](LICENSE) for details.

