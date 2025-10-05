# gRPC Server Usage Guide

## Overview

Raftoral includes a **production-ready, generic gRPC server** with:
- **Fully generic transport** - Works with any command type, not just workflows
- **Graceful shutdown** support
- **Peer discovery** via RPC
- **Auto node ID assignment** for joining nodes
- **Automatic node addition/removal** on join/leave
- **CLI interface** for easy deployment
- **TLS/authentication support** via custom ChannelBuilder

## Key Feature: Generic Architecture ✅

The gRPC server is **completely decoupled from the workflow system**. It can be used for:
- Workflow orchestration (WorkflowCommandExecutor)
- Distributed databases
- Configuration management
- Any Raft-based application

The server works with ANY command type `C` implementing:
```rust
Clone + Debug + Serialize + DeserializeOwned + Send + Sync + 'static
```

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
   Adding node 1 to existing cluster...
✓ Node added to cluster
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
4. **Automatically add itself to the cluster via ConfChange**

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
   Adding node 2 to existing cluster...
✓ Node added to cluster
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

4. **Cluster Join**:
   - Automatically calls `cluster.add_node(node_id)` after initialization
   - Uses automatic leader discovery (no manual leader tracking needed)
   - Leader applies ConfChange to add the new node

### Discovery RPC Definition

```protobuf
service RaftService {
    rpc SendMessage(GenericMessage) returns (MessageResponse);
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
   Removing node 2 from cluster...
✓ Node removed from cluster
✓ Server stopped
```

The shutdown process:
1. **Removes node from cluster** via ConfChange (automatic leader discovery)
2. **Stops accepting new connections**
3. **Completes in-flight requests**
4. **Cleans up resources**

The server uses tonic's `serve_with_shutdown` for clean termination.

## Programmatic Usage

### Starting a Server (Basic)

```rust
use raftoral::raft::generic::grpc_transport::{GrpcClusterTransport, NodeConfig};
use raftoral::grpc::start_grpc_server;
use std::sync::Arc;

let nodes = vec![
    NodeConfig { node_id: 1, address: "127.0.0.1:5001".to_string() }
];

let transport = Arc::new(
    GrpcClusterTransport::<WorkflowCommandExecutor>::new(nodes)
);
transport.start().await?;

// Returns a handle for graceful shutdown
let server_handle = start_grpc_server(
    "127.0.0.1:5001".to_string(),
    transport.clone(),
    1  // node_id
).await?;

// Later, shutdown gracefully
server_handle.shutdown();
```

### Starting a Server with TLS

```rust
use raftoral::grpc::start_grpc_server_with_config;
use tonic::transport::{Server, ServerTlsConfig, Identity};

// Load TLS certificate and key
let cert = std::fs::read("server.crt")?;
let key = std::fs::read("server.key")?;
let identity = Identity::from_pem(cert, key);

// Custom server configuration
let server_config = Box::new(|server: Server| {
    let tls = ServerTlsConfig::new()
        .identity(identity);

    server.tls_config(tls).unwrap()
});

// Start server with TLS
let server_handle = start_grpc_server_with_config(
    "127.0.0.1:5001".to_string(),
    transport,
    1,
    Some(server_config)
).await?;
```

### Custom Channel Builder (Client TLS)

```rust
use raftoral::grpc::client::{ChannelBuilder, default_channel_builder};
use raftoral::raft::generic::grpc_transport::GrpcClusterTransport;
use tonic::transport::ClientTlsConfig;

// Custom TLS channel builder
let ca_cert = std::fs::read("ca.crt")?;
let client_cert = std::fs::read("client.crt")?;
let client_key = std::fs::read("client.key")?;
let client_identity = Identity::from_pem(client_cert, client_key);

let channel_builder: ChannelBuilder = Arc::new(|address: String| {
    Box::pin(async move {
        let tls = ClientTlsConfig::new()
            .ca_certificate(tonic::transport::Certificate::from_pem(ca_cert))
            .identity(client_identity);

        Channel::from_shared(format!("https://{}", address))?
            .tls_config(tls)?
            .connect()
            .await
    })
});

// Create transport with custom TLS
let transport = GrpcClusterTransport::new_with_channel_builder(
    nodes,
    channel_builder
);
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

## Generic Message Flow

### How Messages Are Sent Over gRPC

**Sender Side**:
```
Application creates Message<C>
  ↓
Message::Propose { command: MyCommand::Foo, ... }
  ↓
to_serializable() → SerializableMessage<C>
  ↓
serde_json::to_vec() → JSON bytes
  ↓
GenericMessage { serialized_message: bytes }
  ↓
gRPC SendMessage RPC
  ↓
Network
```

**Receiver Side**:
```
Network
  ↓
gRPC receives GenericMessage
  ↓
Extract bytes from serialized_message field
  ↓
serde_json::from_slice() → SerializableMessage<C>
  ↓
from_serializable() → Message<C>
  ↓
Inject into local node's mailbox
  ↓
