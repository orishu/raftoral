# gRPC Server Usage Guide

## Overview

Raftoral now includes a production-ready gRPC server with:
- **Graceful shutdown** support
- **Peer discovery** via RPC
- **Auto node ID assignment** for joining nodes
- **CLI interface** for easy deployment

## Starting a Cluster

### Bootstrap First Node

To start a new cluster, use the `--bootstrap` flag:

```bash
cargo run -- --address 127.0.0.1:5001 --bootstrap
```

Output:
```
🚀 Starting Raftoral node...
   Address: 127.0.0.1:5001
   Mode: Bootstrap (starting new cluster)
   Node ID: 1
   Cluster nodes: 1 total
✓ Raft cluster initialized
✓ gRPC server listening on 127.0.0.1:5001
✓ Workflow runtime ready

🎉 Node 1 is running!
   Press Ctrl+C to shutdown gracefully
```

### Join Existing Cluster

To join an existing cluster, specify peer addresses:

```bash
cargo run -- --address 127.0.0.1:5002 --peers 127.0.0.1:5001
```

The node will:
1. Discover the existing peer(s)
2. Query their highest known node ID
3. Auto-assign itself the next available node ID

Output:
```
🚀 Starting Raftoral node...
   Address: 127.0.0.1:5002
   Mode: Join existing cluster
   Discovering peers: ["127.0.0.1:5001"]
✓ Discovered peer at 127.0.0.1:5001: node_id=1, role=Leader, highest_known=1
   Auto-assigned node ID: 2
   Cluster nodes: 2 total
✓ Raft cluster initialized
✓ gRPC server listening on 127.0.0.1:5002
✓ Workflow runtime ready

🎉 Node 2 is running!
   Press Ctrl+C to shutdown gracefully
```

### Join with Multiple Peers

For resilience, specify multiple peers:

```bash
cargo run -- --address 127.0.0.1:5003 --peers 127.0.0.1:5001,127.0.0.1:5002
```

The discovery will try all peers and use the highest known node ID from all responses.

### Manual Node ID Assignment

You can explicitly set the node ID:

```bash
cargo run -- --address 127.0.0.1:5002 --node-id 5 --peers 127.0.0.1:5001
```

**Note**: Ensure the node ID doesn't conflict with existing nodes!

## CLI Options

```
Usage: raftoral [OPTIONS] --address <ADDRESS>

Options:
  -a, --address <ADDRESS>  Address to bind the gRPC server to (e.g., 127.0.0.1:5001)
  -n, --node-id <NODE_ID>  Node ID (optional, will be auto-assigned if joining)
  -p, --peers <PEERS>      Comma-separated addresses of peer nodes to discover
  -b, --bootstrap          Bootstrap a new cluster (use for the first node)
  -h, --help              Print help
```

## Discovery Protocol

### How It Works

When a node starts in join mode:

1. **Discovery Request**: Connects to each peer via gRPC
2. **Peer Response**: Each peer returns:
   - `node_id`: The peer's Raft node ID
   - `role`: Current Raft role (Leader/Follower)
   - `highest_known_node_id`: Highest node ID the peer knows about
   - `address`: The peer's listening address

3. **Node ID Assignment**:
   - Collects all `highest_known_node_id` values
   - Assigns itself: `max(highest_known_node_id) + 1`
   - Falls back to `1` if no peers discovered

### Discovery RPC Definition

```protobuf
service RaftService {
    rpc Discover(DiscoveryRequest) returns (DiscoveryResponse);
}

message DiscoveryRequest {
    // Empty for now
}

message DiscoveryResponse {
    uint64 node_id = 1;
    RaftRole role = 2;
    uint64 highest_known_node_id = 3;
    string address = 4;
}
```

## Graceful Shutdown

Press `Ctrl+C` to trigger graceful shutdown:

```
^C
🛑 Shutdown signal received, stopping gracefully...
✓ Server stopped
```

The server uses tonic's `serve_with_shutdown` to:
- Stop accepting new connections
- Complete in-flight requests
- Clean up resources

## Programmatic Usage

### Starting a Server

```rust
use raftoral::raft::generic::grpc_transport::{GrpcClusterTransport, NodeConfig};
use raftoral::grpc::start_grpc_server;
use std::sync::Arc;

let nodes = vec![
    NodeConfig { node_id: 1, address: "127.0.0.1:5001".to_string() }
];

let transport = Arc::new(GrpcClusterTransport::new(nodes));
let cluster = transport.create_cluster(1).await?;

// Returns a handle for graceful shutdown
let server_handle = start_grpc_server(
    "127.0.0.1:5001".to_string(),
    transport,
    cluster,
    1
).await?;

// Later, shutdown gracefully
server_handle.shutdown();
```

