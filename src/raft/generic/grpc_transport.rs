use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::mpsc;
use crate::raft::generic::message::Message;
use crate::grpc::client::{RaftClient, ChannelBuilder, default_channel_builder};
use crate::grpc::server::raft_proto::GenericMessage;

/// Configuration for a node in the gRPC cluster
#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub node_id: u64,
    pub address: String,  // host:port
}

/// All components needed to send messages to a peer node
/// Consolidates sender, gRPC client, and forwarder handle for atomic management
/// Phase 3: Now works with GenericMessage (no type parameter)
struct Destination
{
    /// Channel to send messages to this node (now GenericMessage)
    sender: mpsc::UnboundedSender<GenericMessage>,

    /// gRPC client for remote communication (None for local node or not yet connected)
    grpc_client: Option<Arc<tokio::sync::Mutex<RaftClient>>>,

    /// Forwarder task handle (None for local node)
    forwarder_handle: Option<tokio::task::JoinHandle<()>>,
}

/// Generic gRPC-based transport for distributed Raft clusters
///
/// Phase 3: Now works with GenericMessage (no type parameters!)
/// This enables a single transport instance to be shared across multiple clusters
/// with different command types (management, workflow, etc.)
///
/// This transport enables Raft nodes to communicate over network via gRPC.
/// It maintains a mapping of node IDs to network addresses and provides
/// the infrastructure for sending Raft messages between nodes.
pub struct GrpcClusterTransport
{
    /// Unified peer management: node_id -> all communication components
    destinations: Arc<RwLock<HashMap<u64, Destination>>>,

    /// Node configurations (node_id -> address)
    nodes: Arc<RwLock<HashMap<u64, NodeConfig>>>,

    /// Receivers for each node (held until extract_receiver is called)
    /// Now returns GenericMessage receivers
    node_receivers: Arc<Mutex<HashMap<u64, mpsc::UnboundedReceiver<GenericMessage>>>>,

    /// Shutdown signal
    shutdown_tx: Arc<Mutex<Option<tokio::sync::broadcast::Sender<()>>>>,

    /// Custom channel builder for gRPC connections
    channel_builder: ChannelBuilder,

    /// Discovered cluster configuration (voters from discovery)
    /// Used to initialize joining nodes with proper Raft configuration
    discovered_voters: Arc<RwLock<Vec<u64>>>,
}

impl GrpcClusterTransport
{
    /// Create a new gRPC transport with the given node configurations
    /// Phase 3: Now type-parameter-free!
    pub fn new(nodes: Vec<NodeConfig>) -> Self {
        Self::new_with_channel_builder(nodes, default_channel_builder())
    }

