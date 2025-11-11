//! Configuration for Raftoral nodes

// TODO: Re-implement these types in grpc2 if needed
// use crate::grpc::{client::ChannelBuilder, ServerConfigurator};

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

    // TODO: Re-enable these when gRPC customization types are re-implemented in grpc2
    // /// Optional gRPC channel builder for customizing client connections
    // /// (TLS, timeouts, compression, etc.)
    // pub channel_builder: Option<ChannelBuilder>,
    //
    // /// Optional gRPC server configurator for customizing the server
    // /// (TLS, concurrency limits, timeouts, etc.)
    // pub server_configurator: Option<ServerConfigurator>,
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

    // TODO: Re-enable these when gRPC customization types are re-implemented in grpc2
    // /// Set a custom gRPC channel builder for client connections.
    // ///
    // /// This allows customization of outgoing gRPC connections with features like:
    // /// - TLS/SSL configuration
    // /// - Connection timeouts
    // /// - Keep-alive settings
    // /// - Compression
    // /// - Interceptors for authentication
    // pub fn with_channel_builder(mut self, builder: ChannelBuilder) -> Self {
    //     self.channel_builder = Some(builder);
    //     self
    // }
    //
    // /// Set a custom gRPC server configurator.
    // ///
    // /// This allows customization of the gRPC server with features like:
    // /// - TLS/SSL configuration
    // /// - Concurrency limits
    // /// - Request timeouts
    // /// - Maximum message sizes
    // /// - Interceptors for authentication/authorization
    // pub fn with_server_configurator(mut self, configurator: ServerConfigurator) -> Self {
    //     self.server_configurator = Some(configurator);
    //     self
    // }
}
