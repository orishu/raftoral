# Raftoral V2 Architecture

## Overview

This document describes the redesigned Raftoral architecture with a clean layered approach. The architecture is **reusable** for both Management and Execution clusters, with shared infrastructure (transport, node IDs) but different application-level semantics.

### Design Principles

1. **Unidirectional Flow**: Data flows downward (propose) and events flow upward
2. **No Callbacks**: Lower layers never call into upper layers directly
3. **Event-Driven**: State changes propagate via events, not direct calls
4. **Separation of Concerns**: Each layer has a single, well-defined responsibility
5. **Type-Parameterized**: Generic over application-specific command and event types
6. **Shared Infrastructure**: Transport and node ID space shared across all clusters

## Architecture Layers

**Note**: Layers are numbered from bottom (closest to network) to top (application logic)

```
┌──────────────────────────────────────────────────────────────┐
│ Layer 7: Application Runtime                                 │
│   - Management Runtime: Cluster membership operations        │
│   - Workflow Runtime: Workflow registration & execution      │
│   - Subscribes to application-specific events                │
│   - Proposes commands via Proposal Router                    │
└──────────────────────────────────────────────────────────────┘
                          ↓ propose(Command)
┌──────────────────────────────────────────────────────────────┐
│ Layer 6: Proposal Router                                     │
│   - Routes proposals to Raft (local leader vs remote leader) │
│   - Maintains sync waiters for proposal confirmation         │
│   - Subscribes to LeaderChanged events                       │
│   - NO direct state queries (uses events only)               │
└──────────────────────────────────────────────────────────────┘
                          ↓ local_propose / remote_propose
┌──────────────────────────────────────────────────────────────┐
│ Layer 5: Event Bus                                           │
│   - Broadcast channel for StateEvent<AppEvt>                │
│   - Decouples State Machine from upper layers               │
│   - Type-parameterized on application-specific events        │
└──────────────────────────────────────────────────────────────┘
                          ↑ emit events        ↓ subscribe
┌──────────────────────────────────────────────────────────────┐
│ Layer 4: State Machine                                       │
│   - Applies committed commands to authoritative state        │
│   - Emits StateEvent<AppEvt> for state changes              │
│   - NEVER calls upper layers or Raft                         │
│   - Provides snapshot() and restore() for Raft snapshots     │
└──────────────────────────────────────────────────────────────┘
                          ↑ apply(entry)
┌──────────────────────────────────────────────────────────────┐
│ Layer 3: Raft Node                                           │
│   - Wraps raft-rs RawNode                                    │
│   - Drives Raft state machine (tick, ready loop)            │
│   - Applies committed entries → State Machine                │
│   - Emits RaftEvent (RoleChanged, EntryCommitted)           │
│   - Sends/receives Raft messages via Transport               │
└──────────────────────────────────────────────────────────────┘
                          ↓ send              ↑ receive
┌──────────────────────────────────────────────────────────────┐
│ Layer 2: Cluster Router                                      │
│   - Routes incoming Raft messages by cluster_id             │
│   - Maintains map: cluster_id → RaftNode mailbox            │
│   - Called by Transport for peer messages                    │
└──────────────────────────────────────────────────────────────┘
                          ↓ outbound          ↑ inbound
┌──────────────────────────────────────────────────────────────┐
│ Layer 1: Transport Layer (Protocol-Agnostic)                 │
│   - Abstract interface for sending/receiving messages        │
│   - OutboundTransport trait for sending to peers             │
│   - Maintains peer registry (node_id → address)              │
│   - No knowledge of gRPC/HTTP/InProcess implementation       │
└──────────────────────────────────────────────────────────────┘
                          ↓ network send      ↑ network receive
┌──────────────────────────────────────────────────────────────┐
│ Layer 0: Server Layer (Protocol Implementation)              │
│   - gRPC: Production distributed deployment                  │
│   - HTTP: Alternative protocol with protobuf payloads        │
│   - InProcess: Unit testing without network overhead         │
│   - Implements actual network I/O                            │
└──────────────────────────────────────────────────────────────┘
```

## Layer Descriptions

### Layer 0: Server Layer (Protocol Implementation)

**Responsibility**: Actual network I/O and protocol handling

**Implementations**:

**1. gRPC Server** (Production):
```rust
pub struct GrpcServer {
    transport: Arc<dyn Transport>,
    server_handle: JoinHandle<()>,
}

impl GrpcServer {
    // Receives GenericMessage from peers, forwards to transport
    async fn handle_raft_message(&self, msg: GenericMessage) -> Result<(), Status> {
        self.transport.receive_message(msg).await
    }

    // Receives workflow/management requests from clients
    async fn handle_client_request(&self, req: WorkflowRequest) -> Result<Response, Status>;
}
```

**2. HTTP Server** (Alternative):
```rust
pub struct HttpServer {
    transport: Arc<dyn Transport>,
    // Similar to gRPC but uses HTTP endpoints with protobuf payloads
}
```

**3. InProcess Server** (Testing):
```rust
pub struct InProcessServer {
    transport: Arc<dyn Transport>,
    // In-memory message routing, no actual network I/O
    // Simulates network by routing messages between local instances
}
```

**Key Characteristic**: Server layer is **swappable** without changing upper layers

