# Raftoral as Kubernetes Sidecar: Architecture & Implementation Plan

## Overview

This document describes the architectural enhancement to run Raftoral as a **sidecar container** in Kubernetes deployments, enabling polyglot application development while maintaining Raft-based fault tolerance.

### Current Architecture (Embedded)

```
┌─────────────────────────────────────────┐
│         Application Process             │
│  ┌───────────────────────────────────┐  │
│  │      Application Code             │  │
│  │   (Workflow Implementations)      │  │
│  └────────────┬──────────────────────┘  │
│               │ Function Calls           │
│  ┌────────────▼──────────────────────┐  │
│  │       Raftoral Library            │  │
│  │  • Raft Consensus                 │  │
│  │  • Workflow Runtime               │  │
│  │  • Checkpoint Coordination        │  │
│  └───────────────────────────────────┘  │
└─────────────────────────────────────────┘
```

**Limitations:**
- Application must be written in Rust
- Tight coupling between app and Raftoral
- Workflow closures registered in-process
- Cannot use existing applications without rewrite

### Target Architecture (Sidecar)

```
┌─────────────────────────────────────────────────────────────┐
│                    Kubernetes Pod                           │
│                                                             │
│  ┌──────────────────────┐    ┌──────────────────────────┐  │
│  │  App Container       │    │  Raftoral Sidecar        │  │
│  │                      │    │                          │  │
│  │  ┌────────────────┐ │    │ ┌──────────────────────┐ │  │
│  │  │   Your App     │ │    │ │  Raft Consensus      │ │  │
│  │  │  (Any Lang)    │ │    │ │  Management Layer    │ │  │
│  │  │                │ │    │ │  Execution Layer     │ │  │
│  │  │  Workflow Code │◄├────┼─┤  Workflow Runtime    │ │  │
│  │  └────────┬───────┘ │    │ └──────────┬───────────┘ │  │
│  │           │         │    │            │             │  │
│  │  ┌────────▼───────┐ │    │ ┌──────────▼───────────┐ │  │
│  │  │ Raftoral Client│ │    │ │ Streaming gRPC Server│ │  │
│  │  │   SDK (Rust)   │◄├────┼─┤   (localhost:9001)   │ │  │
│  │  └────────────────┘ │    │ └──────────────────────┘ │  │
│  └──────────────────────┘    │            │             │  │
│           localhost           │ ┌──────────▼───────────┐ │  │
│                               │ │  Inter-Node gRPC     │ │  │
│                               │ │  (Service IP)        │ │  │
│                               │ └──────────────────────┘ │  │
│                               └──────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
                                        │
                          ┌─────────────┼─────────────┐
                          ▼             ▼             ▼
                    Other Pods    Other Pods    Other Pods
                   (via Service) (via Service) (via Service)
```

**Benefits:**
- ✅ Polyglot support (any language can use Raftoral SDK)
- ✅ Separation of concerns (app logic vs. consensus)
- ✅ Existing applications can add fault-tolerant workflows
- ✅ Independent scaling (app vs. Raftoral configuration)
- ✅ Kubernetes-native deployment model
- ✅ Zero external dependencies (no separate orchestrator pods)

---

## Architecture Components

### 1. Raftoral Sidecar Container

The sidecar runs as a separate container in the pod and manages:

**Responsibilities:**
- **Raft Consensus**: Participates in management and execution clusters
- **Peer Discovery**: Uses Kubernetes API to find other Raftoral instances
- **Workflow Orchestration**: Coordinates workflow execution across the cluster
- **Checkpoint Coordination**: Proposes checkpoints to Raft, notifies app
- **Local API**: Provides streaming gRPC server on `localhost:9001`
- **Inter-Node Communication**: gRPC/HTTP server on pod IP for Raft messages

**Endpoints:**
- `localhost:9001` - Streaming gRPC for app communication (NEW)
- `0.0.0.0:7001` - gRPC/HTTP for inter-node Raft messages (existing)

