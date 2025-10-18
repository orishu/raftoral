//! High-level runtime for managing Raftoral nodes with gRPC transport.
//!
//! This module provides a simplified interface for starting and managing
//! Raftoral nodes in a distributed cluster, handling all the complexity
//! of bootstrapping, peer discovery, and graceful shutdown.

use crate::grpc::{
    bootstrap, discover_peers,
    GrpcServerHandle, ServerConfigurator,
};
use crate::grpc::client::ChannelBuilder;
use crate::raft::generic::grpc_transport::{GrpcClusterTransport, NodeConfig};
use crate::workflow::WorkflowRuntime;
use log::{debug, info, warn};
use std::sync::Arc;

/// Configuration for starting a Raftoral gRPC node.
pub struct RaftoralConfig {
    /// Address to listen on for gRPC connections (e.g., "0.0.0.0:5001")
    pub listen_address: String,

    /// Advertised address for other nodes to connect to (e.g., "192.168.1.10:5001")
    /// If None, uses the listen_address
    pub advertise_address: Option<String>,

    /// Node ID (optional, will be auto-assigned if joining existing cluster)
    pub node_id: Option<u64>,

    /// Addresses of peer nodes to discover (e.g., ["192.168.1.10:5001", "192.168.1.11:5001"])
    /// Empty for bootstrap mode
    pub peers: Vec<String>,

    /// Bootstrap a new cluster (use this for the first node)
    pub bootstrap: bool,

    /// Optional gRPC channel builder for customizing client connections
    /// (TLS, timeouts, compression, etc.)
    pub channel_builder: Option<ChannelBuilder>,

    /// Optional gRPC server configurator for customizing the server
    /// (TLS, concurrency limits, timeouts, etc.)
    pub server_configurator: Option<ServerConfigurator>,
}

impl RaftoralConfig {
    /// Create a configuration for bootstrapping a new cluster.
    pub fn bootstrap(listen_address: String, node_id: Option<u64>) -> Self {
        Self {
            listen_address,
            advertise_address: None,
            node_id,
            peers: Vec::new(),
            bootstrap: true,
            channel_builder: None,
            server_configurator: None,
        }
    }

    /// Create a configuration for joining an existing cluster.
    pub fn join(listen_address: String, peers: Vec<String>) -> Self {
        Self {
            listen_address,
            advertise_address: None,
            node_id: None,
            peers,
            bootstrap: false,
            channel_builder: None,
            server_configurator: None,
        }
    }

    /// Set the advertised address (different from listen address).
    pub fn with_advertise_address(mut self, address: String) -> Self {
        self.advertise_address = Some(address);
        self
    }

    /// Set the node ID explicitly.
    pub fn with_node_id(mut self, node_id: u64) -> Self {
        self.node_id = Some(node_id);
        self
    }

    /// Set a custom gRPC channel builder for client connections.
    ///
    /// This allows customization of outgoing gRPC connections with features like:
    /// - TLS/SSL configuration
    /// - Connection timeouts
    /// - Keep-alive settings
    /// - Compression
    /// - Interceptors for authentication
    pub fn with_channel_builder(mut self, builder: ChannelBuilder) -> Self {
        self.channel_builder = Some(builder);
        self
    }

    /// Set a custom gRPC server configurator.
    ///
    /// This allows customization of the gRPC server with features like:
    /// - TLS/SSL configuration
    /// - Concurrency limits
    /// - Request timeouts
    /// - Maximum message sizes
    /// - Interceptors for authentication/authorization
    pub fn with_server_configurator(mut self, configurator: ServerConfigurator) -> Self {
        self.server_configurator = Some(configurator);
        self
    }
}

/// High-level runtime for managing a Raftoral node with gRPC transport.
///
/// This runtime handles:
/// - Transport initialization
/// - Cluster bootstrapping or joining
/// - gRPC server lifecycle
/// - Graceful shutdown with cluster membership updates
///
/// # Example
///
/// ```no_run
/// use raftoral::runtime::{RaftoralGrpcRuntime, RaftoralConfig};
/// use tokio::signal;
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     // Bootstrap a new cluster
///     let config = RaftoralConfig::bootstrap("127.0.0.1:5001".to_string(), Some(1));
///     let runtime = RaftoralGrpcRuntime::start(config).await?;
///
///     // Register workflows here...
///     // runtime.workflow_runtime().register_workflow_closure(...)?;
///
///     // Wait for shutdown signal
///     signal::ctrl_c().await?;
///     runtime.shutdown().await?;
///
///     Ok(())
/// }
/// ```
/// Phase 3: Transport is now type-parameter-free!
pub struct RaftoralGrpcRuntime {
    node_id: u64,
    _transport: Arc<GrpcClusterTransport>,
    node_manager: Arc<crate::nodemanager::NodeManager>,
    server_handle: GrpcServerHandle,
    advertise_address: String,
    is_bootstrap: bool,
}

