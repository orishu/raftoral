# Raftoral Quickstart Guide

## Overview

Raftoral is a fault-tolerant workflow execution engine using a novel **dual-layer Raft architecture**:

- **Management Layer (cluster_id = 0)**: Orchestrates cluster topology and node membership
- **Execution Layer (cluster_id = 1+)**: Small clusters (3-5 nodes) that execute workflows independently

This architecture enables horizontal scaling while maintaining strong consistency guarantees.

### Transport Protocols

Raftoral supports two transport protocols:

| Protocol | Use Case | Message Format |
|----------|----------|----------------|
| **gRPC** | Production deployments, high performance | Protobuf binary |
| **HTTP** | Development, browser integration, simpler debugging | JSON over REST |

Both protocols provide identical functionality - choose based on your operational needs.

## Prerequisites

- Rust 1.70+ with `cargo`
- For HTTP examples: `jq` (JSON processor)
- For gRPC examples: `grpcurl` (install via `brew install grpcurl`)

## Quick Start

### 1. Bootstrap a Single-Node Cluster

Start the first node to create a new cluster:

**gRPC Transport:**
```bash
cargo run --bin raftoral -- --listen 127.0.0.1:7001 --bootstrap
```

**HTTP Transport:**
```bash
cargo run --bin raftoral -- --transport http --listen 127.0.0.1:7001 --bootstrap
```

You'll see output like:
```
INFO Starting node, node_id: 1, address: 127.0.0.1:7001
INFO Starting gRPC server, address: 127.0.0.1:7001
INFO Node 1 is ready
```

The bootstrap node:
- Creates a new management cluster (cluster_id=0)
- Auto-assigns itself node_id=1
- Becomes the initial leader
- Automatically creates the first execution cluster (cluster_id=1)

### 2. Run a Workflow

Once your cluster is running, test it with the ping_pong workflow:

**gRPC Transport:**
```bash
./scripts/run_ping_pong.sh 127.0.0.1:7001
```

**HTTP Transport:**
```bash
./scripts/run_ping_pong_http.sh 127.0.0.1:7001
```

Both scripts will:
1. Start the workflow asynchronously
2. Wait for completion
3. Display the result (e.g., `"pong"` for input `"ping"`)

**Expected Output:**
```
=== Running ping_pong workflow via HTTP REST API ===

Node address: 127.0.0.1:7001
Workflow: ping_pong (v1)
Input: "ping"

Step 1: Starting workflow asynchronously...
{
  "workflow_id": "550e8400-e29b-41d4-a716-446655440000",
  "success": true,
  "execution_cluster_id": "1",
  "result": null,
  "error": null
}

Step 2: Waiting for workflow completion...
Workflow ID: 550e8400-e29b-41d4-a716-446655440000
Execution Cluster ID: 1

{
  "success": true,
  "result": "\"pong\"",
  "error": null
}

Done!
```

### 3. Add More Nodes (Multi-Node Cluster)

To create a fault-tolerant cluster, add additional nodes:

**Terminal 2 - Second Node:**
```bash
# gRPC
cargo run --bin raftoral -- --listen 127.0.0.1:7002 --peers 127.0.0.1:7001

# HTTP
cargo run --bin raftoral -- --transport http --listen 127.0.0.1:7002 --peers 127.0.0.1:7001
```

**Terminal 3 - Third Node:**
```bash
# gRPC
cargo run --bin raftoral -- --listen 127.0.0.1:7003 --peers 127.0.0.1:7001,127.0.0.1:7002

# HTTP
cargo run --bin raftoral -- --transport http --listen 127.0.0.1:7003 --peers 127.0.0.1:7001,127.0.0.1:7002
```

Each joining node will:
1. Discover the cluster via the `/discovery/peers` endpoint
2. Auto-assign itself a unique node_id
3. Join the management cluster
4. Be automatically added to an execution cluster by the ClusterManager

## CLI Reference