### Layer 1: Transport Layer (Protocol-Agnostic)

**Responsibility**: Abstract message sending/receiving interface

**Key Design**:
```rust
pub trait Transport: Send + Sync {
    // Send a message to a peer node
    async fn send_message(&self,
        target_node_id: u64,
        message: GenericMessage
    ) -> Result<(), TransportError>;

    // Receive a message from a peer (called by Server layer)
    async fn receive_message(&self, message: GenericMessage) -> Result<(), TransportError>;

    // Peer management
    async fn add_peer(&self, node_id: u64, address: String);
    async fn remove_peer(&self, node_id: u64);
    fn list_peers(&self) -> Vec<u64>;
}
```

**Implementation**:
```rust
pub struct TransportLayer {
    cluster_router: Arc<ClusterRouter>,

    // Peer registry: node_id → address (protocol-agnostic string)
    peers: Arc<Mutex<HashMap<u64, String>>>,

    // Server-specific sender (injected by server layer)
    // For gRPC: gRPC client pool
    // For HTTP: HTTP client pool
    // For InProcess: in-memory routing
    message_sender: Arc<dyn MessageSender>,
}

// Server-specific trait implemented by each protocol
pub trait MessageSender: Send + Sync {
    async fn send(&self, address: &str, message: GenericMessage) -> Result<(), TransportError>;
}
```

**Design Notes**:
- Transport is **shared** across all clusters
- Transport doesn't know about gRPC/HTTP/InProcess
- Server layer injects `MessageSender` implementation
- Only Management cluster's state machine updates peer registry via events

### Layer 2: Cluster Router

**Responsibility**: Route incoming Raft messages to the correct cluster

**Key Design**:
```rust
pub struct ClusterRouter {
    // Map: cluster_id → mpsc::Sender for that cluster's RaftNode
    routes: Arc<Mutex<HashMap<u32, mpsc::Sender<GenericMessage>>>>,
}

impl ClusterRouter {
    pub fn register_cluster(&self, cluster_id: u32, sender: mpsc::Sender<GenericMessage>);
    pub fn unregister_cluster(&self, cluster_id: u32);
    pub async fn route_message(&self, message: GenericMessage) -> Result<(), RoutingError>;
}
```

**Usage**:
- gRPC Server calls `cluster_router.route_message(msg)` for incoming messages
- ClusterRouter looks up `msg.cluster_id` and forwards to appropriate RaftNode

### Layer 3: Raft Node

**Responsibility**: Drive the Raft consensus protocol

**Key Design**:
```rust
pub struct RaftNode<Cmd, AppEvt>
where
    Cmd: Message + Clone,
    AppEvt: Clone + Send + 'static,
{
    node_id: u64,
    cluster_id: u32,
    raft_group: RawNode<MemStorage>,
    state_machine: Arc<dyn StateMachine<Cmd, AppEvt>>,
    transport: Arc<dyn Transport>,
    event_bus: broadcast::Sender<StateEvent<AppEvt>>,

    // Incoming messages from ClusterRouter
    message_rx: mpsc::Receiver<GenericMessage>,
}
```

**Responsibilities**:
1. Run the Raft RawNode loop (tick, ready, apply)
2. Receive messages from ClusterRouter via `message_rx`
3. Send messages to peers via `transport`
4. Call `state_machine.apply()` for committed entries
5. Emit `RaftEvent` (role changes, leader changes)

**Events Emitted**:
```rust
pub enum RaftEvent {
    LeaderChanged { new_leader_id: u64 },
    RoleChanged { new_role: StateRole },
    EntryCommitted { index: u64, sync_id: Option<u64> },
}
```

### Layer 4: State Machine

**Responsibility**: Apply committed commands and emit state change events

**Key Design**:
```rust
pub trait StateMachine<Cmd, AppEvt>: Send + Sync {
    // Apply a committed command to the state machine
    fn apply(&self, command: &Cmd) -> Result<(), Box<dyn std::error::Error>>;

    // Create a snapshot of current state
    fn snapshot(&self) -> Result<Vec<u8>, Box<dyn std::error::Error>>;

    // Restore state from snapshot
    fn restore(&self, snapshot: &[u8]) -> Result<(), Box<dyn std::error::Error>>;

    // Get the event bus sender for emitting application events
    fn event_bus(&self) -> &broadcast::Sender<StateEvent<AppEvt>>;
}
```

**Rules**:
- ✅ **DO**: Update internal state, emit events via event_bus
- ❌ **DON'T**: Call RaftNode, call ProposalRouter, call Runtime
- ❌ **DON'T**: Access Transport directly (emit events instead)

**Type Parameter**:
- `Cmd`: Command type (ManagementCommand or WorkflowCommand)
- `AppEvt`: Application event type (ManagementEvent or WorkflowEvent)

### Layer 5: Event Bus

**Responsibility**: Decouple event emission from consumption

