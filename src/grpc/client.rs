use tonic::transport::Channel;
use std::sync::Arc;
use crate::grpc::server::raft_proto::{
    raft_service_client::RaftServiceClient, RaftMessage,
};

/// Type alias for channel creation function
pub type ChannelBuilder = Arc<dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Channel, Box<dyn std::error::Error + Send + Sync>>> + Send>> + Send + Sync>;

/// gRPC client for sending Raft messages to peer nodes
pub struct RaftClient {
    client: RaftServiceClient<Channel>,
}

impl RaftClient {
    /// Create a new gRPC client connected to the specified address
    pub async fn connect(address: String) -> Result<Self, Box<dyn std::error::Error>> {
        Self::connect_with_channel_builder(address, default_channel_builder()).await
    }

    /// Create a new gRPC client with a custom channel builder
    ///
    /// This allows customization of the gRPC channel with features like:
    /// - TLS/SSL configuration
    /// - Interceptors for authentication
    /// - Custom timeout settings
    /// - Compression options
    ///
    /// # Example
    /// ```no_run
    /// use raftoral::grpc::client::RaftClient;
    /// use tonic::transport::Channel;
    ///
    /// let channel_builder = Arc::new(|address: String| {
    ///     Box::pin(async move {
    ///         Channel::from_shared(format!("https://{}", address))?
    ///             .tls_config(tonic::transport::ClientTlsConfig::new())?
    ///             .connect()
    ///             .await
    ///     }) as std::pin::Pin<Box<dyn std::future::Future<Output = _> + Send>>
    /// }) as ChannelBuilder;
    ///
    /// let client = RaftClient::connect_with_channel_builder(
    ///     "127.0.0.1:5001".to_string(),
    ///     channel_builder
    /// ).await?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub async fn connect_with_channel_builder(
        address: String,
        channel_builder: ChannelBuilder,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let channel = channel_builder(address).await
            .map_err(|e| e as Box<dyn std::error::Error>)?;
        let client = RaftServiceClient::new(channel);
        Ok(Self { client })
    }

    /// Send a Raft message to the peer node
    pub async fn send_raft_message(
        &mut self,
        raft_msg: RaftMessage,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let response = self.client.send_message(raft_msg).await?;
        let msg_response = response.into_inner();

        if !msg_response.success {
            return Err(msg_response.error.into());
        }

        Ok(())
    }
}

/// Default channel builder for insecure HTTP connections
pub fn default_channel_builder() -> ChannelBuilder {
    Arc::new(|address: String| {
        Box::pin(async move {
            let uri = format!("http://{}", address);
            let endpoint = Channel::from_shared(uri)
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
            endpoint.connect().await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
        })
    })
}