### 2. Application Container

The main application container that implements business logic:

**Responsibilities:**
- **Workflow Implementation**: Contains the actual workflow code
- **Raftoral Client**: Uses SDK to communicate with sidecar
- **Checkpoint Handling**: Responds to checkpoint callbacks from sidecar
- **Business Logic**: Domain-specific functionality

**Dependencies:**
- Raftoral Client SDK (Rust initially, other languages later)
- Connects to `localhost:9001` on startup

### 3. Communication Protocol: Streaming gRPC

Bidirectional streaming enables both parties to send messages at any time.

**Protocol Definition:**

```protobuf
service RaftoralSidecar {
  // Bidirectional stream for workflow coordination
  rpc WorkflowStream(stream AppMessage) returns (stream SidecarMessage);
}

// Messages from App → Sidecar
message AppMessage {
  oneof message {
    RegisterWorkflowRequest register_workflow = 1;
    CheckpointProposal checkpoint_proposal = 2;
    WorkflowResult workflow_result = 3;
    HeartbeatRequest heartbeat = 4;
  }
}

// Messages from Sidecar → App
message SidecarMessage {
  oneof message {
    ExecuteWorkflowRequest execute_workflow = 1;
    CheckpointEvent checkpoint_event = 2;
    WorkflowCancellation workflow_cancellation = 3;
    HeartbeatResponse heartbeat_response = 4;
  }
}

// Register a workflow implementation
message RegisterWorkflowRequest {
  string workflow_name = 1;
  uint32 version = 2;
}

// App proposes checkpoint value (owner node only)
message CheckpointProposal {
  string workflow_id = 1;
  string checkpoint_name = 2;
  bytes value = 3;  // Serialized checkpoint value
}

// Workflow execution result
message WorkflowResult {
  string workflow_id = 1;
  bool success = 2;
  bytes result = 3;  // Serialized result
  string error = 4;
}

// Sidecar tells app to start workflow execution
message ExecuteWorkflowRequest {
  string workflow_id = 1;
  string workflow_name = 2;
  uint32 version = 3;
  bytes input = 4;  // Serialized input
  bool is_owner = 5;  // True if this node is the workflow owner
}

// Sidecar notifies app of checkpoint (from consensus)
message CheckpointEvent {
  string workflow_id = 1;
  string checkpoint_name = 2;
  bytes value = 3;  // Serialized checkpoint value from Raft
}

// Sidecar requests workflow cancellation
message WorkflowCancellation {
  string workflow_id = 1;
  string reason = 2;
}

// Heartbeat to keep connection alive
message HeartbeatRequest {
  uint64 timestamp = 1;
}

message HeartbeatResponse {
  uint64 timestamp = 1;
}
```

---

## Workflow Execution Model

### Execution Flow

