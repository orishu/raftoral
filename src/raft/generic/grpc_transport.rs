use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, RwLock};
use crate::raft::generic::message::{Message, CommandExecutor};
use crate::raft::generic::cluster::RaftCluster;
use crate::raft::generic::transport::ClusterTransport;
use crate::grpc::client::{RaftClient, ChannelBuilder, default_channel_builder};
use protobuf::Message as ProtobufMessage;

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
    grpc_clients: Arc<RwLock<HashMap<u64, Arc<Mutex<RaftClient>>>>>,
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
    /// use tonic::transport::Channel;
    ///
    /// let nodes = vec![
    ///     NodeConfig { node_id: 1, address: "127.0.0.1:5001".to_string() },
    ///     NodeConfig { node_id: 2, address: "127.0.0.1:5002".to_string() },
    /// ];
    ///
    /// let channel_builder = std::sync::Arc::new(|address: String| {
    ///     Box::pin(async move {
    ///         Channel::from_shared(format!("https://{}", address))?
    ///             .connect()
    ///             .await
    ///     }) as std::pin::Pin<Box<dyn std::future::Future<Output = _> + Send>>
    /// }) as crate::grpc::client::ChannelBuilder;
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
        let mut nodes = self.nodes.write().await;
        let mut senders = self.node_senders.write().await;
        let mut receivers = self.node_receivers.lock().await;

        // Create channel for the new node
        let (sender, receiver) = mpsc::unbounded_channel();

        senders.insert(node.node_id, sender);
        receivers.insert(node.node_id, receiver);
        nodes.insert(node.node_id, node);

        Ok(())
    }

    /// Remove a node from the transport configuration
    pub async fn remove_node(&self, node_id: u64) -> Result<(), Box<dyn std::error::Error>> {
        let mut nodes = self.nodes.write().await;
        let mut senders = self.node_senders.write().await;
        let mut receivers = self.node_receivers.lock().await;

        nodes.remove(&node_id);
        senders.remove(&node_id);
        receivers.remove(&node_id);

        Ok(())
    }

    /// Get the address for a specific node
    pub async fn get_node_address(&self, node_id: u64) -> Option<String> {
        let nodes = self.nodes.read().await;
        nodes.get(&node_id).map(|n| n.address.clone())
    }

    /// Get all node configurations
    pub async fn get_all_nodes(&self) -> Vec<NodeConfig> {
        let nodes = self.nodes.read().await;
        nodes.values().cloned().collect()
    }

    /// Internal method to get a sender for a specific node
    pub async fn get_node_sender(&self, node_id: u64) -> Option<mpsc::UnboundedSender<Message<E::Command>>> {
        let senders = self.node_senders.read().await;
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
                        // Only forward Raft protocol messages over gRPC
                        if let Message::Raft(raft_msg) = message {
                            // Get the gRPC client for this node
                            let clients = grpc_clients.read().await;
                            if let Some(client) = clients.get(&target_node_id) {
                                let mut client = client.lock().await;

                                // Serialize the Raft message
                                match raft_msg.write_to_bytes() {
                                    Ok(bytes) => {
                                        let grpc_msg = crate::grpc::server::raft_proto::RaftMessage {
                                            raft_message: bytes,
                                            command: None, // Raft messages don't carry commands
                                        };

                                        if let Err(e) = client.send_raft_message(grpc_msg).await {
                                            eprintln!("Failed to send message to node {}: {}", target_node_id, e);
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!("Failed to serialize Raft message: {}", e);
                                    }
                                }
                            }
                        }
                        // Other message types (Propose, Campaign, etc.) are local-only
                        // They should never be sent to remote nodes
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
        &self,
        node_id: u64,
    ) -> Result<Arc<RaftCluster<E>>, Box<dyn std::error::Error>> {
        // Create a new executor instance
        let executor = E::default();

        // Extract the receiver for this node
        let receiver = {
            let mut receivers = self.node_receivers.lock().await;
            receivers.remove(&node_id)
                .ok_or_else(|| format!("Node {} receiver already claimed or doesn't exist", node_id))?
        };

        // Get shutdown receiver
        let shutdown_tx = self.shutdown_tx.lock().await;
        let shutdown_rx = shutdown_tx.as_ref()
            .ok_or("Transport not started - call start() before create_cluster()")?
            .subscribe();
        drop(shutdown_tx);

        // Build peer senders - spawn forwarders for remote peers
        let senders = self.node_senders.read().await;
        let mut peer_senders = HashMap::new();

        for (&peer_id, _sender) in senders.iter() {
            if peer_id != node_id {
                // For remote peers, create a forwarder channel
                let (forwarder_tx, forwarder_rx) = mpsc::unbounded_channel();
                peer_senders.insert(peer_id, forwarder_tx);

                // Spawn forwarder task for this peer
                let handle = self.spawn_peer_forwarder(peer_id, forwarder_rx, shutdown_rx.resubscribe());

                // Store the handle
                let mut handles = self.forwarder_handles.lock().await;
                handles.push(handle);
            }
        }

        // Get the sender for this node (for cluster's propose methods to use)
        let self_sender = senders.get(&node_id)
            .ok_or_else(|| format!("Node {} sender not found", node_id))?
            .clone();

        drop(senders); // Release the read lock

        // Create the cluster with the receiver, peer senders, and self sender
        let cluster = RaftCluster::new_with_transport(
            node_id,
            receiver,
            peer_senders,
            self_sender,
            executor,
        ).await?;

        Ok(Arc::new(cluster))
    }

    async fn node_ids(&self) -> Vec<u64> {
        let nodes = self.nodes.read().await;
        nodes.keys().copied().collect()
    }

    async fn start(&self) -> Result<(), Box<dyn std::error::Error>> {
        // Create shutdown channel
        let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);
        *self.shutdown_tx.lock().await = Some(shutdown_tx.clone());

        // Get all nodes
        let nodes = self.nodes.read().await;
        let node_list: Vec<NodeConfig> = nodes.values().cloned().collect();
        drop(nodes);

        // Create gRPC clients for all nodes using the configured channel builder
        let mut clients = self.grpc_clients.write().await;
        for node in &node_list {
            match RaftClient::connect_with_channel_builder(
                node.address.clone(),
                self.channel_builder.clone()
            ).await {
                Ok(client) => {
                    clients.insert(node.node_id, Arc::new(Mutex::new(client)));
                }
                Err(e) => {
                    eprintln!("Warning: Failed to connect to node {}: {}", node.node_id, e);
                    // Continue anyway - the node might not be up yet
                }
            }
        }
        drop(clients);

        Ok(())
    }

    async fn shutdown(&self) -> Result<(), Box<dyn std::error::Error>> {
        let shutdown_tx = self.shutdown_tx.lock().await;
        if let Some(tx) = shutdown_tx.as_ref() {
            let _ = tx.send(());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            RaftMessage, MessageResponse,
        };
        use tonic::{Request, Response, Status};

        struct TestService {
            addr: String,
            received_message: Arc<TokioMutex<Option<raft::prelude::Message>>>,
        }

        #[tonic::async_trait]
        impl RaftService for TestService {
            async fn send_message(
                &self,
                request: Request<RaftMessage>,
            ) -> Result<Response<MessageResponse>, Status> {
                let raft_msg = request.into_inner();

                // Deserialize and store the Raft message
                if let Ok(msg) = protobuf::Message::parse_from_bytes(&raft_msg.raft_message) {
                    *self.received_message.lock().await = Some(msg);
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

        // Shared storage for received message
        let received_message = Arc::new(TokioMutex::new(None));
        let received_message_clone = received_message.clone();

        // Start test server in background
        let addr_for_service = addr.clone();
        tokio::spawn(async move {
            Server::builder()
                .add_service(RaftServiceServer::new(TestService {
                    addr: addr_for_service,
                    received_message: received_message_clone,
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

        // Create a test Raft message
        let mut raft_msg = raft::prelude::Message::default();
        raft_msg.set_msg_type(raft::prelude::MessageType::MsgHeartbeat);
        raft_msg.set_to(1);
        raft_msg.set_from(2);

        let msg_bytes = protobuf::Message::write_to_bytes(&raft_msg)
            .expect("Should serialize message");

        let grpc_msg = RaftMessage {
            raft_message: msg_bytes,
            command: None,
        };

        // Send message
        let result = timeout(
            Duration::from_secs(5),
            client.send_raft_message(grpc_msg)
        ).await;

        assert!(result.is_ok(), "Message send should not timeout");
        assert!(result.unwrap().is_ok(), "Message send should succeed");

        // Give server time to process the message
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Verify the received message payload
        let received = received_message.lock().await;
        assert!(received.is_some(), "Server should have received a message");

        let received_msg = received.as_ref().unwrap();
        assert_eq!(received_msg.get_msg_type(), raft::prelude::MessageType::MsgHeartbeat,
                   "Message type should be MsgHeartbeat");
        assert_eq!(received_msg.get_to(), 1, "Message 'to' field should be 1");
        assert_eq!(received_msg.get_from(), 2, "Message 'from' field should be 2");

        println!("Test completed successfully");
        println!("Verified message: type={:?}, from={}, to={}",
                 received_msg.get_msg_type(),
                 received_msg.get_from(),
                 received_msg.get_to());

        // Cleanup
        let _ = shutdown_tx.send(());
    }
}