**Key Design**:
```rust
pub enum StateEvent<AppEvt> {
    // Raft-level events
    Raft(RaftEvent),

    // Application-specific events
    App(AppEvt),
}

// Management cluster events
pub enum ManagementEvent {
    NodeAdded { node_id: u64, address: String },
    NodeRemoved { node_id: u64 },
    ExecutionClusterCreated { cluster_id: Uuid },
    WorkflowScheduled { workflow_id: Uuid, cluster_id: Uuid },
}

// Execution cluster events
pub enum WorkflowEvent {
    WorkflowStarted { workflow_id: Uuid },
    WorkflowEnded { workflow_id: Uuid, result: Vec<u8> },
    CheckpointCreated { workflow_id: Uuid, key: String, value: Vec<u8> },
}
```

**Usage**:
```rust
// CommandExecutor emits
event_bus.send(StateEvent::App(ManagementEvent::NodeAdded { ... }));

// Layers subscribe
let mut rx = event_bus.subscribe();
while let Ok(event) = rx.recv().await {
    match event {
        StateEvent::App(ManagementEvent::NodeAdded { node_id, address }) => {
            // Handle in this layer
        }
        _ => {}
    }
}
```

### Layer 5: Proposal Router

**Responsibility**: Route proposals to Raft leader

**Key Design**:
```rust
pub struct ProposalRouter<Cmd> {
    node_id: u64,
    cluster_id: u32,

    // Where to send local proposals (this node is leader)
    local_tx: mpsc::Sender<Cmd>,

    // Where to send remote proposals (forward to leader)
    transport: Arc<dyn OutboundTransport>,

    // Leader tracking (updated via events)
    leader_id: Arc<AtomicU64>,

    // Sync waiters: sync_id → oneshot::Sender<Result>
    sync_waiters: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<(), String>>>>>,
}

impl<Cmd> ProposalRouter<Cmd> {
    // Propose and wait for commitment
    pub async fn propose_and_wait(&self, command: Cmd) -> Result<(), String>;

    // Fire-and-forget proposal
    pub async fn propose(&self, command: Cmd) -> Result<(), String>;

    // Subscribe to events (updates leader_id, completes sync_waiters)
    pub fn subscribe_to_events(&self, event_rx: broadcast::Receiver<StateEvent<AppEvt>>);
}
```

**Leader Routing Logic**:
```rust
async fn route_proposal(&self, command: Cmd, sync_id: Option<u64>) -> Result<(), String> {
    let leader = self.leader_id.load(Ordering::SeqCst);

    if leader == self.node_id {
        // This node is leader - send locally
        self.local_tx.send(command).await?;
    } else if leader != 0 {
        // Forward to known leader
        let msg = command.to_protobuf(self.cluster_id, sync_id);
        self.transport.send_message(leader, msg).await?;
    } else {
        // Leader unknown - fail fast
        return Err("Leader unknown".to_string());
    }
    Ok(())
}
```

### Layer 6: Proposal Router

**Responsibility**: Route proposals to Raft leader

**Key Design**:
```rust
pub struct ProposalRouter<Cmd> {
    node_id: u64,
    cluster_id: u32,

    // Where to send local proposals (this node is leader)
    local_tx: mpsc::Sender<Cmd>,

    // Where to send remote proposals (forward to leader)
    transport: Arc<dyn Transport>,

    // Leader tracking (updated via events)
    leader_id: Arc<AtomicU64>,

    // Sync waiters: sync_id → oneshot::Sender<Result>
    sync_waiters: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<(), String>>>>>,
}

impl<Cmd> ProposalRouter<Cmd> {
    // Propose and wait for commitment
    pub async fn propose_and_wait(&self, command: Cmd) -> Result<(), String>;

    // Fire-and-forget proposal
    pub async fn propose(&self, command: Cmd) -> Result<(), String>;

    // Subscribe to events (updates leader_id, completes sync_waiters)
    pub fn subscribe_to_events(&self, event_rx: broadcast::Receiver<StateEvent<AppEvt>>);
}
```

**Leader Routing Logic**:
```rust
async fn route_proposal(&self, command: Cmd, sync_id: Option<u64>) -> Result<(), String> {
    let leader = self.leader_id.load(Ordering::SeqCst);

    if leader == self.node_id {
        // This node is leader - send locally
        self.local_tx.send(command).await?;
    } else if leader != 0 {
        // Forward to known leader
        let msg = command.to_protobuf(self.cluster_id, sync_id);
        self.transport.send_message(leader, msg).await?;
    } else {
        // Leader unknown - fail fast
        return Err("Leader unknown".to_string());
    }
    Ok(())
}
```

### Layer 7: Application Runtime

**Responsibility**: Application-specific logic (Management vs Workflow)

**Management Runtime**:
```rust
pub struct ManagementRuntime {
    proposal_router: Arc<ProposalRouter<ManagementCommand>>,
    event_rx: broadcast::Receiver<StateEvent<ManagementEvent>>,

    // Local read models (updated via events)
    cluster_info: Arc<Mutex<HashMap<Uuid, ExecutionClusterInfo>>>,
    node_memberships: Arc<Mutex<HashMap<u64, HashSet<Uuid>>>>,
}

impl ManagementRuntime {
    pub async fn add_node(&self, node_id: u64, address: String) -> Result<(), String> {
        self.proposal_router.propose_and_wait(
            ManagementCommand::AddNode { node_id, address }
        ).await
    }

    pub async fn create_execution_cluster(&self, nodes: Vec<u64>) -> Result<Uuid, String>;
    pub async fn schedule_workflow(&self, workflow_id: Uuid, cluster_id: Uuid) -> Result<(), String>;
}
```