```
┌─────────────┐     ┌──────────────────────────────┐     ┌──────────────────────────────┐
│   Client    │     │      Leader Pod              │     │      Follower Pod            │
│             │     │                              │     │                              │
└──────┬──────┘     │  ┌────────────────────────┐  │     │  ┌────────────────────────┐  │
       │            │  │   Leader Sidecar       │  │     │  │  Follower Sidecar      │  │
       │            │  └──────┬─────────────────┘  │     │  └──────┬─────────────────┘  │
       │            │         │                    │     │         │                    │
       │ StartWorkflow        │                    │     │         │                    │
       ├──────────────────────►                    │     │         │                    │
       │            │         │                    │     │         │                    │
       │            │  ┌──────▼──────┐             │     │  ┌──────▼──────┐             │
       │            │  │ Propose via │─────────────┼─────┼─►│ Receive via │             │
       │            │  │    Raft     │             │     │  │    Raft     │             │
       │            │  └──────┬──────┘             │     │  └──────┬──────┘             │
       │            │         │                    │     │         │                    │
       │            │  ┌──────▼──────────────────┐ │     │  ┌──────▼──────────────────┐ │
       │            │  │ ExecuteWorkflowRequest │ │     │  │ ExecuteWorkflowRequest │ │
       │            │  │   (owner=true)         │ │     │  │   (owner=false)        │ │
       │            │  └──────┬─────────────────┘ │     │  └──────┬─────────────────┘ │
       │            │         │ gRPC stream       │     │         │ gRPC stream       │
       │            │  ┌──────▼─────────────────┐ │     │  ┌──────▼─────────────────┐ │
       │            │  │   App Container        │ │     │  │   App Container        │ │
       │            │  │                        │ │     │  │                        │ │
       │            │  │   workflow_fn()        │ │     │  │   workflow_fn()        │ │
       │            │  │        │               │ │     │  │        │               │ │
       │            │  │   checkpoint!()        │ │     │  │   checkpoint!()        │ │
       │            │  │        │ (blocks)      │ │     │  │        │ (blocks)      │ │
       │            │  └────────┼───────────────┘ │     │  └────────┼───────────────┘ │
       │            │           │ gRPC            │     │           │ gRPC            │
       │            │  ┌────────▼──────────────┐  │     │  ┌────────▼──────────────┐  │
       │            │  │ CheckpointProposal    │  │     │  │  (Waiting for        │  │
       │            │  │   (owner sends value) │  │     │  │   CheckpointEvent)   │  │
       │            │  └────────┬──────────────┘  │     │  └────────┬──────────────┘  │
       │            │           │                 │     │           │                 │
       │            │  ┌────────▼──────┐          │     │           │                 │
       │            │  │ Propose via   │──────────┼─────┼───────────┤                 │
       │            │  │    Raft       │          │     │  ┌────────▼──────┐          │
       │            │  └────────┬──────┘          │     │  │ Receive via   │          │
       │            │           │                 │     │  │    Raft       │          │
       │            │  ┌────────▼──────────────┐  │     │  └────────┬──────┘          │
       │            │  │  CheckpointEvent      │  │     │  ┌────────▼──────────────┐  │
       │            │  │  (unblocks app)       │  │     │  │  CheckpointEvent      │  │
       │            │  └────────┬──────────────┘  │     │  │  (unblocks app)       │  │
       │            │           │ gRPC            │     │  └────────┬──────────────┘  │
       │            │  ┌────────▼───────────────┐ │     │  ┌────────▼───────────────┐ │
       │            │  │   App Container        │ │     │  │   App Container        │ │
       │            │  │   (continues)          │ │     │  │   (continues)          │ │
       │            │  │        │               │ │     │  │        │               │ │
       │            │  │   WorkflowResult       │ │     │  │   WorkflowResult       │ │
       │            │  └────────┬───────────────┘ │     │  └────────┬───────────────┘ │
       │            │           │ gRPC            │     │           │ gRPC            │
       │            │  ┌────────▼──────────────┐  │     │  ┌────────▼──────────────┐  │
       │            │  │   Sidecar receives    │  │     │  │   Sidecar receives    │  │
       │◄───────────┼──┤   result              │  │     │  │   result              │  │
       │            │  └───────────────────────┘  │     │  └───────────────────────┘  │
       │            └──────────────────────────────┘     └──────────────────────────────┘
```

### Code Example: App-Side Workflow

```rust
use raftoral_client::{RaftoralClient, WorkflowContext};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Connect to sidecar
    let client = RaftoralClient::connect("http://localhost:9001").await?;

    // Register workflow
    client.register_workflow("ping_pong", 1).await?;

    // Start workflow handler
    client.handle_workflows(|req| async move {
        match (req.workflow_name.as_str(), req.version) {
            ("ping_pong", 1) => ping_pong_workflow(req).await,
            _ => Err("Unknown workflow".into()),
        }
    }).await?;

    Ok(())
}

async fn ping_pong_workflow(
    req: ExecuteWorkflowRequest
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let ctx = req.context;
    let input: String = serde_json::from_slice(&req.input)?;

    // Checkpoint - blocks until sidecar confirms via Raft
    let output = ctx.checkpoint("result", || {
        if input == "ping" { "pong" } else { "ping" }
    }).await?;

    Ok(serde_json::to_vec(&output)?)
}
```