Raft processes message
```

**Key Points**:
- Server is generic over `CommandExecutor` type
- No knowledge of specific command types
- All serialization handled via JSON
- Raft protocol messages use protobuf internally

### Supported Message Types

All message types work seamlessly:
- `Message::Propose` - Command proposals
- `Message::Raft` - Raft protocol messages
- `Message::ConfChangeV2` - Configuration changes
- `Message::Campaign` - Leadership election
- `Message::AddNode` - Add node (with automatic leader discovery)
- `Message::RemoveNode` - Remove node (with automatic leader discovery)

## Testing

### Run gRPC Tests

```bash
# All gRPC transport tests
cargo test --lib grpc_transport

# With output
cargo test --lib grpc_transport -- --nocapture
```

Tests include:
- `test_grpc_transport_creation`: Node configuration and address lookup
- `test_add_remove_node`: Dynamic node management
- `test_grpc_client_connect`: **Generic message forwarding with payload verification**

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

Each node will:
1. Auto-assign node ID
2. Start gRPC server
3. **Automatically add itself to cluster** via ConfChange
4. Begin participating in Raft consensus

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
2. **RaftServiceImpl<E>**: Generic gRPC service implementation
   - `SendMessage`: Generic message forwarding (any command type)
   - `Discover`: Peer discovery for bootstrap
3. **SerializableMessage<C>**: Wire format without callbacks
4. **Bootstrap Module**: Node ID assignment and peer discovery
5. **GrpcClusterTransport<E>**: Generic transport implementation

### Key Design Decisions

- **Complete Genericity**: Works with any CommandExecutor type
- **Graceful Shutdown**: Uses `oneshot` channel to signal shutdown
- **Auto Node ID**: Prevents manual coordination during cluster growth
- **Automatic Join/Leave**: Nodes add/remove themselves via ConfChange
- **Automatic Leader Discovery**: No manual leader tracking needed
- **SerializableMessage Pattern**: Enables network serialization without callbacks
- **Minimal Proto File**: 41 lines, zero business logic

## Generic Architecture Benefits

### 1. Reusable Across Applications

The same gRPC infrastructure can be used for:
```rust
// Workflow orchestration
GrpcClusterTransport::<WorkflowCommandExecutor>::new(nodes)

// Custom distributed database
struct DbExecutor;
impl CommandExecutor for DbExecutor {
    type Command = DbCommand;
    // ...
}
GrpcClusterTransport::<DbExecutor>::new(nodes)  // Works!
```

### 2. Type Safety Maintained

Despite being generic, compile-time checking is preserved:
```rust
// This compiles
let msg = Message::Propose {
    command: MyCommand::Write { key: "foo", value: "bar" },
    ...
};
grpc_client.send_message(&msg).await?;

// This fails at compile time
let msg = Message::Propose {
    command: WrongCommand::Delete,  // Type error!
    ...
};
```

### 3. Minimal Protocol Definition

Proto file is **41 lines** with zero business logic:
- `GenericMessage` - JSON bytes envelope
- `RaftService` - Two RPC methods
- Discovery messages - Generic cluster info
- No workflow-specific types

## Troubleshooting

### "Could not discover any peers"

**Cause**: No peers are reachable at the specified addresses.

**Solutions**:
- Verify peer addresses are correct
- Ensure peer nodes are running
- Check network connectivity and firewall rules

### "Failed to add node to cluster"

**Cause**: Leader not available or ConfChange rejected.

**Solutions**:
- Main.rs handles this gracefully with "Continuing anyway" message
- Node may already be in cluster
- Check that bootstrap node is running and healthy

### Server fails to bind

**Cause**: Address already in use or insufficient permissions.

**Solutions**:
- Choose a different port
- Kill process using the port: `lsof -ti:5001 | xargs kill`
- Use `0.0.0.0` instead of `127.0.0.1` for Docker deployments

### "Message deserialization failed"

**Cause**: Command type mismatch between nodes.

**Solutions**:
- Ensure all nodes use same CommandExecutor type
- Verify command definitions match across nodes
- Check serde versions are compatible

## Future Enhancements

Potential improvements:
- **Compression**: gRPC built-in compression for large messages
- **Health checks**: Dedicated health check endpoint
- **Metrics**: Prometheus metrics export
- **Retry logic**: Exponential backoff for transient failures
- **Load balancing**: Smart peer selection for discovery
- **Persistence**: Snapshot and WAL storage configuration

## Files Overview

**Core Implementation**:
- `proto/raft.proto` - 41 lines of generic protocol
- `src/grpc/server.rs` - Generic gRPC server
- `src/grpc/client.rs` - Generic gRPC client with custom ChannelBuilder support
- `src/grpc/bootstrap.rs` - Peer discovery and node ID logic
- `src/raft/generic/grpc_transport.rs` - Generic transport implementation
- `src/raft/generic/message.rs` - SerializableMessage architecture

**Application**:
- `src/main.rs` - CLI with automatic join/leave
- `Cargo.toml` - Dependencies (tonic, prost, serde_json)

## Summary

The gRPC server provides a **production-ready, generic Raft communication layer**:

✅ **Zero workflow coupling** - Works with any command type
✅ **Automatic cluster management** - Join/leave handled automatically
✅ **Automatic leader discovery** - No manual tracking needed
✅ **Type-safe** - Full compile-time checking
✅ **Secure** - TLS/mTLS support via custom builders
✅ **Graceful shutdown** - Clean termination with ConfChange removal
✅ **Production-ready** - CLI, Docker support, error handling

The implementation is a **reusable Raft infrastructure** suitable for any distributed consensus application.