```
Usage: raftoral [OPTIONS] --listen <ADDRESS>

Options:
  -l, --listen <ADDRESS>
          Address to listen on (e.g., 127.0.0.1:7001)

  -a, --advertise <ADDRESS>
          Advertised address for other nodes to connect to
          If not specified, uses the listen address
          Use this when running behind NAT or in Docker

  -n, --node-id <NODE_ID>
          Node ID (optional, will be auto-assigned if joining)

  -p, --peers <PEERS>
          Comma-separated addresses of peer nodes for discovery
          Example: 192.168.1.10:7001,192.168.1.11:7001

  -b, --bootstrap
          Bootstrap a new cluster (use for the first node only)

  -s, --storage-path <PATH>
          Path for persistent storage (optional)
          If not provided, uses in-memory storage (data lost on restart)

  -t, --transport <PROTOCOL>
          Transport protocol: grpc or http (default: grpc)

  -h, --help
          Print help
```

### Environment Variables

- `RUST_LOG`: Set log level (`trace`, `debug`, `info`, `warn`, `error`)
  ```bash
  RUST_LOG=debug cargo run --bin raftoral -- --listen 127.0.0.1:7001 --bootstrap
  ```

## Discovery Protocol

When a node joins an existing cluster:

1. **Discovery Request**: Connects to each peer address
2. **Peer Response**: Returns cluster information:
   - Current leader node ID and address
   - Recommended voter/learner status
   - Current voter count
3. **Auto-Assignment**: Assigns itself `highest_known_node_id + 1`
4. **Registration**: Sends registration request to leader
5. **Automatic Placement**: ClusterManager adds node to appropriate clusters

### Discovery Endpoints

**gRPC:**
```bash
grpcurl -plaintext 127.0.0.1:7001 raftoral.RaftService/Discover
```

**HTTP:**
```bash
curl http://127.0.0.1:7001/discovery/peers | jq .
```

**Response:**
```json
{
  "node_id": 1,
  "highest_known_node_id": 1,
  "address": "127.0.0.1:7001",
  "management_leader_node_id": 1,
  "management_leader_address": "127.0.0.1:7001",
  "should_join_as_voter": true,
  "current_voter_count": 1,
  "max_voters": 5
}
```

## Workflow Execution

### Asynchronous Execution (Recommended)

Workflows run asynchronously - you start them and poll for results.

**gRPC Example:**
```bash
# 1. Start workflow
grpcurl -plaintext -d '{
  "workflow_type": "ping_pong",
  "version": 1,
  "input_json": "\"ping\""
}' 127.0.0.1:7001 raftoral.WorkflowManagement/RunWorkflowAsync

# Returns: { "workflow_id": "abc-123", "execution_cluster_id": "1" }

# 2. Wait for completion
grpcurl -plaintext -d '{
  "workflow_id": "abc-123",
  "execution_cluster_id": "1",
  "timeout_seconds": 30
}' 127.0.0.1:7001 raftoral.WorkflowManagement/WaitForWorkflowCompletion

# Returns: { "success": true, "result_json": "\"pong\"" }
```

**HTTP Example:**
```bash
# 1. Start workflow
curl -X POST http://127.0.0.1:7001/workflow/execute \
  -H "Content-Type: application/json" \
  -d '{
    "name": "ping_pong",
    "version": 1,
    "input": "\"ping\""
  }' | jq .

# Returns: { "workflow_id": "abc-123", "execution_cluster_id": "1" }

# 2. Wait for completion
curl -X GET "http://127.0.0.1:7001/workflow/wait?workflow_id=abc-123&execution_cluster_id=1&timeout_seconds=30" | jq .

# Returns: { "success": true, "result": "\"pong\"" }
```

### Using the Helper Scripts

For quick testing, use the provided scripts:

```bash
# gRPC transport
./scripts/run_ping_pong.sh [node_address]

# HTTP transport
./scripts/run_ping_pong_http.sh [node_address]
```

Both scripts handle the full async workflow execution flow.

## Persistent Storage

By default, nodes use in-memory storage (data is lost on restart). For production deployments, enable persistent storage:

```bash
cargo run --bin raftoral -- \
  --listen 127.0.0.1:7001 \
  --bootstrap \
  --storage-path ./raftoral_data
```

This creates a RocksDB database at `./raftoral_data/node_1/` with:
- Raft log entries
- Snapshots
- Node identity
- Hard state (term, vote, commit)

### Node Restart

With persistent storage, nodes can restart and rejoin:

```bash
# First start - creates new identity
cargo run --bin raftoral -- --listen 127.0.0.1:7001 --bootstrap --storage-path ./node1_data

# Later restart - resumes with same node_id
cargo run --bin raftoral -- --listen 127.0.0.1:7001 --storage-path ./node1_data
```

The node will:
1. Load its persisted node_id
2. Resume with existing Raft state
3. Reconnect to the cluster
4. Catch up on missed entries

## Architecture Deep Dive

### Dual-Layer Raft

**Management Cluster (cluster_id=0):**
- Single cluster spanning all nodes
- Manages cluster topology
- Routes workflow requests to execution clusters
- Supports voter/learner mode for scalability (default: 5 voters)

**Execution Clusters (cluster_id=1+):**
- Small, independent Raft clusters (3-5 nodes)
- Execute workflows via consensus
- Automatic creation and rebalancing via ClusterManager
- Each cluster provides fault tolerance for its workflows

### Message Routing

All Raft messages contain a `cluster_id` field:
- Messages with `cluster_id=0` → Management cluster
- Messages with `cluster_id≥1` → Execution cluster

The `ClusterRouter` component inspects incoming messages and routes them to the appropriate cluster.

### Automatic Cluster Management

The `ClusterManager` automatically:
- **Creates clusters**: When bootstrap node joins management cluster
- **Balances nodes**: Assigns joining nodes to under-capacity clusters
- **Splits clusters**: When a cluster grows too large (≥6 nodes → split to 3+3)
- **Consolidates clusters**: When node count decreases
- **Detects failures**: Removes unresponsive nodes after timeout

## Testing

### Two-Node Cluster Test

Run the comprehensive two-node test:

```bash
./scripts/test_two_node_cluster.sh
```

This script:
1. Starts a bootstrap node
2. Starts a joining node
3. Verifies cluster formation
4. Runs a workflow
5. Cleans up gracefully

### Node Restart Test

Test persistent storage and node restart:

```bash
./scripts/test_resume.sh
```

This verifies:
- Node identity persistence
- Raft state recovery
- Cluster rejoin behavior

### Unit Tests

Run all library tests:

```bash
# All tests
cargo test

# Specific modules
cargo test http::
cargo test management::
cargo test workflow::
```

## Production Deployment

### Multi-Node Setup

**Node 1 (Bootstrap):**
```bash
cargo run --release --bin raftoral -- \
  --listen 10.0.1.10:7001 \
  --storage-path /var/lib/raftoral/node1 \
  --bootstrap
```

**Node 2:**
```bash
cargo run --release --bin raftoral -- \
  --listen 10.0.1.11:7001 \
  --storage-path /var/lib/raftoral/node2 \
  --peers 10.0.1.10:7001
```

**Node 3:**
```bash
cargo run --release --bin raftoral -- \
  --listen 10.0.1.12:7001 \
  --storage-path /var/lib/raftoral/node3 \
  --peers 10.0.1.10:7001,10.0.1.11:7001
```

### Docker Deployment

Example `docker-compose.yml`:

```yaml
version: '3.8'
services:
  raftoral-1:
    build: .
    command: [
      "--listen", "0.0.0.0:7001",
      "--advertise", "raftoral-1:7001",
      "--bootstrap",
      "--storage-path", "/data"
    ]
    volumes:
      - raftoral-1-data:/data
    ports:
      - "7001:7001"

  raftoral-2:
    build: .
    command: [
      "--listen", "0.0.0.0:7001",
      "--advertise", "raftoral-2:7001",
      "--peers", "raftoral-1:7001",
      "--storage-path", "/data"
    ]
    volumes:
      - raftoral-2-data:/data
    ports:
      - "7002:7001"
    depends_on:
      - raftoral-1

  raftoral-3:
    build: .
    command: [
      "--listen", "0.0.0.0:7001",
      "--advertise", "raftoral-3:7001",
      "--peers", "raftoral-1:7001,raftoral-2:7001",
      "--storage-path", "/data"
    ]
    volumes:
      - raftoral-3-data:/data
    ports:
      - "7003:7001"
    depends_on:
      - raftoral-1
      - raftoral-2

volumes:
  raftoral-1-data:
  raftoral-2-data:
  raftoral-3-data:
```