---

## Kubernetes Integration

### Deployment with Dynamic Peer Discovery

Use standard Kubernetes Deployment for stateless applications:

```yaml
apiVersion: v1
kind: Service
metadata:
  name: raftoral
  labels:
    app: raftoral
spec:
  clusterIP: None  # Headless service for DNS-based discovery
  selector:
    app: raftoral
  ports:
  - name: raft
    port: 7001
    targetPort: 7001
  - name: app
    port: 8080
    targetPort: 8080

---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: raftoral
spec:
  replicas: 3
  selector:
    matchLabels:
      app: raftoral
  template:
    metadata:
      labels:
        app: raftoral
    spec:
      serviceAccountName: raftoral
      containers:
      # Main application container
      - name: app
        image: my-app:latest
        env:
        - name: RAFTORAL_SIDECAR_URL
          value: "http://localhost:9001"
        ports:
        - containerPort: 8080
          name: http

      # Raftoral sidecar container
      - name: raftoral
        image: raftoral:latest
        env:
        - name: POD_NAME
          valueFrom:
            fieldRef:
              fieldPath: metadata.name
        - name: POD_IP
          valueFrom:
            fieldRef:
              fieldPath: status.podIP
        - name: POD_NAMESPACE
          valueFrom:
            fieldRef:
              fieldPath: metadata.namespace
        - name: RAFTORAL_LISTEN
          value: "0.0.0.0:7001"
        - name: RAFTORAL_SIDECAR_LISTEN
          value: "127.0.0.1:9001"
        - name: RAFTORAL_SERVICE_NAME
          value: "raftoral"
        - name: RAFTORAL_STORAGE_PATH
          value: "/data"
        ports:
        - containerPort: 7001
          name: raft
        - containerPort: 9001
          name: sidecar
        volumeMounts:
        - name: data
          mountPath: /data

      volumes:
      - name: data
        emptyDir: {}  # For stateless apps; use PVC for persistence

---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: raftoral

---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: raftoral
rules:
- apiGroups: [""]
  resources: ["endpoints", "pods"]
  verbs: ["get", "list", "watch"]

---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: raftoral
subjects:
- kind: ServiceAccount
  name: raftoral
roleRef:
  kind: Role
  name: raftoral
  apiGroup: rbac.authorization.k8s.io
```

### Peer Discovery via Kubernetes API

The sidecar discovers peers dynamically using Kubernetes endpoints:

**Discovery Logic:**
1. **Query Kubernetes API** for endpoints of the raftoral Service:
   ```
   GET /api/v1/namespaces/{namespace}/endpoints/raftoral
   ```

2. **Parse Endpoint Addresses**:
   ```
   Endpoints:
   - 10.244.1.5:7001 (pod: myapp-7f6c9d8b-xk2jl)
   - 10.244.2.3:7001 (pod: myapp-7f6c9d8b-m9p2x)
   - 10.244.1.7:7001 (pod: myapp-7f6c9d8b-q5r8t)
   ```

3. **Determine Bootstrap vs Join**:
   - If no endpoints exist yet → Bootstrap new cluster
   - If endpoints exist → Join existing cluster via discovery protocol

4. **Connect to Existing Peers**:
   - Query each peer's `/discovery/peers` endpoint
   - Get cluster information and auto-assign node ID
   - Join cluster via existing discovery mechanism

**Bootstrap Coordination:**
- First pod to start finds empty endpoints → bootstraps
- Subsequent pods find existing endpoints → join
- No fixed "bootstrap pod" needed!

