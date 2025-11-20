//! Full Node - Complete stack from Layer 0 (gRPC) to Layer 7 (ManagementRuntime)
//!
//! This module provides a complete Raft node implementation that includes:
//! - Layer 0: gRPC Server
//! - Layer 1: Transport
//! - Layer 2: Cluster Router
//! - Layer 3-4: Raft Node with State Machine
//! - Layer 5: Event Bus
//! - Layer 6: Proposal Router
//! - Layer 7: Application Runtime (ManagementRuntime)

mod workflow_service;
pub use workflow_service::WorkflowManagementService;

use crate::grpc::{GrpcMessageSender, GrpcServer};
use crate::management::ManagementRuntime;
use crate::raft::generic::{ClusterRouter, RaftNode, RaftNodeConfig, Transport, TransportLayer};
use crate::workflow::WorkflowRuntime;
use slog::{Logger, info};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tonic::transport::Server;

/// A complete Raft node with all layers from gRPC to ManagementRuntime
///
/// The FullNode manages a management cluster (cluster_id=0) that tracks metadata
/// about workflow execution clusters. When sub-clusters are created via the management
/// runtime, FullNode automatically creates and manages WorkflowRuntime instances.
pub struct FullNode {
    /// Management runtime (Layer 7) - manages WorkflowRuntime sub-clusters
    runtime: Arc<ManagementRuntime<WorkflowRuntime>>,

    /// Raft node handle
    node: Arc<Mutex<RaftNode<crate::management::ManagementStateMachine>>>,

    /// Server handle (generic over transport type)
    server_handle: Option<JoinHandle<()>>,

    /// Shared workflow registry (accessible before execution clusters are created)
    workflow_registry: Arc<Mutex<crate::workflow::WorkflowRegistry>>,

    /// Node address
    address: String,

    /// Logger
    logger: Logger,
}

impl FullNode {
    /// Create and start a new full node in bootstrap mode (single-node cluster)
    ///
    /// # Arguments
    /// * `node_id` - Unique identifier for this node
    /// * `address` - Network address to bind to (e.g., "127.0.0.1:50051")
    /// * `storage_path` - Optional path for persistent storage (None = in-memory)
    /// * `logger` - Logger instance
    ///
    /// # Returns
    /// A running FullNode instance
    pub async fn new(
        node_id: u64,
        address: String,
        storage_path: Option<std::path::PathBuf>,
        logger: Logger,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        info!(logger, "Creating FullNode"; "node_id" => node_id, "address" => &address);

        // Layer 1: Create transport with gRPC message sender
        let grpc_sender = Arc::new(GrpcMessageSender::new());
        let transport = Arc::new(TransportLayer::new(grpc_sender));

        // Create mailbox for this node
        let (mailbox_tx, mailbox_rx) = mpsc::channel(1000);

        // Layer 2: Create cluster router
        let cluster_router = Arc::new(ClusterRouter::new());

        // Register this node's cluster (cluster_id = 0 for management)
        cluster_router.register_cluster(0, mailbox_tx).await;

        // Create shared workflow registry
        let registry = Arc::new(Mutex::new(crate::workflow::WorkflowRegistry::new()));

        // Layers 3-7: Create management runtime (includes RaftNode, EventBus, ProposalRouter)
        let config = RaftNodeConfig {
            node_id,
            cluster_id: 0,          // Management cluster ID
            snapshot_interval: 100, // Take snapshot every 100 entries
            storage_path,
            ..Default::default()
        };

        let (runtime, node) = ManagementRuntime::new(
            config,
            transport.clone(),
            mailbox_rx,
            cluster_router.clone(),
            registry.clone(),
            logger.clone(),
        )?;

        // Run Raft node in background
        let node_clone = node.clone();
        tokio::spawn(async move {
            info!(
                slog::Logger::root(slog::Discard, slog::o!()),
                "Starting Raft node event loop"
            );
            let _ = RaftNode::run_from_arc(node_clone).await;
        });

        // Campaign to become leader (for single-node cluster)
        node.lock().await.campaign().await?;

        // Wait for leader election to complete
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        // Bootstrap node should add itself to trigger ClusterManager
        // This creates the first execution cluster with this node
        info!(
            logger,
            "Bootstrap node adding itself to management cluster to trigger ClusterManager"
        );
        let add_rx = runtime.add_node(node_id, address.clone()).await?;

        // Wait for the operation to complete
        match add_rx.await {
            Ok(Ok(_)) => info!(
                logger,
                "Bootstrap node successfully added to management cluster"
            ),
            Ok(Err(e)) => return Err(format!("Failed to add bootstrap node: {}", e).into()),
            Err(e) => return Err(format!("Add node oneshot error: {}", e).into()),
        }

        // Layer 0: Start gRPC server
        let grpc_server = Arc::new(GrpcServer::new(
            cluster_router,
            node_id,
            address.clone(),
            transport.clone(),
            runtime.clone(),
        ));

        // Create WorkflowManagement service
        let workflow_service = WorkflowManagementService::new(runtime.clone(), logger.clone());

        let addr = address.parse()?;

        info!(logger, "Starting gRPC server"; "address" => &address);

        let grpc_server_clone = grpc_server.clone();
        let server_handle = tokio::spawn(async move {
            // Enable gRPC reflection for grpcurl support
            let reflection_service = tonic_reflection::server::Builder::configure()
                .register_encoded_file_descriptor_set(crate::grpc::proto::FILE_DESCRIPTOR_SET)
                .build_v1()
                .unwrap();

            let _ = Server::builder()
                .add_service(reflection_service)
                .add_service(
                    crate::grpc::proto::raft_service_server::RaftServiceServer::new(
                        (*grpc_server_clone).clone(),
                    ),
                )
                .add_service(
                    crate::grpc::proto::workflow_management_server::WorkflowManagementServer::new(
                        workflow_service,
                    ),
                )
                .serve(addr)
                .await;
        });

        info!(logger, "FullNode started"; "node_id" => node_id, "address" => &address);

        Ok(Self {
            runtime,
            node,
            server_handle: Some(server_handle),
            workflow_registry: registry,
            address,
            logger,
        })
    }

