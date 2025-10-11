# Comparison: Raftoral vs. Temporal vs. DBOS

This document provides a detailed comparison between Raftoral, Temporal, and DBOS—three systems designed for building fault-tolerant, durable workflows and stateful applications.

**All three systems are designed for long-running services**, not serverless/FaaS deployments (like AWS Lambda). Workflows execute continuously and maintain state across hours, days, or even months, which requires persistent runtime processes.

The key differentiator is **infrastructure requirements**:
- **Raftoral**: Embedded orchestration (no separate infrastructure)
- **Temporal**: Separate orchestrator cluster + database cluster
- **DBOS**: Requires database cluster for state persistence

The comparison covers key aspects such as overview, architecture, language support, fault tolerance, state management, scalability, complexity/ease of use, use cases, pros/cons, and when to choose each. Data is based on official documentation and project descriptions as of October 2025.

## Overview

- **Raftoral**: A pure Rust library for fault-tolerant, distributed workflows using embedded Raft consensus. Orchestration runs inside your application processes via peer-to-peer coordination, eliminating separate infrastructure. Inspired by Temporal but designed with no dedicated orchestrator servers or databases.
- **Temporal**: An open-source platform for orchestrating durable workflow executions that survive failures. Requires deploying a Temporal Server cluster and backend database (Cassandra, MySQL, or PostgreSQL). Workers run in your services and communicate with the central server for orchestration.
- **DBOS**: A database-oriented system for building reliable, stateful applications and workflows. Uses annotations to make existing code durable, backed by PostgreSQL for execution state. Requires database infrastructure but no separate orchestrator servers.

## Architecture

- **Raftoral**: Decentralized, peer-to-peer using Raft consensus embedded in Rust applications. Nodes form self-coordinating clusters with parallel workflow execution. State is managed in-memory with Raft log replication and snapshots for catch-up. Components include Raft layer (consensus), workflow layer (execution, checkpoints), and runtime layer (gRPC for transport). No external database; all state is replicated across nodes.
- **Temporal**: Centralized service with a Temporal Server (Go-based) and user-hosted Worker Processes. Uses event sourcing to record workflow history in a backend database (e.g., Cassandra, MySQL, PostgreSQL). Workflows run in workers via SDKs, communicating with the server for orchestration. Event History enables replay for recovery.
- **DBOS**: Database-centric, with workflows annotated in code and execution state stored in PostgreSQL. No separate orchestration server; durability is achieved through step-by-step recording in the database. Supports durable queues and event processing, with built-in observability via a console.

| Aspect              | Raftoral                          | Temporal                          | DBOS                              |
|---------------------|-----------------------------------|-----------------------------------|-----------------------------------|
| Core Mechanism      | Raft consensus for replication    | Event sourcing with central server| Database-backed durable steps     |
| Infrastructure      | Embedded in app nodes (P2P)       | Central server + workers          | Database (Postgres) + annotations |
| State Storage       | In-memory + Raft logs/snapshots   | External database (e.g., Cassandra)| Postgres for execution state      |

## Language Support

- **Raftoral**: Exclusively Rust (requires 1.70+). Workflows are defined as async closures with serializable types.
- **Temporal**: Multi-language SDKs: Go, Java, Python, .NET, PHP, TypeScript/JavaScript. Allows polyglot workflows.
- **DBOS**: Primarily TypeScript and Python, with annotations for durability. Integrates with existing stacks like Kafka and Slack.

## Fault Tolerance

- **Raftoral**: Automatic leader election via Raft; workflows continue on any node post-failover. Replicated checkpoints ensure consistency. Handles node additions/removals dynamically without downtime. Parallel execution distributes load and enhances resilience.
- **Temporal**: Durable execution via Event History replay; resumes from last event after failures. Supports retries, timeouts, and failure representations in SDKs. Distinguishes application vs. platform failures.
- **DBOS**: Automatic recovery from last successful step after crashes/restarts. Guarantees exactly-once processing with idempotency. Retries built into workflows; durable queues ensure task completion.

| Aspect              | Raftoral                          | Temporal                          | DBOS                              |
|---------------------|-----------------------------------|-----------------------------------|-----------------------------------|
| Failover            | Raft leader promotion             | Event replay from database        | Resume from database step         |
| Recovery Granularity| Checkpoint-based                  | Event-by-event                    | Step-by-step                      |
| Node Failures       | Handles via consensus             | Workers can fail; server resilient| Database ensures persistence      |

## State Management

