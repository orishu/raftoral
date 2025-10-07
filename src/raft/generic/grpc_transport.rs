use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::mpsc;
use crate::raft::generic::message::{Message, CommandExecutor};
use crate::raft::generic::cluster::RaftCluster;
use crate::raft::generic::transport::ClusterTransport;
use crate::grpc::client::{RaftClient, ChannelBuilder, default_channel_builder};

/// Configuration for a node in the gRPC cluster
#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub node_id: u64,
    pub address: String,  // host:port
}

/// Generic gRPC-based transport for distributed Raft clusters
///
/// This transport enables Raft nodes to communicate over network via gRPC.
/// It maintains a mapping of node IDs to network addresses and provides
/// the infrastructure for sending Raft messages between nodes.
///
/// The transport is generic over the CommandExecutor type, but the actual
/// gRPC protocol is defined for specific command types (e.g., WorkflowCommand).
pub struct GrpcClusterTransport<E: CommandExecutor> {
    /// Node configurations (node_id -> address)
    nodes: Arc<RwLock<HashMap<u64, NodeConfig>>>,
    /// Receivers for each node (held until create_cluster is called)
    node_receivers: Arc<Mutex<HashMap<u64, mpsc::UnboundedReceiver<Message<E::Command>>>>>,
    /// Senders for each node (node_id -> sender to that node's receiver)
    node_senders: Arc<RwLock<HashMap<u64, mpsc::UnboundedSender<Message<E::Command>>>>>,
    /// gRPC clients for sending to remote nodes (node_id -> client)
    grpc_clients: Arc<RwLock<HashMap<u64, Arc<tokio::sync::Mutex<RaftClient>>>>>,
    /// Background task handles for message forwarding
    forwarder_handles: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    /// Shutdown signal
    shutdown_tx: Arc<Mutex<Option<tokio::sync::broadcast::Sender<()>>>>,
    /// Custom channel builder for gRPC connections
    channel_builder: ChannelBuilder,
}

impl<E: CommandExecutor> GrpcClusterTransport<E> {
    /// Create a new gRPC transport with the given node configurations
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
        let mut node_senders = HashMap::new();
        let mut node_receivers_map = HashMap::new();

        // Create channels for each node
        for node in nodes {
            let (sender, receiver) = mpsc::unbounded_channel();
            node_senders.insert(node.node_id, sender);
            node_receivers_map.insert(node.node_id, receiver);
            node_map.insert(node.node_id, node);
        }