    /// Create and start a new full node in join mode (joining existing cluster)
    ///
    /// # Arguments
    /// * `address` - Network address to bind to (e.g., "127.0.0.1:50051")
    /// * `seed_addresses` - Addresses of existing cluster nodes to discover
    /// * `storage_path` - Optional path for persistent storage (None = in-memory)
    /// * `logger` - Logger instance
    ///
    /// # Returns
    /// A running FullNode instance that has joined the cluster
    pub async fn new_joining(
        address: String,
        seed_addresses: Vec<String>,
        storage_path: Option<std::path::PathBuf>,
        logger: Logger,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        info!(logger, "Creating FullNode in join mode";
            "address" => &address,
            "seeds" => ?seed_addresses
        );

        // Discover existing peers
        use crate::grpc::bootstrap::{discover_peers, next_node_id};
        let discovered_peers = discover_peers(seed_addresses).await;

        if discovered_peers.is_empty() {
            return Err("Failed to discover any peers".into());
        }

        // Determine our node ID
        let node_id = next_node_id(&discovered_peers);
        info!(logger, "Assigned node ID"; "node_id" => node_id);

        // Find management leader
        let leader_peer = discovered_peers
            .iter()
            .find(|p| p.management_leader_node_id != 0)
            .ok_or("No management leader found in discovered peers")?;
        info!(logger, "Found management leader";
            "leader_node_id" => leader_peer.management_leader_node_id,
            "leader_address" => &leader_peer.management_leader_address
        );
        // Layer 1: Create transport with gRPC message sender
        let grpc_sender = Arc::new(GrpcMessageSender::new());
        let transport = Arc::new(TransportLayer::new(grpc_sender));
        // Add all discovered peers to transport
        for peer in &discovered_peers {
            transport.add_peer(peer.node_id, peer.address.clone()).await;
        }
        // Create mailbox for this node
        let (mailbox_tx, mailbox_rx) = mpsc::channel(1000);
        // Layer 2: Create cluster router
        let cluster_router = Arc::new(ClusterRouter::new());
        // Register this node's cluster (cluster_id = 0 for management)
        cluster_router.register_cluster(0, mailbox_tx).await;
        // Determine if we should join as voter or learner
        // All discovered peers should have consistent voter information
        let should_join_as_voter = discovered_peers
            .first()
            .map(|p| p.should_join_as_voter)
            .unwrap_or(true); // Default to voter if no info available
        // Create shared workflow registry
        let registry = Arc::new(Mutex::new(crate::workflow::WorkflowRegistry::new()));

        // Layers 3-7: Create management runtime in joining mode
        let config = RaftNodeConfig {
            node_id,
            cluster_id: 0, // Management cluster ID
            snapshot_interval: 100,
            storage_path,
            ..Default::default()
        };

        let (runtime, node) = if should_join_as_voter {
            // Collect existing voter node IDs (NOT including ourselves yet - we'll be added via ConfChange)
            let existing_voters: Vec<u64> = discovered_peers
                .iter()
                .map(|p| p.node_id)
                .collect();

            info!(logger, "Joining management cluster as VOTER";
            "node_id" => node_id,
            "existing_voters" => ?existing_voters
            );

            ManagementRuntime::new_joining_node(
                config,
                transport.clone(),
                mailbox_rx,
                existing_voters,
                cluster_router.clone(),
                registry.clone(),
                logger.clone(),
            )?
        } else {
            // Collect existing voter node IDs (NOT including ourselves)
            let existing_voters: Vec<u64> = discovered_peers.iter().map(|p| p.node_id).collect();

            info!(logger, "Joining management cluster as LEARNER";
            "node_id" => node_id,
            "existing_voters" => ?existing_voters
            );

            ManagementRuntime::new_joining_learner(
                config,
                transport.clone(),
                mailbox_rx,
                existing_voters,
                cluster_router.clone(),
                registry.clone(),
                logger.clone(),
            )?
        };

        // Run Raft node in background
        let node_clone = node.clone();
        tokio::spawn(async move {
            info!(
                slog::Logger::root(slog::Discard, slog::o!()),
                "Starting Raft node event loop"
            );
            let _ = RaftNode::run_from_arc(node_clone).await;
        });

        // Do NOT campaign - we're joining an existing cluster
        // The leader will add us via add_node()

        // Layer 0: Start gRPC server
        let grpc_server = Arc::new(GrpcServer::new(
            cluster_router,
            node_id,
            address.clone(),
            transport.clone(),
            runtime.clone(),
        ));

        // Create WorkflowManagement service
        let workflow_service = WorkflowManagementService::new(runtime.clone(), logger.clone());

        let addr = address.parse()?;

        info!(logger, "Starting gRPC server"; "address" => &address);

        let grpc_server_clone = grpc_server.clone();
        let server_handle = tokio::spawn(async move {
            // Enable gRPC reflection for grpcurl support
            let reflection_service = tonic_reflection::server::Builder::configure()
                .register_encoded_file_descriptor_set(crate::grpc::proto::FILE_DESCRIPTOR_SET)
                .build_v1()
                .unwrap();

            if let Err(e) = Server::builder()
                .add_service(reflection_service)
                .add_service(
                    crate::grpc::proto::raft_service_server::RaftServiceServer::new(
                        (*grpc_server_clone).clone(),
                    ),
                )
                .add_service(
                    crate::grpc::proto::workflow_management_server::WorkflowManagementServer::new(
                        workflow_service,
                    ),
                )
                .serve(addr)
                .await
            {
                eprintln!("gRPC server failed to start on {:?}: {:?}", addr, e);
            }
        });

        info!(logger, "FullNode started in join mode"; "node_id" => node_id, "address" => &address);

        let full_node = Self {
            runtime: runtime.clone(),
            node,
            server_handle: Some(server_handle),
            workflow_registry: registry,
            address: address.clone(),
            logger: logger.clone(),
        };

        // Use AddNode RPC to register with management cluster leader
        info!(logger, "Calling AddNode RPC on management leader";
"node_id" => node_id, "leader" => &leader_peer.management_leader_address);

        use crate::grpc::proto::AddNodeRequest;
        use crate::grpc::proto::raft_service_client::RaftServiceClient;

        let leader_endpoint = format!("http://{}", leader_peer.management_leader_address);
        match RaftServiceClient::connect(leader_endpoint).await {
            Ok(mut client) => {
                let request = tonic::Request::new(AddNodeRequest {
                    node_id,
                    address: address.clone(),
                });

                match client.add_node(request).await {
                    Ok(response) => {
                        let resp = response.into_inner();
                        if resp.success {
                            info!(logger, "Successfully added to management cluster via RPC");
                        // ClusterManager will automatically assign us to an execution cluster
                        } else {
                            info!(logger, "AddNode RPC returned error"; "error" => &resp.error);
                        }
                    }
                    Err(e) => {
                        info!(logger, "AddNode RPC call failed"; "error" => %e);
                    }
                }
            }
            Err(e) => {
                info!(logger, "Failed to connect to management leader"; "error" => %e);
            }
        }

        Ok(full_node)
    }