**Pod Replacement Handling:**
- When pod is deleted, new pod starts with different IP
- New pod queries endpoints, finds cluster, and joins
- Old node's storage is ephemeral (emptyDir) or can use PVC for persistence

### Configuration via Environment Variables

```bash
# Pod identity (injected by Kubernetes)
POD_NAME=myapp-7f6c9d8b-xk2jl
POD_IP=10.244.1.5
POD_NAMESPACE=default

# Raftoral configuration
RAFTORAL_LISTEN=0.0.0.0:7001              # Inter-node Raft (on pod IP)
RAFTORAL_SIDECAR_LISTEN=127.0.0.1:9001   # App communication (localhost)
RAFTORAL_TRANSPORT=grpc                   # or http
RAFTORAL_STORAGE_PATH=/data               # Persistent storage

# Kubernetes discovery
RAFTORAL_SERVICE_NAME=raftoral            # Service name for endpoint discovery
```

---

## Implementation Plan

### Phase 1: Streaming gRPC Protocol (2-3 days)

**Goal:** Define and implement the sidecar communication protocol

**Tasks:**

1. **Define Protobuf Schema**
   - Create `proto/sidecar.proto` with message definitions
   - Generate Rust code via prost

2. **Implement Sidecar Server**
   - Create `src/sidecar/server.rs`
   - Implement bidirectional streaming RPC handler
   - Handle incoming AppMessages (register, checkpoint proposals)
   - Send SidecarMessages (execute workflow, checkpoint events)

3. **Implement Client SDK**
   - Create `raftoral-client` crate
   - Implement streaming gRPC client
   - Provide ergonomic async API for workflows

**Deliverable:**
- Working streaming gRPC connection between test app and sidecar
- Unit tests for message serialization/deserialization

**Success Criteria:**
- Sidecar can accept connections on `localhost:9001`
- Client can send RegisterWorkflow messages
- Bidirectional stream stays open with heartbeats

---

### Phase 2: Workflow Execution Bridge (3-4 days)

**Goal:** Bridge between Raft consensus and app-side workflow execution

**Tasks:**

1. **Create WorkflowProxy**
   - New component: `src/sidecar/workflow_proxy.rs`
   - Translates between Raft workflow commands and gRPC messages
   - Manages mapping of workflow_id → gRPC stream

2. **Modify WorkflowRuntime**
   - Instead of calling local closures, send ExecuteWorkflowRequest via gRPC
   - Await WorkflowResult from app via stream
   - Handle timeouts and app disconnections

3. **Checkpoint Coordination**
   - App sends CheckpointProposal → Sidecar proposes to Raft
   - Raft commits checkpoint → Sidecar sends CheckpointEvent to all apps
   - Owner app: immediate unblock
   - Follower apps: block until CheckpointEvent arrives

4. **Error Handling**
   - Workflow execution timeouts
   - App container crashes (sidecar retries on another node)
   - Stream disconnections (reconnect logic)

**Deliverable:**
- Modified WorkflowRuntime that communicates with app via gRPC
- Working checkpoint proposal/event flow

**Success Criteria:**
- Can start workflow via gRPC → app receives ExecuteWorkflowRequest
- App sends CheckpointProposal → goes through Raft
- All nodes receive CheckpointEvent
- Workflow completes with result returned via gRPC

---

### Phase 3: Client SDK Development (2-3 days)

**Goal:** Ergonomic Rust SDK for application developers

**Tasks:**

1. **RaftoralClient API**
   ```rust
   pub struct RaftoralClient {
       stream: BiStream<AppMessage, SidecarMessage>,
   }

   impl RaftoralClient {
       pub async fn connect(url: &str) -> Result<Self>;
       pub async fn register_workflow(&self, name: &str, version: u32);
       pub async fn handle_workflows<F>(&self, handler: F) -> Result<()>
           where F: Fn(ExecuteWorkflowRequest) -> BoxFuture<Result<Vec<u8>>>;
   }
   ```