        GrpcClusterTransport {
            nodes: Arc::new(RwLock::new(node_map)),
            node_receivers: Arc::new(Mutex::new(node_receivers_map)),
            node_senders: Arc::new(RwLock::new(node_senders)),
            grpc_clients: Arc::new(RwLock::new(HashMap::new())),
            forwarder_handles: Arc::new(Mutex::new(Vec::new())),
            shutdown_tx: Arc::new(Mutex::new(None)),
            channel_builder,
        }
    }

    /// Add a new node to the transport configuration
    pub async fn add_node(&self, node: NodeConfig) -> Result<(), Box<dyn std::error::Error>> {
        let mut nodes = self.nodes.write().unwrap();
        let mut senders = self.node_senders.write().unwrap();
        let mut receivers = self.node_receivers.lock().unwrap();

        // Create channel for the new node
        let (sender, receiver) = mpsc::unbounded_channel();

        senders.insert(node.node_id, sender);
        receivers.insert(node.node_id, receiver);
        nodes.insert(node.node_id, node);

        Ok(())
    }

    /// Remove a node from the transport configuration
    pub async fn remove_node(&self, node_id: u64) -> Result<(), Box<dyn std::error::Error>> {
        let mut nodes = self.nodes.write().unwrap();
        let mut senders = self.node_senders.write().unwrap();
        let mut receivers = self.node_receivers.lock().unwrap();

        nodes.remove(&node_id);
        senders.remove(&node_id);
        receivers.remove(&node_id);

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

    /// Internal method to get a sender for a specific node
    pub async fn get_node_sender(&self, node_id: u64) -> Option<mpsc::UnboundedSender<Message<E::Command>>> {
        let senders = self.node_senders.read().unwrap();
        senders.get(&node_id).cloned()
    }

    /// Spawn a background task that forwards messages to a remote peer via gRPC
    fn spawn_peer_forwarder(
        &self,
        target_node_id: u64,
        mut forwarder_rx: mpsc::UnboundedReceiver<Message<E::Command>>,
        mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
    ) -> tokio::task::JoinHandle<()> {
        let grpc_clients = self.grpc_clients.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(message) = forwarder_rx.recv() => {
                        // Get the gRPC client for this node
                        let client_arc = {
                            let clients = grpc_clients.read().unwrap();
                            clients.get(&target_node_id).cloned()
                        };

                        if let Some(client) = client_arc {
                            let mut client = client.lock().await;

                            // Simply forward the message via gRPC - completely generic!
                            if let Err(e) = client.send_message(&message).await {
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
}

impl<E: CommandExecutor + Default + 'static> ClusterTransport<E> for GrpcClusterTransport<E> {
    async fn create_cluster(
        self: &Arc<Self>,
        node_id: u64,
    ) -> Result<Arc<RaftCluster<E>>, Box<dyn std::error::Error>> {
        // Create a new executor instance
        let executor = E::default();

        // Extract the receiver for this node
        let receiver = {
            let mut receivers = self.node_receivers.lock().unwrap();
            receivers.remove(&node_id)
                .ok_or_else(|| format!("Node {} receiver already claimed or doesn't exist", node_id))?
        };

        // Get shutdown receiver
        let shutdown_rx = {
            let shutdown_tx = self.shutdown_tx.lock().unwrap();
            shutdown_tx.as_ref()
                .ok_or("Transport not started - call start() before create_cluster()")?
                .subscribe()
        };

        // Create forwarders for outgoing messages to remote peers
        // Each forwarder monitors the shared node_senders channel for a specific peer
        // and forwards Raft messages via gRPC to that peer
        let peer_ids: Vec<u64> = {
            let senders = self.node_senders.read().unwrap();
            senders.keys()
                .filter(|&&id| id != node_id)
                .copied()
                .collect()
        };

        let mut forwarder_handles_vec = Vec::new();
        for peer_id in peer_ids {
            // Create a channel that will replace the direct sender in node_senders
            // The forwarder reads from this channel and sends via gRPC
            let (forwarder_tx, forwarder_rx) = mpsc::unbounded_channel();

            // Replace the direct sender with the forwarder sender in the shared HashMap
            // This way when RaftNode sends to this peer, it goes through the forwarder
            {
                let mut senders_mut = self.node_senders.write().unwrap();
                senders_mut.insert(peer_id, forwarder_tx);
            }

            // Spawn forwarder task for this peer
            let handle = self.spawn_peer_forwarder(peer_id, forwarder_rx, shutdown_rx.resubscribe());
            forwarder_handles_vec.push(handle);
        }

        // Store all forwarder handles
        {
            let mut handles = self.forwarder_handles.lock().unwrap();
            handles.extend(forwarder_handles_vec);
        }

        // Create the cluster with shared node_senders Arc
        // Pass self as the transport updater (implements TransportUpdater trait)
        let cluster = RaftCluster::new_with_transport(
            node_id,
            receiver,
            self.node_senders.clone(), // Share the same HashMap instance
            executor,
            Some(Arc::clone(self) as Arc<dyn crate::raft::generic::transport::TransportUpdater>),
        ).await?;

        Ok(Arc::new(cluster))
    }

    async fn node_ids(&self) -> Vec<u64> {
        let nodes = self.nodes.read().unwrap();
        nodes.keys().copied().collect()
    }

    async fn start(&self) -> Result<(), Box<dyn std::error::Error>> {
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
                    let mut clients = self.grpc_clients.write().unwrap();
                    clients.insert(node.node_id, Arc::new(tokio::sync::Mutex::new(client)));
                }
                Err(e) => {
                    eprintln!("Warning: Failed to connect to node {}: {}", node.node_id, e);
                    // Continue anyway - the node might not be up yet
                }
            }
        }

        Ok(())
    }

    async fn shutdown(&self) -> Result<(), Box<dyn std::error::Error>> {
        let shutdown_tx = self.shutdown_tx.lock().unwrap();
        if let Some(tx) = shutdown_tx.as_ref() {
            let _ = tx.send(());
        }
        Ok(())
    }

    fn add_peer(&self, node_id: u64, address: String) -> Result<(), Box<dyn std::error::Error>> {
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

        // Create sender/receiver pair for this node
        let (tx, rx) = mpsc::unbounded_channel();

        {
            let mut senders = self.node_senders.write().unwrap();
            senders.insert(node_id, tx.clone());
        }

        {
            let mut receivers = self.node_receivers.lock().unwrap();
            receivers.insert(node_id, rx);
        }

        // Get shutdown receiver for forwarders
        let _shutdown_rx = {
            let shutdown_tx = self.shutdown_tx.lock().unwrap();
            shutdown_tx.as_ref()
                .ok_or("Transport not started")?
                .subscribe()
        };

        // Note: No need to notify existing nodes with AddPeerSender messages.
        // When nodes apply the ConfChange, they all call add_peer() on their local transport,
        // which shares the node_senders HashMap. Since node_senders is shared via Arc<RwLock>,
        // the new peer is immediately visible to all RaftNodes without explicit notification.

        // Spawn async task to create gRPC client
        let grpc_clients = self.grpc_clients.clone();
        let channel_builder = self.channel_builder.clone();
        tokio::spawn(async move {
            match RaftClient::connect_with_channel_builder(address.clone(), channel_builder).await {
                Ok(client) => {
                    let mut clients = grpc_clients.write().unwrap();
                    clients.insert(node_id, Arc::new(tokio::sync::Mutex::new(client)));
                }
                Err(e) => {
                    eprintln!("Warning: Failed to connect to node {}: {}", node_id, e);
                }
            }
        });

        Ok(())
    }

    fn remove_peer(&self, node_id: u64) -> Result<(), Box<dyn std::error::Error>> {
        // Remove from shared maps - all RaftNodes see the change immediately
        self.nodes.write().unwrap().remove(&node_id);
        self.node_senders.write().unwrap().remove(&node_id);
        self.grpc_clients.write().unwrap().remove(&node_id);
        // Note: receiver is consumed when cluster is created, can't remove
        // Forwarder will stop when shutdown_rx fires or sender is dropped

        Ok(())
    }
}