- **Raftoral**: Replicated variables/checkpoints synchronized via Raft proposals. Deterministic execution required; side effects computed once on leader and queued for followers. Snapshots for efficient catch-up.
- **Temporal**: Event-sourced state in database; workflows replay events to reconstruct state. Supports local state with exclusive access, durable timers, and signals for external events.
- **DBOS**: Database-stored execution state; steps are checkpointed automatically. Handles stateful logic like loops/conditionals; durable queues for distributed state.

## Scalability

- **Raftoral**: Dynamic scaling with node addition/removal at runtime. Parallel execution across nodes for load distribution. Suitable for small to medium clusters; performance: 30-171µs per command.
- **Temporal**: Handles millions to billions of workflows; suspended workflows use minimal resources. Scales via clustered servers and multiple workers.
- **DBOS**: Autoscaling in cloud deployments; handles large-scale tasks (e.g., genomic data transfers 40x faster than alternatives). 25x better price-performance than AWS Lambda + Step Functions.

## Complexity / Ease of Use

- **Raftoral**: Minimal annotations (checkpoints via macros); workflows look like single-threaded code. Requires deterministic design and Rust expertise. No external setup, but manages cluster config.
- **Temporal**: SDKs abstract durability; write workflows as sequential code. Involves setting up server/database, workers, and namespaces. Higher initial complexity but powerful for complex orchestrations.
- **DBOS**: Lowest barrier: Add annotations (@DBOS.workflow(), @DBOS.step()) to existing code. No rearchitecting; quick starts with templates. Built-in observability reduces debugging effort.

| Aspect              | Raftoral                          | Temporal                          | DBOS                              |
|---------------------|-----------------------------------|-----------------------------------|-----------------------------------|
| Learning Curve      | Rust-specific, deterministic focus| SDK setup + concepts (events)     | Annotation-based, minimal changes |
| Setup Overhead      | Embedded, config-based            | Server + DB deployment            | Database connection + annotations |

## Use Cases

- **Raftoral**: Long-running services needing embedded workflow orchestration (e.g., payment processing services, order management systems, microservice coordination). Best for teams wanting to minimize infrastructure components while maintaining fault tolerance.
- **Temporal**: Complex, long-running, failure-prone processes requiring mature tooling (e.g., billing systems, CI/CD pipelines, multi-step microservices orchestration). Suited for polyglot teams and workflows with complex patterns (activities, child workflows, signals).
- **DBOS**: Reliable data pipelines, AI agents, event processing, cron jobs, e-commerce checkouts. Great for adding durability to existing services (e.g., Slack integrations, genomic processing) with minimal code changes.

## Pros and Cons

### Raftoral
- **Pros**: No separate infrastructure; unified operations; dynamic scaling; type-safe; low latency for checkpoints.
- **Cons**: Rust-only; requires long-running services; requires determinism; in-memory (persistence planned); no built-in compensation.

### Temporal
- **Pros**: Multi-language; mature ecosystem; handles arbitrary failures; scalable for massive workloads.
- **Cons**: Requires central server/database; potential network overhead; steeper setup for small projects.

### DBOS
- **Pros**: Easy integration (annotations); automatic recovery/observability; cost-efficient; versatile deployments.
- **Cons**: Database dependency (Postgres); less focus on distributed consensus; potential vendor tie-in for cloud features.

## When to Choose

- **Choose Raftoral** if you're building long-running services in Rust and want to minimize operational overhead by embedding orchestration. Best for teams wanting unified infrastructure (no separate orchestrator/database clusters) while maintaining fault tolerance and dynamic scaling.
- **Choose Temporal** if you need a battle-tested platform for complex, polyglot orchestrations with event sourcing. Ideal for large-scale, long-running processes in failure-prone environments where mature tooling and multi-language support justify the infrastructure overhead.
- **Choose DBOS** if you're adding durability to existing services quickly, with a focus on database-backed simplicity and observability. Suited for rapid prototyping and teams already comfortable with PostgreSQL infrastructure.

## Summary

Each system offers unique trade-offs:

- **Raftoral** excels at embedded orchestration without external dependencies, perfect for Rust applications that need peer-to-peer resilience
- **Temporal** provides the most mature, battle-tested platform for enterprise-scale workflow orchestration across multiple languages
- **DBOS** offers the fastest path to adding durability to existing applications through simple annotations

For more details, refer to the respective documentation:
- Raftoral: [GitHub Repo](https://github.com/orishu/raftoral)
- Temporal: [Docs](https://docs.temporal.io/)
- DBOS: [Website](https://www.dbos.dev/)