### Discovery API

```rust
use raftoral::grpc::bootstrap::{discover_peers, next_node_id};

// Discover multiple peers
let peers = discover_peers(vec![
    "127.0.0.1:5001".to_string(),
    "127.0.0.1:5002".to_string(),
]).await;

// Determine next node ID
let my_node_id = next_node_id(&peers);
println!("Assigned node ID: {}", my_node_id);
```

## Testing

### Run gRPC Tests

```bash
cargo test --test grpc_test
```

Tests include:
- `test_grpc_message_passing`: Verifies message transmission over gRPC
- `test_grpc_transport_node_management`: Tests dynamic node add/remove
- `test_grpc_transport_node_ids`: Validates node ID retrieval
- `test_discovery_rpc`: Tests peer discovery protocol

### Bootstrap Tests

```bash
cargo test --lib bootstrap
```

Tests the node ID assignment logic with various peer configurations.

## Production Deployment

### Multi-Node Cluster Setup

**Terminal 1 - Bootstrap Node:**
```bash
cargo run --release -- --address 10.0.1.10:5001 --bootstrap
```

**Terminal 2 - Second Node:**
```bash
cargo run --release -- --address 10.0.1.11:5001 --peers 10.0.1.10:5001
```

**Terminal 3 - Third Node:**
```bash
cargo run --release -- --address 10.0.1.12:5001 --peers 10.0.1.10:5001,10.0.1.11:5001
```

### Docker Deployment

Example `docker-compose.yml`:

```yaml
version: '3'
services:
  raftoral-1:
    build: .
    command: ["--address", "0.0.0.0:5001", "--bootstrap"]
    ports:
      - "5001:5001"

  raftoral-2:
    build: .
    command: ["--address", "0.0.0.0:5001", "--peers", "raftoral-1:5001"]
    ports:
      - "5002:5001"
    depends_on:
      - raftoral-1

  raftoral-3:
    build: .
    command: ["--address", "0.0.0.0:5001", "--peers", "raftoral-1:5001,raftoral-2:5001"]
    ports:
      - "5003:5001"
    depends_on:
      - raftoral-1
      - raftoral-2
```

## Architecture

### Components

1. **GrpcServerHandle**: Manages server lifecycle with graceful shutdown
2. **RaftServiceImpl**: Implements gRPC service with:
   - `SendMessage`: Raft protocol communication
   - `Discover`: Peer discovery for bootstrap
3. **Bootstrap Module**: Node ID assignment and peer discovery logic

### Key Design Decisions

- **Graceful Shutdown**: Uses `oneshot` channel to signal shutdown
- **Auto Node ID**: Prevents manual coordination during cluster growth
- **Discovery-based Join**: Nodes automatically learn cluster topology
- **Transport Integration**: Works seamlessly with `GrpcClusterTransport`

## Troubleshooting

### "Could not discover any peers"

**Cause**: No peers are reachable at the specified addresses.

**Solutions**:
- Verify peer addresses are correct
- Ensure peer nodes are running
- Check network connectivity and firewall rules

### "Node ID already exists"

**Cause**: Manually specified node ID conflicts with existing node.

**Solution**:
- Remove `--node-id` flag to use auto-assignment
- Or choose a unique node ID

### Server fails to bind

**Cause**: Address already in use or insufficient permissions.

**Solutions**:
- Choose a different port
- Kill process using the port: `lsof -ti:5001 | xargs kill`
- Use `0.0.0.0` instead of `127.0.0.1` for Docker deployments

## Future Enhancements

Potential improvements:
- **TLS/mTLS**: Secure gRPC connections
- **Health checks**: Dedicated health check endpoint
- **Metrics**: Prometheus metrics export
- **Dynamic membership**: Add Raft `ConfChange` integration
- **Persistence**: Snapshot and WAL storage configuration
- **Load balancing**: Smart peer selection for discovery

## Files Modified

**New Files**:
- `src/grpc/bootstrap.rs` - Peer discovery and node ID logic
- `docs/GRPC_SERVER_USAGE.md` - This guide

**Modified Files**:
- `proto/raft.proto` - Added `Discover` RPC and messages
- `src/grpc/server.rs` - Added graceful shutdown and discovery handler
- `src/main.rs` - Complete rewrite with CLI interface
- `Cargo.toml` - Added `clap` for CLI parsing
- `tests/grpc_test.rs` - Added discovery test