### Health Checks

Monitor cluster health:

**gRPC:**
```bash
grpcurl -plaintext 127.0.0.1:7001 grpc.health.v1.Health/Check
```

**HTTP:**
```bash
curl http://127.0.0.1:7001/health | jq .
```

**Response:**
```json
{
  "status": "ok",
  "node_id": 1,
  "is_leader": true
}
```

## API Reference

### HTTP Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/workflow/execute` | POST | Start a workflow asynchronously |
| `/workflow/wait` | GET | Wait for workflow completion |
| `/raft/message` | POST | Send Raft message (internal) |
| `/discovery/register` | POST | Register new node |
| `/discovery/peers` | GET | Get cluster information |
| `/health` | GET | Health check |

### gRPC Services

```protobuf
service RaftService {
  rpc SendMessage(GenericMessage) returns (MessageResponse);
  rpc Discover(DiscoveryRequest) returns (DiscoveryResponse);
}

service WorkflowManagement {
  rpc RunWorkflowAsync(RunWorkflowRequest) returns (RunWorkflowAsyncResponse);
  rpc WaitForWorkflowCompletion(WaitForWorkflowRequest) returns (RunWorkflowResponse);
  rpc RunWorkflowSync(RunWorkflowRequest) returns (RunWorkflowResponse);
}
```

## Troubleshooting

### "No execution clusters available"

**Cause**: Management cluster hasn't created execution clusters yet (only on bootstrap).

**Solution**: Wait a few seconds for the ClusterManager to create clusters, or check logs for errors.

### "Could not discover any peers"

**Cause**: Peer nodes are not reachable.

**Solutions**:
- Verify peer addresses are correct
- Ensure peer nodes are running
- Check network connectivity and firewall rules
- Try pinging peers: `curl http://<peer>/health`

### "Failed to add node to cluster"

**Cause**: Leader not available or already in cluster.

**Solutions**:
- Check that bootstrap node is running
- Verify you're not trying to bootstrap multiple nodes
- Check logs for ConfChange rejection reasons

### "Address already in use"

**Cause**: Port is already bound by another process.

**Solutions**:
- Choose a different port
- Kill the existing process: `lsof -ti:7001 | xargs kill -9`
- Check for zombie processes: `ps aux | grep raftoral`

### Mixed Transport Protocols

**Problem**: Can I mix gRPC and HTTP nodes in the same cluster?

**Answer**: No. All nodes in a cluster must use the same transport protocol. When joining, new nodes automatically detect and use the cluster's protocol.

## Next Steps

- Read [V2_ARCHITECTURE.md](V2_ARCHITECTURE.md) for architecture details
- See [README.md](../README.md) for workflow development guide
- Check [examples/](../examples/) for advanced workflow patterns
- Review [SCALABILITY_2.md](SCALABILITY_2.md) for scaling considerations

## Summary

Raftoral provides fault-tolerant workflow execution with:

✅ **Dual-layer Raft** - Management + Execution clusters for horizontal scaling
✅ **Automatic topology management** - ClusterManager handles node placement
✅ **Multiple transports** - gRPC for performance, HTTP for simplicity
✅ **Persistent storage** - RocksDB-backed Raft logs and snapshots
✅ **Auto-discovery** - Nodes join automatically via discovery protocol
✅ **Strong consistency** - All workflow operations go through Raft consensus
✅ **Fault tolerance** - Survives minority node failures per cluster

Start building fault-tolerant workflows today!