    /// Create a new gRPC transport with a custom channel builder
    ///
    /// This allows customization of gRPC connections with features like:
    /// - TLS/SSL configuration
    /// - Custom authentication headers
    /// - Timeout settings
    /// - Compression options
    ///
    /// # Example
    /// ```no_run
    /// use raftoral::raft::generic::grpc_transport::{GrpcClusterTransport, NodeConfig};
    /// use raftoral::workflow::WorkflowCommandExecutor;
    /// use raftoral::grpc::client::ChannelBuilder;
    /// use tonic::transport::Channel;
    /// use std::sync::Arc;
    ///
    /// let nodes = vec![
    ///     NodeConfig { node_id: 1, address: "127.0.0.1:5001".to_string() },
    ///     NodeConfig { node_id: 2, address: "127.0.0.1:5002".to_string() },
    /// ];
    ///
    /// let channel_builder = Arc::new(|address: String| {
    ///     Box::pin(async move {
    ///         Channel::from_shared(format!("http://{}", address))
    ///             .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
    ///             .connect()
    ///             .await
    ///             .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    ///     }) as std::pin::Pin<Box<dyn std::future::Future<Output = Result<Channel, Box<dyn std::error::Error + Send + Sync>>> + Send>>
    /// }) as ChannelBuilder;
    ///
    /// let transport = GrpcClusterTransport::<WorkflowCommandExecutor>::new_with_channel_builder(
    ///     nodes,
    ///     channel_builder
    /// );
    /// ```
    pub fn new_with_channel_builder(nodes: Vec<NodeConfig>, channel_builder: ChannelBuilder) -> Self {
        let mut node_map = HashMap::new();
        let mut destinations_map = HashMap::new();
        let mut node_receivers_map = HashMap::new();

        // Create channels for each node
        for node in nodes {
            let (sender, receiver) = mpsc::unbounded_channel();

            // Create destination with sender only (no client/forwarder yet)
            destinations_map.insert(node.node_id, Destination {
                sender,
                grpc_client: None,
                forwarder_handle: None,
            });

            node_receivers_map.insert(node.node_id, receiver);
            node_map.insert(node.node_id, node);
        }

        GrpcClusterTransport {
            destinations: Arc::new(RwLock::new(destinations_map)),
            nodes: Arc::new(RwLock::new(node_map)),
            node_receivers: Arc::new(Mutex::new(node_receivers_map)),
            shutdown_tx: Arc::new(Mutex::new(None)),
            channel_builder,
            discovered_voters: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Extract the receiver for a specific node (raw GenericMessage receiver)
    /// This removes the receiver from the internal map, so it can only be called once per node
    /// Phase 3: Returns GenericMessage receiver
    pub fn extract_receiver(
        &self,
        node_id: u64,
    ) -> Result<mpsc::UnboundedReceiver<GenericMessage>, Box<dyn std::error::Error>> {
        let mut receivers = self.node_receivers.lock().unwrap();
        receivers.remove(&node_id)
            .ok_or_else(|| format!("Node {} receiver already extracted or doesn't exist", node_id).into())
    }

    /// Extract a typed receiver that deserializes GenericMessage → Message<C>
    /// This creates an adapter channel that performs deserialization
    /// Phase 3: Helper for RaftCluster which needs Message<C>
    pub fn extract_typed_receiver<C>(
        &self,
        node_id: u64,
    ) -> Result<mpsc::UnboundedReceiver<Message<C>>, Box<dyn std::error::Error>>
    where
        C: Clone + std::fmt::Debug + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    {
        let generic_rx = self.extract_receiver(node_id)?;

        // Create adapter channel
        let (typed_tx, typed_rx) = mpsc::unbounded_channel();

        // Spawn adapter task that deserializes GenericMessage → Message<C>
        tokio::spawn(async move {
            let mut rx = generic_rx;
            while let Some(generic_msg) = rx.recv().await {
                match Message::<C>::from_protobuf(generic_msg) {
                    Ok(message) => {
                        if typed_tx.send(message).is_err() {
                            break; // Receiver dropped
                        }
                    }
                    Err(e) => {
                        eprintln!("Failed to deserialize message: {}", e);
                    }
                }
            }
        });

        Ok(typed_rx)
    }

    /// Start the transport by creating the shutdown channel and connecting to all nodes
    pub async fn start(&self) -> Result<(), Box<dyn std::error::Error>> {
        // Create shutdown channel
        let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);
        {
            *self.shutdown_tx.lock().unwrap() = Some(shutdown_tx.clone());
        }

        // Get all nodes
        let node_list: Vec<NodeConfig> = {
            let nodes = self.nodes.read().unwrap();
            nodes.values().cloned().collect()
        };

        // Create gRPC clients for all nodes using the configured channel builder
        for node in &node_list {
            match RaftClient::connect_with_channel_builder(
                node.address.clone(),
                self.channel_builder.clone()
            ).await {
                Ok(client) => {
                    let mut dests = self.destinations.write().unwrap();
                    if let Some(dest) = dests.get_mut(&node.node_id) {
                        dest.grpc_client = Some(Arc::new(tokio::sync::Mutex::new(client)));
                    }
                }
                Err(e) => {
                    eprintln!("Warning: Failed to connect to node {}: {}", node.node_id, e);
                    // Continue anyway - the node might not be up yet
                }
            }
        }

        Ok(())
    }
    /// Remove a node from the transport configuration
    pub async fn remove_node(&self, node_id: u64) -> Result<(), Box<dyn std::error::Error>> {
        // Remove destination (forwarder will exit when sender is dropped)
        {
            let mut destinations = self.destinations.write().unwrap();
            destinations.remove(&node_id);
        }

        {
            let mut receivers = self.node_receivers.lock().unwrap();
            receivers.remove(&node_id);
        }

        {
            let mut nodes = self.nodes.write().unwrap();
            nodes.remove(&node_id);
        }

        Ok(())
    }

    /// Get the address for a specific node
    pub async fn get_node_address(&self, node_id: u64) -> Option<String> {
        let nodes = self.nodes.read().unwrap();
        nodes.get(&node_id).map(|n| n.address.clone())
    }

    /// Get all node configurations
    pub async fn get_all_nodes(&self) -> Vec<NodeConfig> {
        let nodes = self.nodes.read().unwrap();
        nodes.values().cloned().collect()
    }

    /// Set discovered voter configuration from peer discovery
    /// This should be called after discovering the cluster but before creating nodes
    pub fn set_discovered_voters(&self, voters: Vec<u64>) {
        *self.discovered_voters.write().unwrap() = voters;
    }

    /// Get discovered voter configuration
    pub fn get_discovered_voters(&self) -> Vec<u64> {
        self.discovered_voters.read().unwrap().clone()
    }
}

// Phase 3: All methods now work with GenericMessage
impl GrpcClusterTransport
{
    /// Add a new node to the transport configuration
    /// IMPORTANT: This creates gRPC client and forwarder for the new node
    /// Phase 3: Now works with GenericMessage
    pub async fn add_node(&self, node: NodeConfig) -> Result<(), Box<dyn std::error::Error>> {
        // Check if already exists
        {
            let nodes = self.nodes.read().unwrap();
            if nodes.contains_key(&node.node_id) {
                return Ok(()); // Already added
            }
        }

        // Add to nodes map
        {
            let mut nodes = self.nodes.write().unwrap();
            nodes.insert(node.node_id, node.clone());
        }

        // Create receiver for this node (will be claimed when cluster is created)
        let (_tx, rx) = mpsc::unbounded_channel();

        {
            let mut receivers = self.node_receivers.lock().unwrap();
            receivers.insert(node.node_id, rx);
        }

        // Check if transport has been started (needed for forwarders)
        let shutdown_rx_opt = {
            let shutdown_tx = self.shutdown_tx.lock().unwrap();
            shutdown_tx.as_ref().map(|tx| tx.subscribe())
        };

        // Create sender channel and destination
        let (sender, forwarder_handle) = if let Some(shutdown_rx) = shutdown_rx_opt {
            // CRITICAL FIX: Transport is started, create forwarder and gRPC client
            // This ensures messages can be sent to the new node immediately
            let (forwarder_tx, forwarder_rx) = mpsc::unbounded_channel();

            // Spawn the forwarder task for this peer
            let handle = self.spawn_peer_forwarder(node.node_id, forwarder_rx, shutdown_rx);

            // Spawn async task to create gRPC client
            let destinations = self.destinations.clone();
            let channel_builder = self.channel_builder.clone();
            let node_id = node.node_id;
            let address = node.address.clone();
            tokio::spawn(async move {
                match RaftClient::connect_with_channel_builder(address.clone(), channel_builder).await {
                    Ok(client) => {
                        // Update destination with gRPC client
                        let mut dests = destinations.write().unwrap();
                        if let Some(dest) = dests.get_mut(&node_id) {
                            dest.grpc_client = Some(Arc::new(tokio::sync::Mutex::new(client)));
                        }
                    }
                    Err(e) => {
                        eprintln!("Warning: Failed to connect to node {}: {}", node_id, e);
                    }
                }
            });

            (forwarder_tx, Some(handle))
        } else {
            // Transport not started yet - create basic channel without forwarder
            // Forwarders will be created when create_cluster() is called
            let (tx, _rx) = mpsc::unbounded_channel();
            (tx, None)
        };

        // Insert destination with appropriate configuration
        {
            let mut dests = self.destinations.write().unwrap();
            dests.insert(node.node_id, Destination {
                sender,
                grpc_client: None, // Will be set async if transport started
                forwarder_handle,
            });
        }

        Ok(())
    }