**Workflow Runtime**:
```rust
pub struct WorkflowRuntime {
    proposal_router: Arc<ProposalRouter<WorkflowCommand>>,
    event_rx: broadcast::Receiver<StateEvent<WorkflowEvent>>,

    // Workflow registry
    workflows: Arc<Mutex<HashMap<String, Box<dyn WorkflowFunction>>>>,

    // Checkpoint cache (updated via events)
    checkpoints: Arc<Mutex<HashMap<(Uuid, String), Vec<u8>>>>,
}

impl WorkflowRuntime {
    pub async fn start_workflow<I, O>(&self, name: &str, input: I) -> Result<WorkflowHandle<O>, String>;
    pub async fn create_checkpoint(&self, workflow_id: Uuid, key: &str, value: Vec<u8>) -> Result<(), String>;
}
```

## Cluster Types

### Management Cluster (cluster_id = 0)

**Purpose**: Track cluster membership and workflow-to-cluster assignments

**Command Type**: `ManagementCommand`
```rust
pub enum ManagementCommand {
    AddNode { node_id: u64, address: String },
    RemoveNode { node_id: u64 },
    CreateExecutionCluster { cluster_id: Uuid, nodes: Vec<u64> },
    DestroyExecutionCluster { cluster_id: Uuid },
    AssociateNode { cluster_id: Uuid, node_id: u64 },
    DisassociateNode { cluster_id: Uuid, node_id: u64 },
    ScheduleWorkflow { workflow_id: Uuid, cluster_id: Uuid, workflow_type: String, input: Vec<u8> },
    ReportWorkflowEnded { workflow_id: Uuid, result: Vec<u8> },
}
```

**Event Type**: `ManagementEvent` (see Layer 4)

**State**:
```rust
pub struct ManagementState {
    // Node registry: node_id → address
    nodes: HashMap<u64, String>,

    // Execution clusters
    execution_clusters: HashMap<Uuid, ExecutionClusterInfo>,

    // Node memberships: node_id → set of execution cluster IDs
    node_memberships: HashMap<u64, HashSet<Uuid>>,

    // Workflow locations: workflow_id → cluster_id
    workflow_locations: HashMap<Uuid, Uuid>,

    // Completed workflow results (TTL cache)
    workflow_results: TtlCache<Uuid, Vec<u8>>,
}
```

**Special Responsibilities**:
- **ONLY** the Management cluster's state machine updates the Transport
- When `AddNode` is applied, emit `NodeAdded` event
- Transport subscribes to events and calls `add_peer()`

### Execution Cluster (cluster_id = 1+)

**Purpose**: Execute workflows with consensus-driven coordination

**Command Type**: `WorkflowCommand`
```rust
pub enum WorkflowCommand {
    WorkflowStart { workflow_id: Uuid, workflow_type: String, input: Vec<u8> },
    WorkflowEnd { workflow_id: Uuid },
    SetCheckpoint { workflow_id: Uuid, key: String, value: Vec<u8> },
    OwnerChange { workflow_id: Uuid, old_owner: u64, new_owner: u64 },
}
```

**Event Type**: `WorkflowEvent` (see Layer 4)

**State**:
```rust
pub struct WorkflowState {
    // Active workflows: workflow_id → execution state
    active_workflows: HashMap<Uuid, WorkflowExecutionState>,

    // Checkpoints: (workflow_id, key) → value
    checkpoints: HashMap<(Uuid, String), Vec<u8>>,

    // Checkpoint queues for late followers
    checkpoint_queues: HashMap<(Uuid, String), VecDeque<Vec<u8>>>,
}
```

## Dual-Cluster Architecture Visualization

This diagram illustrates how Management and Execution clusters coexist on the same node, sharing infrastructure layers while maintaining independent Raft consensus.