impl<E: CommandExecutor + Default + 'static> crate::raft::generic::transport::TransportUpdater for GrpcClusterTransport<E> {
    fn update_add_peer(&self, node_id: u64, address: String) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.add_peer(node_id, address).map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("{}", e).into()
        })
    }

    fn update_remove_peer(&self, node_id: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.remove_peer(node_id).map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("{}", e).into()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::generic::message::SerializableMessage;
    use serde::{Deserialize, Serialize};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::time::{timeout, Duration};
    use tokio::sync::Mutex as TokioMutex;

    #[derive(Clone, Debug, Serialize, Deserialize)]
    enum TestCommand {
        Noop,
        TestMessage(String),
    }

    #[derive(Default)]
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

        let transport = GrpcClusterTransport::<TestExecutor>::new(nodes);

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
        let transport = GrpcClusterTransport::<TestExecutor>::new(vec![]);

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
                let generic_msg = request.into_inner();

                // Deserialize to SerializableMessage first
                let serializable: SerializableMessage<TestCommand> = serde_json::from_slice(&generic_msg.serialized_message)
                    .map_err(|e| Status::invalid_argument(format!("Failed to deserialize: {}", e)))?;

                // Convert to Message and extract the command if it's a Propose
                let message = Message::<TestCommand>::from_serializable(serializable)
                    .map_err(|e| Status::invalid_argument(format!("Failed to convert: {}", e)))?;

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
            id: 1,
            callback: None,
            sync_callback: None,
            command: test_command.clone(),
        };

        // Send message
        let result = timeout(
            Duration::from_secs(5),
            client.send_message(&message)
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