2. **WorkflowContext API**
   ```rust
   pub struct WorkflowContext {
       workflow_id: String,
       is_owner: bool,
       client: Arc<RaftoralClient>,
   }

   impl WorkflowContext {
       pub async fn checkpoint<T>(&self, name: &str, compute: impl Fn() -> T) -> Result<T>;
       pub async fn checkpoint_compute<T, F>(&self, name: &str, compute: F) -> Result<T>
           where F: Future<Output = T>;
   }
   ```

3. **Checkpoint Macros**
   - Port existing `checkpoint!` and `checkpoint_compute!` to work over gRPC
   - Hide streaming complexity from app developer

4. **Connection Management**
   - Auto-reconnect on stream failure
   - Heartbeat mechanism to keep stream alive
   - Graceful shutdown

**Deliverable:**
- Published `raftoral-client` crate
- Documentation and examples

**Success Criteria:**
- App can register workflows with simple API
- Checkpoints work transparently via gRPC
- SDK handles reconnections automatically

---

### Phase 4: Kubernetes Integration (3-4 days)

**Goal:** Raftoral runs natively in Kubernetes with automatic peer discovery via Endpoints API

**Tasks:**

1. **Kubernetes Client Integration**
   - Add `kube-rs` dependency for Kubernetes API access
   - Create `src/k8s/client.rs` for K8s API wrapper
   - Authenticate using in-cluster ServiceAccount credentials

2. **Endpoint-Based Discovery**
   - Create `src/k8s/discovery.rs`
   - Query endpoints API: `GET /api/v1/namespaces/{ns}/endpoints/{service}`
   - Parse endpoint addresses (pod IPs + ports)
   - Watch for endpoint changes (pods added/removed)

3. **Startup Logic**
   - Query endpoints on startup
   - If no endpoints exist → `--bootstrap` (first pod)
   - If endpoints exist → Join via existing discovery protocol
   - No fixed "bootstrap pod" - race condition handled by Raft

4. **RBAC Configuration**
   - ServiceAccount with read access to endpoints/pods
   - Role binding for the raftoral namespace
   - Document required permissions

5. **Health Checks**
   - Kubernetes liveness probe: `/health` endpoint
   - Readiness probe: Check sidecar stream connected AND Raft cluster joined

6. **Docker Image**
   - Create `Dockerfile` for Raftoral sidecar
   - Multi-stage build for minimal image size
   - Configure entrypoint to use environment variables

**Deliverable:**
- Working Dockerfile for raftoral sidecar
- Kubernetes manifests (Deployment, Service, RBAC)
- Endpoint-based peer discovery working in K8s

**Success Criteria:**
- `kubectl apply -f k8s/` deploys 3-replica Deployment
- Pods discover each other dynamically via Endpoints API
- Cluster forms correctly with leader election
- Pod deletion/recreation handled gracefully

---

### Phase 5: Example Application (2 days)

**Goal:** Runnable example showing ping_pong workflow in Kubernetes

**Tasks:**

1. **Create Example App**
   - Simple Rust binary using `raftoral-client`
   - Implements ping_pong workflow
   - HTTP endpoint to trigger workflow: `POST /workflow`

2. **Kubernetes Manifests**
   - Deployment with app + raftoral sidecar containers
   - Headless Service for peer discovery
   - Regular Service for app HTTP endpoint
   - RBAC (ServiceAccount, Role, RoleBinding)

3. **Minikube Deployment**
   - Document steps to deploy to minikube
   - Include smoke test script
   - Verify workflow execution across nodes

4. **Documentation**
   - Update README with sidecar deployment instructions
   - Create `examples/kubernetes/README.md`
   - Add troubleshooting guide

**Deliverable:**
- `examples/kubernetes/ping-pong/` directory
- Working minikube deployment
- End-to-end smoke test