    /// Create and start a new full node with HTTP transport in bootstrap mode
    ///
    /// # Arguments
    /// * `node_id` - Unique identifier for this node
    /// * `address` - Network address to bind to (e.g., "127.0.0.1:8001")
    /// * `storage_path` - Optional path for persistent storage (None = in-memory)
    /// * `logger` - Logger instance
    ///
    /// # Returns
    /// A running FullNode instance using HTTP transport
    pub async fn new_with_http(
        node_id: u64,
        address: String,
        storage_path: Option<std::path::PathBuf>,
        logger: Logger,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        info!(logger, "Creating FullNode with HTTP transport"; "node_id" => node_id, "address" => &address);

        // Layer 1: Create transport with HTTP message sender
        use crate::http::HttpMessageSender;
        let http_sender = Arc::new(HttpMessageSender::new(logger.clone()));
        let transport = Arc::new(TransportLayer::new(http_sender));

        // Create mailbox for this node
        let (mailbox_tx, mailbox_rx) = mpsc::channel(1000);

        // Layer 2: Create cluster router
        let cluster_router = Arc::new(ClusterRouter::new());

        // Register this node's cluster (cluster_id = 0 for management)
        cluster_router.register_cluster(0, mailbox_tx).await;

        // Create shared workflow registry
        let registry = Arc::new(Mutex::new(crate::workflow::WorkflowRegistry::new()));

        // Layers 3-7: Create management runtime (includes RaftNode, EventBus, ProposalRouter)
        let config = RaftNodeConfig {
            node_id,
            cluster_id: 0,          // Management cluster ID
            snapshot_interval: 100, // Take snapshot every 100 entries
            storage_path,
            ..Default::default()
        };

        let (runtime, node) = ManagementRuntime::new(
            config,
            transport.clone(),
            mailbox_rx,
            cluster_router.clone(),
            registry.clone(),
            logger.clone(),
        )?;

        // Run Raft node in background
        let node_clone = node.clone();
        tokio::spawn(async move {
            info!(
                slog::Logger::root(slog::Discard, slog::o!()),
                "Starting Raft node event loop"
            );
            let _ = RaftNode::run_from_arc(node_clone).await;
        });

        // Campaign to become leader (for single-node cluster)
        node.lock().await.campaign().await?;

        // Wait for leader election to complete
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        // Bootstrap node should add itself to trigger ClusterManager
        info!(
            logger,
            "Bootstrap node adding itself to management cluster to trigger ClusterManager"
        );
        // For HTTP transport, use full address with http:// prefix
        let full_address = format!("http://{}", address);
        let add_rx = runtime.add_node(node_id, full_address).await?;

        // Wait for the operation to complete
        match add_rx.await {
            Ok(Ok(_)) => info!(
                logger,
                "Bootstrap node successfully added to management cluster"
            ),
            Ok(Err(e)) => return Err(format!("Failed to add bootstrap node: {}", e).into()),
            Err(e) => return Err(format!("Add node oneshot error: {}", e).into()),
        }

        // Layer 0: Start HTTP server
        use crate::http::HttpServer;

        let addr: std::net::SocketAddr = address.parse()?;
        let http_server = HttpServer::new(
            node_id,
            addr,
            address.clone(),
            cluster_router.clone(),
            transport.clone(),
            runtime.clone(),
            registry.clone(),
            logger.clone(),
        );

        info!(logger, "Starting HTTP server"; "address" => &address);

        let server_handle = tokio::spawn(async move {
            if let Err(e) = http_server.start().await {
                eprintln!("HTTP server error: {}", e);
            }
        });

        info!(logger, "FullNode started with HTTP transport"; "node_id" => node_id, "address" => &address);

        Ok(Self {
            runtime,
            node,
            server_handle: Some(server_handle),
            workflow_registry: registry,
            address,
            logger,
        })
    }