impl RaftoralGrpcRuntime {
    /// Start a Raftoral node with the given configuration.
    ///
    /// This method handles all initialization steps:
    /// 1. Peer discovery (if joining an existing cluster)
    /// 2. Node ID assignment
    /// 3. Transport initialization
    /// 4. Cluster creation
    /// 5. gRPC server startup
    /// 6. Cluster membership update (if joining)
    ///
    /// Returns a runtime instance that can be used to register workflows
    /// and manage the node lifecycle.
    pub async fn start(config: RaftoralConfig) -> Result<Self, Box<dyn std::error::Error>> {
        let advertise_addr = config
            .advertise_address
            .as_ref()
            .unwrap_or(&config.listen_address)
            .clone();

        info!("Starting Raftoral node");
        info!("Listen address: {}", config.listen_address);
        if config.advertise_address.is_some() {
            info!("Advertise address: {}", advertise_addr);
        }

        // Determine node ID and mode
        let node_id = if config.bootstrap {
            let node_id = config.node_id.unwrap_or(1);
            info!("Mode: Bootstrap (starting new cluster)");
            info!("Node ID: {}", node_id);
            node_id
        } else {
            info!("Mode: Join existing cluster");

            if config.peers.is_empty() {
                return Err("--peers is required when not in bootstrap mode".into());
            }

            debug!("Discovering peers: {:?}", config.peers);
            let discovered = discover_peers(config.peers.clone()).await;

            if discovered.is_empty() {
                return Err("Could not discover any peers".into());
            }

            info!("Discovered {} peer(s)", discovered.len());
            for peer in &discovered {
                debug!("Peer: Node {} at {}", peer.node_id, peer.address);
            }

            if let Some(id) = config.node_id {
                info!("Using provided node ID: {}", id);
                id
            } else {
                let id = bootstrap::next_node_id(&discovered);
                info!("Auto-assigned node ID: {}", id);
                id
            }
        };

        // Build initial node configuration - only this node
        // Other nodes will be added dynamically via add_peer when they join
        let nodes = vec![NodeConfig {
            node_id,
            address: advertise_addr.clone(),
        }];

        debug!("Initial transport nodes: {}", nodes.len());

        // Create transport with optional custom channel builder
        // Phase 3: Transport is now type-parameter-free!
        // This transport is shared by both management and workflow clusters
        let transport = if let Some(channel_builder) = config.channel_builder {
            Arc::new(GrpcClusterTransport::new_with_channel_builder(
                nodes,
                channel_builder,
            ))
        } else {
            Arc::new(GrpcClusterTransport::new(nodes))
        };
        transport.start().await?;
        info!("Transport started");

        // Create NodeManager (creates both management and workflow clusters internally)
        // ClusterRouter is created and configured inside NodeManager
        let node_manager = Arc::new(crate::nodemanager::NodeManager::new(
            transport.clone(),
            node_id,
        ).await?);
        info!("NodeManager ready with management and workflow clusters");
        info!("ClusterRouter configured for management (cluster_id=0) and workflow (cluster_id=1) clusters");

        // Start gRPC server - gets ClusterRouter from NodeManager internally
        // Server must be started BEFORE add_node so it can receive Raft messages
        let server_handle = if let Some(server_config) = config.server_configurator {
            crate::grpc::start_grpc_server_with_config(
                config.listen_address.clone(),
                node_manager.clone(),
                node_id,
                Some(server_config),
            )
            .await?
        } else {
            crate::grpc::start_grpc_server(
                config.listen_address.clone(),
                node_manager.clone(),
                node_id,
            )
            .await?
        };
        info!("gRPC server listening on {}", config.listen_address);

        // If joining an existing cluster, add this node via ConfChange
        // This must happen AFTER the server is started so we can receive Raft messages
        if !config.bootstrap && !config.peers.is_empty() {
            info!("Adding node {} to existing cluster", node_id);
            // add_node automatically routes to the leader
            match node_manager.add_node(node_id, advertise_addr.clone()).await {
                Ok(_) => info!("Node added to cluster"),
                Err(e) => {
                    warn!("Failed to add node to cluster: {}", e);
                    warn!("Continuing anyway - node may already be in cluster");
                }
            }
        }

        info!("Node {} is running", node_id);
        info!("Cluster size: {} nodes", node_manager.cluster_size());
        info!("Ready to accept workflow registrations");

        // Give the cluster a moment to stabilize (especially important for joining nodes)
        if !config.bootstrap {
            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
        }

        Ok(Self {
            node_id,
            _transport: transport,
            node_manager,
            server_handle,
            advertise_address: advertise_addr,
            is_bootstrap: config.bootstrap,
        })
    }

    /// Get a reference to the workflow runtime for registering and executing workflows.
    pub fn workflow_runtime(&self) -> Arc<WorkflowRuntime> {
        self.node_manager.workflow_runtime()
    }

    /// Get the node ID of this runtime.
    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    /// Get the advertised address of this node.
    pub fn advertise_address(&self) -> &str {
        &self.advertise_address
    }

    /// Get the current cluster size.
    pub fn cluster_size(&self) -> usize {
        self.node_manager.cluster_size()
    }

    /// Gracefully shutdown the node.
    ///
    /// This method:
    /// 1. Removes the node from the cluster (if not the last node)
    /// 2. Stops the gRPC server
    /// 3. Cleans up resources
    pub async fn shutdown(self) -> Result<(), Box<dyn std::error::Error>> {
        info!("Shutting down node {}", self.node_id);

        // Remove this node from the cluster before shutting down
        if !self.is_bootstrap || self.cluster_size() > 1 {
            info!("Removing node {} from cluster", self.node_id);
            match self.node_manager.remove_node(self.node_id).await {
                Ok(_) => info!("Node removed from cluster"),
                Err(e) => warn!("Failed to remove node from cluster: {}", e),
            }
        } else {
            debug!("Skipping node removal - last node in cluster");
        }

        self.server_handle.shutdown();
        info!("Server stopped");

        Ok(())
    }
}