**Success Criteria:**
- `minikube start` → `kubectl apply -f examples/kubernetes/ping-pong/` succeeds
- 3 pods running (each with app + raftoral containers)
- `kubectl exec` into pod → `curl localhost:8080/workflow` triggers workflow
- Workflow executes correctly with checkpoints replicated

---

### Phase 6: Testing & Validation (2-3 days)

**Goal:** Ensure fault tolerance works in Kubernetes environment

**Tasks:**

1. **Failure Scenarios**
   - Kill leader pod → verify re-election
   - Kill follower pod → verify workflow continues
   - Network partition simulation
   - Rolling updates (one pod at a time)

2. **Integration Tests**
   - Multi-workflow concurrent execution
   - Checkpoint ordering verification
   - Workflow versioning across pods

3. **Performance Testing**
   - Measure checkpoint latency (sidecar overhead)
   - Workflow throughput benchmarks
   - Compare to embedded mode

4. **Documentation**
   - Create `docs/SIDECAR_PERFORMANCE.md`
   - Document trade-offs vs embedded mode

**Deliverable:**
- Test suite for Kubernetes deployment
- Performance benchmarks
- Fault tolerance validation report

**Success Criteria:**
- All failure scenarios handled gracefully
- Workflow execution survives pod failures
- Performance within 2x of embedded mode

---

## File Structure

```
raftoral/
├── proto/
│   ├── raft.proto          # Existing inter-node protocol
│   └── sidecar.proto       # NEW: App ↔ Sidecar protocol
│
├── src/
│   ├── sidecar/            # NEW: Sidecar-specific components
│   │   ├── mod.rs
│   │   ├── server.rs       # Streaming gRPC server
│   │   ├── workflow_proxy.rs  # Bridges Raft ↔ gRPC
│   │   └── stream_manager.rs # Manages app connections
│   │
│   ├── k8s/                # NEW: Kubernetes integration
│   │   ├── mod.rs
│   │   ├── client.rs       # Kubernetes API client wrapper
│   │   ├── config.rs       # Environment-based config
│   │   └── discovery.rs    # Endpoint-based peer discovery
│   │
│   └── ... (existing code)
│
├── raftoral-client/        # NEW: Client SDK crate
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── client.rs       # RaftoralClient implementation
│       ├── context.rs      # WorkflowContext
│       └── macros.rs       # checkpoint! macros
│
├── examples/
│   └── kubernetes/         # NEW: K8s examples
│       ├── ping-pong/
│       │   ├── app/        # Example application
│       │   │   ├── Dockerfile
│       │   │   └── main.rs
│       │   └── k8s/        # Kubernetes manifests
│       │       ├── deployment.yaml
│       │       ├── service.yaml
│       │       └── rbac.yaml
│       └── README.md
│
├── docs/
│   ├── RAFTORAL_AS_SIDECAR.md  # This document
│   └── SIDECAR_PERFORMANCE.md  # Performance analysis
│
└── Dockerfile              # NEW: Raftoral sidecar image
```

---

## Timeline & Milestones

### Milestone 1: Local Development (Week 1)
- **Phase 1** complete: Streaming gRPC protocol working
- **Phase 2** complete: Workflow execution via gRPC
- Can run app + sidecar locally (no Kubernetes)

**Demo:** Start sidecar → Start app → Trigger workflow → Observe checkpoints

---

### Milestone 2: Client SDK (Week 2)
- **Phase 3** complete: Ergonomic Rust SDK
- Example ping_pong app using SDK
- Local multi-node cluster (docker-compose)

**Demo:** 3 containers (app+sidecar each) running locally, workflow execution across nodes

---

### Milestone 3: Kubernetes Deployment (Week 3)
- **Phase 4** complete: K8s integration
- **Phase 5** complete: Example deployment
- Minikube cluster with 3 pods

**Demo:** `kubectl apply` → cluster forms → workflow execution in K8s

---