    /// Create and start a new full node with HTTP transport in join mode
    ///
    /// # Arguments
    /// * `address` - Network address to bind to (e.g., "127.0.0.1:8001")
    /// * `seed_addresses` - Addresses of existing cluster nodes to discover
    /// * `storage_path` - Optional path for persistent storage (None = in-memory)
    /// * `logger` - Logger instance
    ///
    /// # Returns
    /// A running FullNode instance that has joined the cluster via HTTP
    pub async fn new_joining_with_http(
        address: String,
        seed_addresses: Vec<String>,
        storage_path: Option<std::path::PathBuf>,
        logger: Logger,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        info!(logger, "Creating FullNode with HTTP transport in join mode";
            "address" => &address,
            "seeds" => ?seed_addresses
        );

        // Discover existing peers via HTTP
        use crate::http::bootstrap::{discover_peers, next_node_id};
        let discovered_peers = discover_peers(seed_addresses).await;

        if discovered_peers.is_empty() {
            return Err("Failed to discover any peers via HTTP".into());
        }

        // Determine our node ID
        let node_id = next_node_id(&discovered_peers);
        info!(logger, "Assigned node ID"; "node_id" => node_id);

        // Find management leader
        let leader_peer = discovered_peers
            .iter()
            .find(|p| p.management_leader_node_id != 0)
            .ok_or("No management leader found in discovered peers")?;
        info!(logger, "Found management leader";
            "leader_node_id" => leader_peer.management_leader_node_id,
            "leader_address" => &leader_peer.management_leader_address
        );

        // Layer 1: Create transport with HTTP message sender
        use crate::http::HttpMessageSender;
        let http_sender = Arc::new(HttpMessageSender::new(logger.clone()));
        let transport = Arc::new(TransportLayer::new(http_sender));

        // Add all discovered peers to transport (with http:// prefix for HTTP transport)
        for peer in &discovered_peers {
            let peer_address = format!("http://{}", peer.address);
            transport.add_peer(peer.node_id, peer_address).await;
        }

        // Create mailbox for this node
        let (mailbox_tx, mailbox_rx) = mpsc::channel(1000);

        // Layer 2: Create cluster router
        let cluster_router = Arc::new(ClusterRouter::new());

        // Register this node's cluster (cluster_id = 0 for management)
        cluster_router.register_cluster(0, mailbox_tx).await;

        // Determine if we should join as voter or learner
        let should_join_as_voter = discovered_peers
            .first()
            .map(|p| p.should_join_as_voter)
            .unwrap_or(true);

        info!(logger, "Joining as";
            "role" => if should_join_as_voter { "voter" } else { "learner" }
        );

        // Create shared workflow registry
        let registry = Arc::new(Mutex::new(crate::workflow::WorkflowRegistry::new()));

        // Layers 3-7: Create management runtime
        let config = RaftNodeConfig {
            node_id,
            cluster_id: 0,
            snapshot_interval: 100,
            storage_path,
            ..Default::default()
        };

        let (runtime, node) = ManagementRuntime::new(
            config,
            transport.clone(),
            mailbox_rx,
            cluster_router.clone(),
            registry.clone(),
            logger.clone(),
        )?;

        // Run Raft node in background
        let node_clone = node.clone();
        tokio::spawn(async move {
            info!(
                slog::Logger::root(slog::Discard, slog::o!()),
                "Starting Raft node event loop"
            );
            let _ = RaftNode::run_from_arc(node_clone).await;
        });

        // Layer 0: Start HTTP server
        use crate::http::HttpServer;

        let addr: std::net::SocketAddr = address.parse()?;
        let http_server = HttpServer::new(
            node_id,
            addr,
            address.clone(),
            cluster_router.clone(),
            transport.clone(),
            runtime.clone(),
            registry.clone(),
            logger.clone(),
        );

        info!(logger, "Starting HTTP server"; "address" => &address);

        let server_handle = tokio::spawn(async move {
            if let Err(e) = http_server.start().await {
                eprintln!("HTTP server error: {}", e);
            }
        });

        info!(logger, "FullNode started in join mode with HTTP"; "node_id" => node_id, "address" => &address);

        let full_node = Self {
            runtime: runtime.clone(),
            node,
            server_handle: Some(server_handle),
            workflow_registry: registry,
            address: address.clone(),
            logger: logger.clone(),
        };

        // Use HTTP POST to register with management cluster leader
        info!(logger, "Registering with management leader via HTTP";
            "node_id" => node_id,
            "leader" => &leader_peer.management_leader_address
        );

        use crate::http::bootstrap::register_with_peer;
        match register_with_peer(&leader_peer.management_leader_address, &address, node_id).await {
            Ok(assigned_id) => {
                info!(logger, "Successfully registered with management leader";
                    "assigned_node_id" => assigned_id
                );
            }
            Err(e) => {
                info!(logger, "Failed to register with management leader (will retry)"; "error" => %e);
            }
        }

        Ok(full_node)
    }