```
                    ┌─────────────────┐      ┌─────────────────┐
                    │  Management     │      │   Workflow      │
                    │  Runtime        │      │   Runtime       │
                    │  (Layer 7)      │      │  (Layer 7)      │
                    │                 │      │                 │
                    │ - add_node()    │      │ - start_wf()    │
                    │ - create_exec() │      │ - checkpoint()  │
                    └────────┬────────┘      └────────┬────────┘
                             │                        │
                  ┌──────────▼────────┐    ┌──────────▼────────┐
                  │ Proposal Router   │    │ Proposal Router   │
                  │   (Layer 6)       │    │   (Layer 6)       │
                  │                   │    │                   │
                  │ - cluster_id=0    │    │ - cluster_id=1    │
                  │ - leader routing  │    │ - leader routing  │
                  └──────────┬────────┘    └──────────┬────────┘
                             │                        │
                  ┌──────────▼────────┐    ┌──────────▼────────┐
                  │   Event Bus       │    │   Event Bus       │
                  │   (Layer 5)       │    │   (Layer 5)       │
                  │                   │    │                   │
                  │ StateEvent<       │    │ StateEvent<       │
                  │ ManagementEvent>  │    │ WorkflowEvent>    │
                  └──────────┬────────┘    └──────────┬────────┘
                             ↕                        ↕
                  ┌──────────▼────────┐    ┌──────────▼────────┐
                  │  State Machine    │    │  State Machine    │
                  │   (Layer 4)       │    │   (Layer 4)       │
                  │                   │    │                   │
                  │ Management        │    │ Workflow          │
                  │ CommandExecutor   │    │ CommandExecutor   │
                  │                   │    │                   │
                  │ - nodes map       │    │ - active wfs      │
                  │ - exec clusters   │    │ - checkpoints     │
                  └──────────┬────────┘    └──────────┬────────┘
                             ↑                        ↑
                  ┌──────────▼────────┐    ┌──────────▼────────┐
                  │   Raft Node       │    │   Raft Node       │
                  │   (Layer 3)       │    │   (Layer 3)       │
                  │                   │    │                   │
                  │ - cluster_id=0    │    │ - cluster_id=1    │
                  │ - RawNode loop    │    │ - RawNode loop    │
                  │ - apply commits   │    │ - apply commits   │
                  └──────────┬────────┘    └──────────┬────────┘
                             │                        │
                             │   Cluster-specific     │
━━━━━━━━━━━━━━━━━━━━━━━━━━━┷━━━━━━━━━━━━━━━━━━━━━━━━┷━━━━━━━━━━━━━
                             │   Shared infrastructure│
                             └────────┬───────────────┘
                                      │
                             ┌────────▼──────────┐
                             │  Cluster Router   │
                             │    (Layer 2)      │
                             │                   │
                             │  cluster_id → RX  │
                             │      0 → mgmt     │
                             │      1 → exec     │
                             └────────┬──────────┘
                                      │
                             ┌────────▼──────────┐
                             │  Transport Layer  │
                             │    (Layer 1)      │
                             │                   │
                             │  - peer registry  │
                             │  - send/receive   │
                             │  - protocol-free  │
                             └────────┬──────────┘
                                      │
                             ┌────────▼──────────┐
                             │  Server Layer     │
                             │    (Layer 0)      │
                             │                   │
                             │  gRPC / HTTP /    │
                             │    InProcess      │
                             └───────────────────┘
```

### Key Observations

**Cluster-Specific Layers (Layers 3-7)**:
- Each cluster has its own:
  - Raft consensus (separate logs, separate leaders)
  - State machine (different command/event types)
  - Event bus (different event subscriptions)
  - Proposal router (routes to its own leader)
  - Runtime (management vs workflow APIs)

**Shared Layers (Layers 0-2)**:
- **Server** (Layer 0): Single gRPC/HTTP/InProcess server handles all clusters
- **Transport** (Layer 1): Single peer registry, shared message sending
- **Cluster Router** (Layer 2): Routes incoming messages by cluster_id

**Data Flow Example** (Adding a Node):

```
1. External client → Server (Layer 0)
   ↓
2. Server → ManagementRuntime.add_node() (Layer 7, left tower)
   ↓
3. ManagementRuntime → ProposalRouter (Layer 6, left)
   ↓
4. ProposalRouter → Transport (Layer 1, shared)
   ↓
5. Transport → Cluster Router (Layer 2, shared)
   ↓
6. Cluster Router → Management RaftNode (Layer 3, left, cluster_id=0)
   ↓
7. RaftNode → Management State Machine (Layer 4, left)
   ↓
8. State Machine emits NodeAdded event → Event Bus (Layer 5, left)
   ↓
9. Transport subscribes to event, updates peer registry (Layer 1, shared)
```

**Event Cross-Tower Communication**:
- Management state machine can emit events that Workflow runtime subscribes to
- Example: `WorkflowScheduled` event from management → workflow runtime starts workflow
- Events flow through separate event buses but can be bridged via subscriptions

## Shared Infrastructure

### Global Node ID System

- **Single namespace**: Node IDs are globally unique across all clusters
- **Management cluster**: Tracks all nodes in the system
- **Execution clusters**: Contain subsets of nodes
- **Example**: Nodes 1,2,3 all exist; Management has {1,2,3}, Exec-A has {1,2}, Exec-B has {2,3}

### Shared Transport

- **Single OutboundTransport instance**: Shared by all clusters
- **Peer connections**: Maintained once per node, not per cluster
- **Management cluster owns updates**: Only Management cluster adds/removes peers
- **All clusters use for sending**: Both Management and Execution clusters call `transport.send_message()`

**Initialization Flow**:
```rust
// 1. Create shared transport
let transport = Arc::new(GrpcOutboundTransport::new());

// 2. Create Management cluster (cluster_id = 0)
let management_cluster = create_cluster(
    node_id,
    0, // cluster_id
    transport.clone(),
    cluster_router.clone(),
    ManagementStateMachine::new(event_bus_tx.clone()),
).await?;

// 3. Management cluster subscribes to own events and updates transport
let transport_clone = transport.clone();
tokio::spawn(async move {
    let mut rx = event_bus_rx;
    while let Ok(StateEvent::App(ManagementEvent::NodeAdded { node_id, address })) = rx.recv().await {
        transport_clone.add_peer(node_id, address).await;
    }
});

// 4. Later: Create Execution clusters using same transport
let exec_cluster = create_cluster(
    node_id,
    1, // cluster_id
    transport.clone(), // Same transport!
    cluster_router.clone(),
    WorkflowStateMachine::new(event_bus_tx.clone()),
).await?;
```