    /// Spawn a background task that forwards messages to a remote peer via gRPC
    /// Phase 3: Now works with GenericMessage directly (already serialized)
    fn spawn_peer_forwarder(
        &self,
        target_node_id: u64,
        mut forwarder_rx: mpsc::UnboundedReceiver<GenericMessage>,
        mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
    ) -> tokio::task::JoinHandle<()> {
        let destinations = self.destinations.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(proto_msg) = forwarder_rx.recv() => {
                        // Get the gRPC client for this node from destination
                        let client_arc = {
                            let dests = destinations.read().unwrap();
                            dests.get(&target_node_id).and_then(|dest| dest.grpc_client.clone())
                        };

                        if let Some(client) = client_arc {
                            let mut client = client.lock().await;

                            // Phase 3: Message is already serialized to GenericMessage
                            if let Err(e) = client.send_message(proto_msg).await {
                                eprintln!("Failed to send message to node {}: {}", target_node_id, e);
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        break;
                    }
                }
            }
        })
    }

    /// Internal method to get a sender for a specific node
    /// Phase 3: Now returns GenericMessage sender
    pub async fn get_node_sender(&self, node_id: u64) -> Option<mpsc::UnboundedSender<GenericMessage>> {
        let destinations = self.destinations.read().unwrap();
        destinations.get(&node_id).map(|dest| dest.sender.clone())
    }
}

