# gRPC Cluster Transport Implementation

## Overview

This document describes the gRPC-based cluster transport implementation for Raftoral, enabling Raft nodes to communicate over network instead of just in-memory channels.

## Architecture

### Components

1. **Generic GrpcClusterTransport** (`src/raft/generic/grpc_transport.rs`)
   - Generic over `CommandExecutor` type
   - Manages node configurations (node_id → host:port mapping)
   - Supports dynamic node addition/removal
   - Implements `ClusterTransport` trait

2. **Protocol Buffers Definition** (`proto/raft.proto`)
   - Defines wire format for Raft messages
   - Includes workflow-specific command types:
     - `WorkflowStartData`
     - `WorkflowEndData`
     - `CheckpointData`
     - `OwnerChangeData`
   - Generic `RaftMessage` wrapper for Raft protocol messages

3. **gRPC Server** (`src/grpc/server.rs`)
   - Workflow-specific implementation using `WorkflowCommandExecutor`
   - Converts between protobuf and internal command types
   - Handles incoming Raft messages from peer nodes
   - Routes messages to local node's receiver channel

4. **gRPC Client** (`src/grpc/client.rs`)
   - Simple client for sending messages to peer nodes
   - Connects to remote node via gRPC
   - Sends `RaftMessage` with embedded commands

## Usage

### Creating a gRPC Transport

```rust
use raftoral::raft::generic::grpc_transport::{GrpcClusterTransport, NodeConfig};
use raftoral::workflow::WorkflowCommandExecutor;

// Define node configurations
let nodes = vec![
    NodeConfig { node_id: 1, address: "127.0.0.1:5001".to_string() },
    NodeConfig { node_id: 2, address: "127.0.0.1:5002".to_string() },
    NodeConfig { node_id: 3, address: "127.0.0.1:5003".to_string() },
];

// Create transport
let transport = Arc::new(
    GrpcClusterTransport::<WorkflowCommandExecutor>::new(nodes)
);
```

### Starting a gRPC Server

```rust
use raftoral::grpc::start_grpc_server;

// Start server for node 1
tokio::spawn(async move {
    start_grpc_server(
        "127.0.0.1:5001".to_string(),
        transport.clone(),
        1  // node_id
    ).await
});
```

### Creating Clusters

```rust
// Create cluster instances for each node
let cluster1 = transport.create_cluster(1).await?;
let cluster2 = transport.create_cluster(2).await?;
let cluster3 = transport.create_cluster(3).await?;

// Use clusters as normal
let runtime1 = WorkflowRuntime::new(cluster1);
```

## Dynamic Node Management

The transport supports adding and removing nodes at runtime:

```rust
// Add a new node
let new_node = NodeConfig {
    node_id: 4,
    address: "127.0.0.1:5004".to_string()
};
transport.add_node(new_node).await?;

// Remove a node
transport.remove_node(4).await?;

// Get node address
let addr = transport.get_node_address(1).await;

// Get all node IDs
let node_ids = transport.node_ids().await;
```

## Protocol Details

### Message Flow

1. **Propose Command**:
   ```
   Node A (Leader) → gRPC Client → Node B Server → Node B Receiver Channel → Raft Processing
   ```

2. **Raft Protocol Message**:
   ```
   Node A → Serialize raft::Message → gRPC → Node B → Deserialize → Process
   ```

### Wire Format

The `RaftMessage` protobuf includes:
- `raft_message`: Serialized Raft protocol message (using protobuf 2.x format from raft-rs)
- `command`: Optional workflow command (for proposals)

### Command Conversion

The server automatically converts between protobuf and internal types:

**Protobuf → Internal**:
```rust
ProtoWorkflowStartData → WorkflowStartData
ProtoWorkflowEndData → WorkflowEndData
ProtoCheckpointData → CheckpointData
ProtoOwnerChangeData → OwnerChangeData
```

**Internal → Protobuf**:
```rust
WorkflowCommand → ProtoWorkflowCommand (via convert_workflow_command_to_proto)
```

## Testing

### Unit Tests

Two comprehensive tests verify the implementation:

1. **test_grpc_message_passing**:
   - Finds two free TCP ports
   - Starts two gRPC servers
   - Sends a WorkflowStart command via gRPC
   - Verifies successful transmission

2. **test_grpc_transport_node_management**:
   - Tests dynamic node addition
   - Tests node removal
   - Verifies node address lookup

### Running Tests

```bash
cargo test test_grpc --test grpc_test
```

## Implementation Notes

### Generic vs Specific

- **Generic Layer** (`GrpcClusterTransport`): Reusable for any `CommandExecutor`
- **Specific Layer** (`src/grpc/`): Workflow-specific implementation with concrete protobuf types

This design allows the Raft infrastructure to remain generic while supporting typed protocol definitions.

### Channel Architecture

The transport maintains:
- **node_senders**: Map of node_id → sender to that node's receiver
- **node_receivers**: Map of node_id → receiver (consumed when cluster is created)

When a gRPC message arrives:
1. Server receives message
2. Converts proto → internal types
3. Sends to node's receiver channel via sender
4. Cluster processes message from receiver

### Future Enhancements

Potential improvements:
1. **TLS/mTLS**: Secure communication between nodes
2. **Connection pooling**: Reuse gRPC connections
3. **Retry logic**: Handle transient network failures
4. **Compression**: Reduce message size for large snapshots
5. **Metrics**: Track message latency, throughput, errors
6. **Load balancing**: Distribute snapshot transfers

## Integration with Existing Code

The gRPC transport is a drop-in replacement for `InMemoryClusterTransport`:

**Before (In-Memory)**:
```rust
let transport = InMemoryClusterTransport::<WorkflowCommandExecutor>::new(vec![1, 2, 3]);
transport.start().await?;
let cluster1 = transport.create_cluster(1).await?;
```

**After (gRPC)**:
```rust
let nodes = vec![
    NodeConfig { node_id: 1, address: "192.168.1.10:5001".to_string() },
    NodeConfig { node_id: 2, address: "192.168.1.11:5001".to_string() },
    NodeConfig { node_id: 3, address: "192.168.1.12:5001".to_string() },
];
let transport = Arc::new(GrpcClusterTransport::<WorkflowCommandExecutor>::new(nodes));

// Start servers (one per node, in separate processes)
start_grpc_server(nodes[0].address.clone(), transport.clone(), 1).await?;

// Create cluster
let cluster1 = transport.create_cluster(1).await?;
```

## Dependencies

Added to `Cargo.toml`:
```toml
[dependencies]
tonic = "0.12"
prost = "0.13"

[build-dependencies]
tonic-build = "0.12"
```

## Files Created/Modified

**New Files**:
- `proto/raft.proto` - Protocol buffer definitions
- `build.rs` - Protobuf code generation
- `src/raft/generic/grpc_transport.rs` - Generic gRPC transport
- `src/grpc/mod.rs` - gRPC module
- `src/grpc/server.rs` - gRPC server implementation
- `src/grpc/client.rs` - gRPC client implementation
- `tests/grpc_test.rs` - gRPC integration tests

**Modified Files**:
- `Cargo.toml` - Added tonic/prost dependencies
- `src/raft/generic/mod.rs` - Export GrpcClusterTransport
- `src/lib.rs` - Export grpc module