### Shared Cluster Router

- **Single ClusterRouter instance**: Routes all incoming Raft messages
- **Multiple registrations**: Management cluster (id=0) + Execution clusters (id=1+)
- **gRPC Server integration**: Server calls `cluster_router.route_message()` for all peer messages

## Message Flows

### Flow 1: Adding a Node to the System

```
1. External gRPC client → Server.add_node(node_id=2, address="192.168.1.2:5001")
   ↓
2. Server → ManagementRuntime.add_node(2, "192.168.1.2:5001")
   ↓
3. ManagementRuntime → ProposalRouter.propose_and_wait(AddNode{2, ...})
   ↓
4. ProposalRouter → RaftNode (via local_tx if leader, or transport if follower)
   ↓
5. RaftNode: Raft consensus (propose → commit → apply)
   ↓
6. RaftNode → ManagementStateMachine.apply(AddNode{2, ...})
   ↓
7. ManagementStateMachine:
   - Updates state.nodes.insert(2, "192.168.1.2:5001")
   - Emits event_bus.send(StateEvent::App(ManagementEvent::NodeAdded{2, ...}))
   ↓
8. Event propagates to subscribers:
   ├─→ Transport: transport.add_peer(2, "192.168.1.2:5001")
   ├─→ ManagementRuntime: Updates local read model
   └─→ Other subscribers...
```

### Flow 2: Starting a Workflow

```
1. Client → Server.run_workflow(workflow_name="fibonacci", input=...)
   ↓
2. Server → ManagementRuntime.schedule_workflow(...)
   ↓
3. ManagementRuntime: Select execution cluster (e.g., least loaded)
   ↓
4. ManagementRuntime → ProposalRouter.propose(ScheduleWorkflow{workflow_id, cluster_id=1, ...})
   ↓
5. Management Raft: Consensus
   ↓
6. ManagementStateMachine.apply(ScheduleWorkflow):
   - Updates workflow_locations
   - Emits WorkflowScheduled event
   ↓
7. WorkflowScheduled event → Execution cluster leader's Runtime
   ↓
8. Execution Runtime → ProposalRouter.propose(WorkflowStart{workflow_id, ...})
   ↓
9. Execution Raft: Consensus
   ↓
10. WorkflowStateMachine.apply(WorkflowStart):
    - Spawns workflow task on ALL nodes
    - Emits WorkflowStarted event
    ↓
11. First node to complete → WorkflowStateMachine.apply(WorkflowEnd)
    - Emits WorkflowEnded event with result
    ↓
12. WorkflowEnded event → ManagementRuntime
    ↓
13. ManagementRuntime → ProposalRouter.propose(ReportWorkflowEnded{result})
    ↓
14. Management cluster stores result in TTL cache
    ↓
15. Client polls WaitForCompletion → Server returns cached result
```

### Flow 3: Creating a Checkpoint

```
1. Workflow execution code: ctx.create_replicated_var("counter", 42)
   ↓
2. WorkflowRuntime → ProposalRouter.propose(SetCheckpoint{workflow_id, "counter", vec![42]})
   ↓
3. Execution Raft: Consensus
   ↓
4. WorkflowStateMachine.apply(SetCheckpoint):
   - Updates checkpoints map
   - OR queues if workflow not started yet
   - Emits CheckpointCreated event
   ↓
5. CheckpointCreated event → WorkflowRuntime
   ↓
6. WorkflowRuntime: Updates local checkpoint cache
   ↓
7. Workflow code: Returns ReplicatedVar handle with value
```

## Type Parameters and Generics

### Generic Cluster Creation

All clusters use the same generic infrastructure:

```rust
pub struct Cluster<Cmd, AppEvt>
where
    Cmd: Message + Clone + Serialize + DeserializeOwned,
    AppEvt: Clone + Send + 'static,
{
    pub node_id: u64,
    pub cluster_id: u32,
    pub raft_node: Arc<RaftNode<Cmd, AppEvt>>,
    pub proposal_router: Arc<ProposalRouter<Cmd>>,
    pub event_bus: broadcast::Sender<StateEvent<AppEvt>>,
}

pub async fn create_cluster<Cmd, AppEvt, SM>(
    node_id: u64,
    cluster_id: u32,
    transport: Arc<dyn Transport>,
    cluster_router: Arc<ClusterRouter>,
    state_machine: SM,
) -> Result<Cluster<Cmd, AppEvt>, Box<dyn std::error::Error>>
where
    Cmd: Message + Clone + Serialize + DeserializeOwned,
    AppEvt: Clone + Send + 'static,
    SM: StateMachine<Cmd, AppEvt> + 'static,
{
    // Create event bus
    let (event_tx, event_rx) = broadcast::channel(1000);

    // Create RaftNode with mailbox for incoming messages
    let (msg_tx, msg_rx) = mpsc::channel(100);
    let raft_node = Arc::new(RaftNode::new(
        node_id,
        cluster_id,
        transport.clone(),
        Arc::new(state_machine),
        event_tx.clone(),
        msg_rx,
    ).await?);

    // Register with ClusterRouter
    cluster_router.register_cluster(cluster_id, msg_tx);

    // Create ProposalRouter
    let (local_proposal_tx, local_proposal_rx) = mpsc::channel(100);
    let proposal_router = Arc::new(ProposalRouter::new(
        node_id,
        cluster_id,
        local_proposal_tx,
        transport.clone(),
    ));

    // Wire up proposal router to raft node
    let raft_node_clone = raft_node.clone();
    tokio::spawn(async move {
        // Forward local proposals to RaftNode
        // ...
    });

    // ProposalRouter subscribes to events
    proposal_router.subscribe_to_events(event_tx.subscribe());

    Ok(Cluster {
        node_id,
        cluster_id,
        raft_node,
        proposal_router,
        event_bus: event_tx,
    })
}
```