### Milestone 4: Production Ready (Week 4)
- **Phase 6** complete: Testing and validation
- Fault tolerance verified
- Performance benchmarks
- Documentation complete

**Demo:** Kill leader pod → observe failover → workflow continues on new leader

---

## Migration Path

For existing Raftoral users:

### Option 1: Keep Embedded Mode
- No changes required
- Continue using FullNode with closures
- Best for pure Rust applications

### Option 2: Migrate to Sidecar
1. Extract workflow closures into separate binary
2. Use `raftoral-client` SDK
3. Deploy as sidecar in Kubernetes
4. Benefit: Can now write apps in other languages

**Both modes supported!** Raftoral will support both embedded and sidecar deployments.

---

## Trade-offs

### Sidecar Benefits
✅ Language-agnostic (any language can use SDK)
✅ Separation of concerns (app vs consensus)
✅ Kubernetes-native deployment
✅ Easier to integrate with existing applications

### Sidecar Drawbacks
❌ Additional latency (gRPC overhead vs in-process calls)
❌ More complex deployment (two containers per pod)
❌ Higher memory usage (two processes vs one)
❌ Stream management complexity

### When to Use Sidecar
- Polyglot teams (non-Rust apps)
- Kubernetes deployments
- Microservices architecture
- Need to add workflows to existing apps

### When to Use Embedded
- Pure Rust applications
- Minimize latency (tight loops)
- Single-binary deployment preferred
- Maximum performance required

---

## Open Questions

1. **Workflow State Persistence**: If app container restarts, how does sidecar resume workflows?
   - **Answer**: Sidecar maintains workflow state in Raft. On restart, app reconnects and resumes.

2. **Multiple Apps per Pod**: Can one sidecar serve multiple app containers?
   - **Answer**: Yes, each app gets its own gRPC stream. WorkflowProxy multiplexes.

3. **Language SDKs**: What's the plan for Python/Go/TypeScript SDKs?
   - **Answer**: After Rust SDK stabilizes, generate from protobuf + implement helper libraries.

4. **Workflow Versioning**: How to handle rolling updates?
   - **Answer**: Same as embedded mode - register multiple versions, drain old version.

5. **Resource Limits**: How to prevent app from starving sidecar?
   - **Answer**: Kubernetes resource limits on both containers independently.

---

## Future Enhancements

Beyond initial implementation:

1. **Multi-Language SDKs**
   - Python: `raftoral-client-py`
   - Go: `raftoral-client-go`
   - TypeScript: `@raftoral/client`

2. **WebSocket Alternative**
   - For browser-based apps
   - Same protocol, different transport

3. **Workflow Debugging**
   - Sidecar exposes debug UI
   - View checkpoint history
   - Replay workflows

4. **Metrics & Observability**
   - Prometheus metrics from sidecar
   - OpenTelemetry tracing
   - Workflow execution dashboards

5. **Advanced Kubernetes Features**
   - Pod Disruption Budgets
   - Horizontal Pod Autoscaler integration
   - Cross-namespace deployments

---

## Summary

Running Raftoral as a Kubernetes sidecar transforms it from a Rust-only library into a **polyglot workflow orchestration platform** while maintaining its core benefits:

- ✅ **No external orchestrator** - Raft consensus embedded in sidecars
- ✅ **Fault-tolerant** - Workflows survive pod failures
- ✅ **Language-agnostic** - Apps in any language via SDK
- ✅ **Kubernetes-native** - Standard Deployment with dynamic peer discovery
- ✅ **Stateless-friendly** - Works with existing stateless apps
- ✅ **Type-safe** - Strong typing in client SDKs
- ✅ **Self-coordinating** - No separate control plane

The implementation plan spans **4 weeks** with clear milestones, culminating in a working Kubernetes Deployment running the ping_pong workflow across a 3-replica cluster.

**Next Step:** Begin Phase 1 - Define and implement streaming gRPC protocol.