    /// Get a reference to the management runtime
    pub fn runtime(&self) -> &Arc<ManagementRuntime<WorkflowRuntime>> {
        &self.runtime
    }

    /// Get a reference to the shared workflow registry
    ///
    /// This allows registering workflows before execution clusters are created.
    /// All WorkflowRuntime instances share this registry.
    pub fn workflow_registry(&self) -> &Arc<Mutex<crate::workflow::WorkflowRegistry>> {
        &self.workflow_registry
    }

    /// Get the node address
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Get the node ID
    pub fn node_id(&self) -> u64 {
        self.runtime.node_id()
    }

    /// Check if this node is the leader
    pub async fn is_leader(&self) -> bool {
        self.runtime.is_leader().await
    }

    /// Shutdown the node
    pub async fn shutdown(mut self) {
        info!(self.logger, "Shutting down FullNode");

        if let Some(handle) = self.server_handle.take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_logger() -> Logger {
        use slog::Drain;
        let decorator = slog_term::PlainDecorator::new(std::io::stdout());
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        Logger::root(drain, slog::o!())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_full_node_single_node_cluster() {
        let logger = create_logger();

        // Create a full node with gRPC server
        let node = FullNode::new(
            1,
            "127.0.0.1:50051".to_string(),
            None, // No persistent storage for test
            logger.clone(),
        )
        .await
        .expect("Failed to create FullNode");

        // Wait for node to become leader and bootstrap to complete
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        assert!(node.is_leader().await, "Node should be leader");

        // Note: ClusterManager automatically created cluster_id=1 during bootstrap
        // when the node added itself to the management cluster

        // Create a sub-cluster using the management runtime
        let cluster_id = node
            .runtime()
            .create_sub_cluster(vec![1, 2, 3])
            .await
            .expect("Create sub-cluster should succeed");
        assert_eq!(cluster_id, 2); // Should get ID 2 (ID 1 was auto-created by ClusterManager)

        // Set metadata using the management runtime
        node.runtime()
            .set_metadata(cluster_id, "type".to_string(), "kv".to_string())
            .await
            .expect("Set metadata should succeed");

        node.runtime()
            .set_metadata(cluster_id, "region".to_string(), "us-west".to_string())
            .await
            .expect("Set metadata should succeed");

        // Verify metadata was set correctly
        let metadata = node
            .runtime()
            .get_sub_cluster(&cluster_id)
            .await
            .expect("Sub-cluster should exist");

        assert_eq!(metadata.node_ids, vec![1, 2, 3]);
        assert_eq!(metadata.metadata.get("type"), Some(&"kv".to_string()));
        assert_eq!(
            metadata.metadata.get("region"),
            Some(&"us-west".to_string())
        );

        // Delete metadata
        node.runtime()
            .delete_metadata(cluster_id, "region".to_string())
            .await
            .expect("Delete metadata should succeed");

        let metadata = node
            .runtime()
            .get_sub_cluster(&cluster_id)
            .await
            .expect("Sub-cluster should exist");

        assert!(metadata.metadata.get("region").is_none());
        assert_eq!(metadata.metadata.get("type"), Some(&"kv".to_string()));

        // Delete sub-cluster
        node.runtime()
            .delete_sub_cluster(cluster_id)
            .await
            .expect("Delete sub-cluster should succeed");

        let metadata = node.runtime().get_sub_cluster(&cluster_id).await;
        assert!(metadata.is_none());

        // Shutdown
        node.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_two_node_cluster_with_discovery() {
        let logger = create_logger();

        println!("\n=== Starting two-node FullNode integration test ===\n");

        // Node 1: Bootstrap mode (creates single-node cluster)
        println!("Creating Node 1 (bootstrap mode) on 127.0.0.1:50061...");
        let node1 = FullNode::new(
            1,
            "127.0.0.1:50061".to_string(),
            None, // No persistent storage for test
            logger.clone(),
        )
        .await
        .expect("Failed to create node 1");

        // Wait for node 1 to become leader
        println!("Waiting for Node 1 to become leader...");
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        assert!(node1.is_leader().await, "Node 1 should be leader");
        println!("✓ Node 1 is leader");

        // Node 2: Join mode (discovers and joins via node 1)
        println!("\nCreating Node 2 (join mode) on 127.0.0.1:50062...");
        let node2 = FullNode::new_joining(
            "127.0.0.1:50062".to_string(),
            vec!["127.0.0.1:50061".to_string()],
            None, // No persistent storage for test
            logger.clone(),
        )
        .await
        .expect("Failed to create node 2");

        println!("Node 2 created, node_id: {}", node2.node_id());

        // Add node 2 to the management cluster via node 1 (as leader)
        println!("\nAdding Node 2 to management cluster via Node 1...");
        let add_rx = node1
            .runtime()
            .add_node(2, "127.0.0.1:50062".to_string())
            .await
            .expect("Should initiate add_node");

        // Wait for the configuration change to complete
        match add_rx.await {
            Ok(Ok(_)) => println!("✓ Node 2 added to management cluster"),
            Ok(Err(e)) => println!("⚠ Add node returned error: {}", e),
            Err(e) => println!("⚠ Add node oneshot error: {}", e),
        }

        // Wait for ClusterManager to automatically create/assign execution clusters
        println!("\nWaiting for ClusterManager to create execution clusters...");
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        // Verify that ClusterManager automatically created execution cluster(s)
        println!("\nVerifying ClusterManager created execution clusters...");
        let all_clusters = node1.runtime().list_sub_clusters().await;

        println!(
            "✓ ClusterManager created {} execution cluster(s): {:?}",
            all_clusters.len(),
            all_clusters
        );

        // ClusterManager should have created at least one cluster
        assert!(
            !all_clusters.is_empty(),
            "ClusterManager should have created at least one execution cluster"
        );

        // Get the first cluster for verification
        let cluster_id = all_clusters[0];

        println!("\nVerifying cluster {} contains both nodes...", cluster_id);
        let metadata1 = node1
            .runtime()
            .get_sub_cluster(&cluster_id)
            .await
            .expect("Node 1 should have sub-cluster metadata");
        println!("✓ Cluster {} nodes: {:?}", cluster_id, metadata1.node_ids);

        // Both nodes should be in the same execution cluster (since cluster size 2 < target size 3)
        assert_eq!(
            metadata1.node_ids.len(),
            2,
            "Cluster should have both nodes"
        );
        assert!(
            metadata1.node_ids.contains(&1),
            "Cluster should contain node 1"
        );
        assert!(
            metadata1.node_ids.contains(&2),
            "Cluster should contain node 2"
        );

        // Verify node 2 can also see the clusters
        println!("\nVerifying Node 2 observed clusters...");
        let metadata2 = node2
            .runtime()
            .get_sub_cluster(&cluster_id)
            .await
            .expect("Node 2 should have sub-cluster metadata");
        println!("✓ Node 2 metadata: node_ids={:?}", metadata2.node_ids);
        assert_eq!(
            metadata2.node_ids, metadata1.node_ids,
            "Both nodes should see the same cluster membership"
        );

        println!(
            "\n✓ ClusterManager successfully created and assigned both nodes to the same execution cluster!"
        );

        println!("\n=== Test complete - shutting down nodes ===");

        // Cleanup
        node1.shutdown().await;
        node2.shutdown().await;

        println!("✓ All tests passed!\n");
    }
}