### Instantiation Examples

```rust
// Management cluster
type ManagementCluster = Cluster<ManagementCommand, ManagementEvent>;

let management = create_cluster(
    node_id,
    0,
    transport.clone(),
    cluster_router.clone(),
    ManagementStateMachine::new(),
).await?;

// Execution cluster
type ExecutionCluster = Cluster<WorkflowCommand, WorkflowEvent>;

let execution = create_cluster(
    node_id,
    1,
    transport.clone(),
    cluster_router.clone(),
    WorkflowStateMachine::new(),
).await?;
```

## Implementation Plan for `src/raft/generic2`

### Phase 1: Core Infrastructure (Week 1)

**Goal**: Basic types, traits, and transport

**Files**:
- `src/raft/generic2/mod.rs` - Module exports
- `src/raft/generic2/server/` - Server layer (Layer 0)
  - `grpc.rs` - GrpcServer implementation
  - `http.rs` - HttpServer implementation
  - `in_process.rs` - InProcessServer for testing
- `src/raft/generic2/transport.rs` - Transport trait + TransportLayer impl (Layer 1)
- `src/raft/generic2/cluster_router.rs` - ClusterRouter implementation (Layer 2)
- `src/raft/generic2/events.rs` - StateEvent<AppEvt>, RaftEvent enums (Layer 5)
- `src/raft/generic2/errors.rs` - Error types

**Tests**:
- Unit tests for ClusterRouter (register, route, unregister)
- Unit tests for Transport with InProcessServer
- Unit tests for event serialization/deserialization

### Phase 2: Raft Node & State Machine (Week 2)

**Goal**: RaftNode implementation with State Machine trait

**Files**:
- `src/raft/generic2/node.rs` - RaftNode<Cmd, AppEvt> (Layer 3)
- `src/raft/generic2/storage.rs` - MemStorage wrapper (if needed)
- `src/raft/generic2/state_machine.rs` - StateMachine trait (Layer 4)

**Tests**:
- Single-node RaftNode test (propose → apply)
- Event emission tests (RoleChanged, EntryCommitted)
- Snapshot creation/restoration tests
- State machine apply() tests with mock implementation

### Phase 3: Proposal Router (Week 3)

**Goal**: ProposalRouter with leader routing and sync waiters

**Files**:
- `src/raft/generic2/proposal_router.rs` - ProposalRouter<Cmd> (Layer 6)

**Tests**:
- Leader routing tests (local vs remote)
- Sync waiter completion tests
- Leader change event handling tests

### Phase 4: Cluster Creation (Week 4)

**Goal**: Generic cluster creation function

**Files**:
- `src/raft/generic2/cluster.rs` - Cluster<Cmd, AppEvt> struct + create_cluster()

**Tests**:
- Multi-node cluster bootstrap test (3 nodes) with InProcessServer
- Join existing cluster test
- Message routing across clusters test
- Event propagation across layers test

### Phase 5: Management Cluster (Week 5)

**Goal**: Management-specific state machine and runtime

**Files**:
- `src/raft/generic2/management/mod.rs`
- `src/raft/generic2/management/commands.rs` - ManagementCommand enum
- `src/raft/generic2/management/events.rs` - ManagementEvent enum
- `src/raft/generic2/management/state_machine.rs` - ManagementStateMachine (Layer 4)
- `src/raft/generic2/management/runtime.rs` - ManagementRuntime (Layer 7)
- `src/raft/generic2/management/state.rs` - ManagementState

**Tests**:
- AddNode/RemoveNode command tests
- CreateExecutionCluster command test
- NodeAdded event → transport update test
- Multi-node management cluster test (3 nodes, InProcessServer)

### Phase 6: Execution Cluster (Week 6)

**Goal**: Workflow-specific state machine and runtime

**Files**:
- `src/raft/generic2/execution/mod.rs`
- `src/raft/generic2/execution/commands.rs` - WorkflowCommand enum
- `src/raft/generic2/execution/events.rs` - WorkflowEvent enum
- `src/raft/generic2/execution/state_machine.rs` - WorkflowStateMachine (Layer 4)
- `src/raft/generic2/execution/runtime.rs` - WorkflowRuntime (Layer 7)
- `src/raft/generic2/execution/state.rs` - WorkflowState

**Tests**:
- WorkflowStart → parallel execution test
- SetCheckpoint → checkpoint queue test
- Multi-node workflow execution test (3 nodes)
- Checkpoint creation and consumption test
- Cross-tower event test (Management → Execution)

### Phase 7: Integration (Week 7)

**Goal**: End-to-end multi-cluster system