// Phase 3: TransportInteraction implementation
// The transport is type-parameter-free, but the trait is still generic over Message<C>
// This impl converts Message<C> → GenericMessage at the boundary
impl<C> crate::raft::generic::transport::TransportInteraction<Message<C>> for GrpcClusterTransport
where
    C: Clone + std::fmt::Debug + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
{
    fn send_message_to_node(
        &self,
        target_node_id: u64,
        message: Message<C>
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Phase 3: Serialize Message<C> → GenericMessage at the transport boundary
        let proto_msg = message.to_protobuf()?;

        let destinations = self.destinations.read().unwrap();
        let dest = destinations.get(&target_node_id)
            .ok_or_else(|| format!("Node {} not found", target_node_id))?;

        dest.sender.send(proto_msg)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

        Ok(())
    }

    fn list_nodes(&self) -> Vec<u64> {
        self.destinations.read().unwrap().keys().copied().collect()
    }

    fn add_peer(
        &self,
        node_id: u64,
        address: String
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Check if already exists
        {
            let nodes = self.nodes.read().unwrap();
            if nodes.contains_key(&node_id) {
                return Ok(()); // Already added
            }
        }

        // Create node config
        let node_config = NodeConfig { node_id, address: address.clone() };

        // Add to nodes map
        {
            let mut nodes = self.nodes.write().unwrap();
            nodes.insert(node_id, node_config.clone());
        }

        // Create receiver for this node (will be claimed when cluster is created)
        let (_tx, rx) = mpsc::unbounded_channel();

        {
            let mut receivers = self.node_receivers.lock().unwrap();
            receivers.insert(node_id, rx);
        }

        // Get shutdown receiver for forwarder
        let shutdown_rx = {
            let shutdown_tx = self.shutdown_tx.lock().unwrap();
            shutdown_tx.as_ref()
                .ok_or("Transport not started")?
                .subscribe()
        };

        // CRITICAL: Create forwarder channel and spawn forwarder task
        let (forwarder_tx, forwarder_rx) = mpsc::unbounded_channel();

        // Spawn the forwarder task for this peer
        let handle = self.spawn_peer_forwarder(node_id, forwarder_rx, shutdown_rx);

        // Spawn async task to create gRPC client
        let destinations = self.destinations.clone();
        let channel_builder = self.channel_builder.clone();
        tokio::spawn(async move {
            match RaftClient::connect_with_channel_builder(address.clone(), channel_builder).await {
                Ok(client) => {
                    // Update destination with gRPC client
                    let mut dests = destinations.write().unwrap();
                    if let Some(dest) = dests.get_mut(&node_id) {
                        dest.grpc_client = Some(Arc::new(tokio::sync::Mutex::new(client)));
                    }
                }
                Err(e) => {
                    eprintln!("Warning: Failed to connect to node {}: {}", node_id, e);
                }
            }
        });

        // Insert complete destination atomically
        {
            let mut dests = self.destinations.write().unwrap();
            dests.insert(node_id, Destination {
                sender: forwarder_tx,
                grpc_client: None, // Will be set async above
                forwarder_handle: Some(handle),
            });
        }

        Ok(())
    }

    fn remove_peer(
        &self,
        node_id: u64
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Remove destination (forwarder will exit when sender is dropped)
        self.destinations.write().unwrap().remove(&node_id);

        // Remove from nodes map
        self.nodes.write().unwrap().remove(&node_id);

        // Note: receiver is consumed when cluster is created, can't remove
        // Forwarder will stop when shutdown_rx fires or sender is dropped

        Ok(())
    }

    fn get_discovered_voters(&self) -> Vec<u64> {
        self.discovered_voters.read().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::generic::message::CommandExecutor;
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::time::{timeout, Duration};
    use tokio::sync::Mutex as TokioMutex;

    #[derive(Clone, Debug, Serialize, Deserialize)]
    enum TestCommand {
        Noop,
        TestMessage(String),
    }

    #[derive(Default, Clone, Debug)]
    struct TestExecutor {
        message_received: Arc<AtomicBool>,
    }

    impl CommandExecutor for TestExecutor {
        type Command = TestCommand;

        fn apply_with_index(&self, command: &Self::Command, _logger: &slog::Logger, _log_index: u64) -> Result<(), Box<dyn std::error::Error>> {
            if matches!(command, TestCommand::TestMessage(_)) {
                self.message_received.store(true, Ordering::SeqCst);
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_grpc_transport_creation() {
        let nodes = vec![
            NodeConfig { node_id: 1, address: "127.0.0.1:5001".to_string() },
            NodeConfig { node_id: 2, address: "127.0.0.1:5002".to_string() },
        ];

        let transport = GrpcClusterTransport::new(nodes);

        // Verify we can get node addresses
        assert_eq!(
            transport.get_node_address(1).await,
            Some("127.0.0.1:5001".to_string())
        );
        assert_eq!(
            transport.get_node_address(2).await,
            Some("127.0.0.1:5002".to_string())
        );
    }

    #[tokio::test]
    async fn test_add_remove_node() {
        let transport = GrpcClusterTransport::new(vec![]);

        // Add a node
        let node = NodeConfig { node_id: 1, address: "127.0.0.1:5001".to_string() };
        transport.add_node(node).await.expect("Should add node");

        assert_eq!(
            transport.get_node_address(1).await,
            Some("127.0.0.1:5001".to_string())
        );

        // Remove the node
        transport.remove_node(1).await.expect("Should remove node");
        assert_eq!(transport.get_node_address(1).await, None);
    }

    #[tokio::test]
    async fn test_grpc_client_connect() {
        // Find a free port
        let port = port_check::free_local_port().expect("Should find free port");
        let addr = format!("127.0.0.1:{}", port);

        println!("Testing client connection with address: {}", addr);

        // Start a simple test server on the port
        use tonic::transport::Server;
        use crate::grpc::server::raft_proto::{
            raft_service_server::{RaftService, RaftServiceServer},
            GenericMessage, MessageResponse,
        };
        use tonic::{Request, Response, Status};

        struct TestService {
            addr: String,
            received_command: Arc<TokioMutex<Option<TestCommand>>>,
        }

        #[tonic::async_trait]
        impl RaftService for TestService {
            async fn send_message(
                &self,
                request: Request<GenericMessage>,
            ) -> Result<Response<MessageResponse>, Status> {
                let proto_msg = request.into_inner();

                // Convert directly from protobuf to Message
                let message = Message::<TestCommand>::from_protobuf(proto_msg)
                    .map_err(|e| Status::invalid_argument(format!("Failed to deserialize: {}", e)))?;

                if let Message::Propose { command, .. } = message {
                    *self.received_command.lock().await = Some(command);
                }

                Ok(Response::new(MessageResponse {
                    success: true,
                    error: String::new(),
                }))
            }

            async fn discover(
                &self,
                _request: Request<crate::grpc::server::raft_proto::DiscoveryRequest>,
            ) -> Result<Response<crate::grpc::server::raft_proto::DiscoveryResponse>, Status> {
                Ok(Response::new(crate::grpc::server::raft_proto::DiscoveryResponse {
                    node_id: 1,
                    role: 0,
                    highest_known_node_id: 1,
                    address: self.addr.clone(),
                    voters: vec![1],
                    learners: vec![],
                }))
            }
        }

        let test_addr = addr.parse().expect("Should parse address");
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        // Shared storage for received command
        let received_command = Arc::new(TokioMutex::new(None));
        let received_command_clone = received_command.clone();

        // Start test server in background
        let addr_for_service = addr.clone();
        tokio::spawn(async move {
            Server::builder()
                .add_service(RaftServiceServer::new(TestService {
                    addr: addr_for_service,
                    received_command: received_command_clone,
                }))
                .serve_with_shutdown(test_addr, async {
                    shutdown_rx.await.ok();
                })
                .await
                .expect("Test server failed");
        });

        // Give server time to start
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Test connection with default channel builder
        let mut client = RaftClient::connect(addr.clone())
            .await
            .expect("Should connect to test server");

        // Create a test message with a command
        let test_command = TestCommand::TestMessage("Hello gRPC!".to_string());
        let message = Message::Propose {
            command: test_command.clone(),
            sync_id: None,
        };

        // Send message - serialize to protobuf first
        let proto_msg = message.to_protobuf().expect("Failed to serialize message");
        let result = timeout(
            Duration::from_secs(5),
            client.send_message(proto_msg)
        ).await;

        assert!(result.is_ok(), "Message send should not timeout");
        assert!(result.unwrap().is_ok(), "Message send should succeed");

        // Give server time to process the message
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Verify the received command payload
        let received = received_command.lock().await;
        assert!(received.is_some(), "Server should have received a command");

        let received_cmd = received.as_ref().unwrap();
        if let TestCommand::TestMessage(msg) = received_cmd {
            assert_eq!(msg, "Hello gRPC!", "Command payload should match");
            println!("Test completed successfully");
            println!("Verified command payload: {}", msg);
        } else {
            panic!("Expected TestMessage variant");
        }

        // Cleanup
        let _ = shutdown_tx.send(());
    }
}