**Tests**:
- Bootstrap management cluster + execution cluster
- Add node to both clusters via management
- Run workflow on execution cluster
- Workflow completion reporting to management
- Multi-node integration test (3 nodes, both cluster types)

## Testing Strategy

### Unit Tests (Per-Component)

**Pattern**: Test each layer in isolation with mocks

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_cluster_router_routing() {
        let router = ClusterRouter::new();
        let (tx, mut rx) = mpsc::channel(10);

        router.register_cluster(1, tx);

        let msg = GenericMessage {
            cluster_id: 1,
            payload: vec![1, 2, 3],
        };

        router.route_message(msg.clone()).await.unwrap();

        let received = rx.recv().await.unwrap();
        assert_eq!(received.cluster_id, 1);
    }
}
```

### Integration Tests (Multi-Node)

**Pattern**: In-memory transport for fast multi-node tests

```rust
#[cfg(test)]
mod integration_tests {
    use super::*;

    #[tokio::test]
    async fn test_three_node_management_cluster() {
        // Create in-process server (no real network)
        let server = Arc::new(InProcessServer::new());

        // Create shared transport with in-process message sender
        let transport = Arc::new(TransportLayer::new(
            Arc::new(InProcessMessageSender::new(server.clone()))
        ));
        let router = Arc::new(ClusterRouter::new());

        // Bootstrap node 1
        let cluster1 = create_cluster(1, 0, transport.clone(), router.clone(),
            ManagementStateMachine::new()).await.unwrap();

        // Join nodes 2 and 3
        let cluster2 = join_cluster(2, 0, transport.clone(), router.clone(),
            ManagementStateMachine::new(), vec![1]).await.unwrap();
        let cluster3 = join_cluster(3, 0, transport.clone(), router.clone(),
            ManagementStateMachine::new(), vec![1]).await.unwrap();

        // Propose AddNode on node 1
        cluster1.proposal_router.propose_and_wait(
            ManagementCommand::AddNode { node_id: 4, address: "192.168.1.4:5001".into() }
        ).await.unwrap();

        // Verify all nodes see the change via events
        // ...
    }
}
```

### E2E Tests (gRPC)

**Pattern**: Full stack with real gRPC server

```rust
#[tokio::test]
async fn test_e2e_workflow_execution() {
    // Start 3-node cluster with management + execution
    let nodes = start_three_node_cluster().await;

    // Register workflow on all nodes
    for node in &nodes {
        node.execution_runtime.register_workflow("fib", fibonacci_workflow);
    }

    // Start workflow via gRPC
    let client = create_grpc_client(nodes[0].address()).await;
    let response = client.run_workflow(RunWorkflowRequest {
        workflow_name: "fib".into(),
        input: serde_json::to_vec(&FibInput { n: 10 }).unwrap(),
    }).await.unwrap();

    // Wait for completion
    let result = client.wait_for_completion(WaitRequest {
        workflow_id: response.workflow_id,
    }).await.unwrap();

    assert_eq!(result.output, serde_json::to_vec(&FibOutput { result: 55 }).unwrap());
}
```

## Migration Path from `generic` to `generic2`

1. **Keep both modules**: `src/raft/generic` (old) and `src/raft/generic2` (new)
2. **Build generic2 in parallel**: No disruption to existing code
3. **Update one test at a time**: Port tests from old to new
4. **Switch main.rs**: Update to use generic2
5. **Remove generic**: Once all tests pass, delete `src/raft/generic`
6. **Rename**: `generic2` → `generic`

## Summary of Key Improvements

### From V1 to V2

| Aspect | V1 (Current) | V2 (New) |
|--------|-------------|----------|
| **Layering** | Tangled dependencies | 8 clear layers (0-7) |
| **Callbacks** | State machine calls RaftCluster | Events only, no callbacks |
| **Transport** | Coupled to RaftCluster, gRPC-only | Protocol-agnostic (gRPC/HTTP/InProcess) |
| **State Management** | Direct queries | Event-driven read models |
| **Testing** | Broken multi-node tests | InProcessServer for fast unit tests |
| **Reusability** | Hardcoded cluster types | Generic over Cmd + AppEvt |
| **Event Handling** | Ad-hoc channels | Centralized Event Bus (Layer 5) |
| **Proposal Routing** | Mixed responsibilities | Dedicated ProposalRouter (Layer 6) |
| **Server Layer** | gRPC only | Swappable (gRPC/HTTP/InProcess) |
| **Terminology** | CommandExecutor | StateMachine (aligns with Raft standards) |

### Benefits

1. **✅ Testability**: Each layer can be tested in isolation with InProcessServer
2. **✅ Maintainability**: Clear boundaries, single responsibilities per layer
3. **✅ Extensibility**: Easy to add new cluster types or protocols (HTTP)
4. **✅ Performance**: Shared transport, no duplicate connections
5. **✅ Correctness**: Events guarantee eventual consistency across layers
6. **✅ Debuggability**: Event logs show complete system behavior
7. **✅ Type Safety**: Compile-time enforcement of command/event types
8. **✅ Protocol Flexibility**: Swap between gRPC, HTTP, or InProcess without code changes
9. **✅ Fast Unit Tests**: InProcessServer enables multi-node tests without network overhead
10. **✅ Standard Terminology**: "State Machine" aligns with Raft literature and other implementations
