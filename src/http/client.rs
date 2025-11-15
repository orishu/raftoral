//! HTTP Client for sending Raft messages

use crate::grpc::proto::GenericMessage;
use crate::http::messages::JsonGenericMessage;
use crate::raft::generic::errors::TransportError;
use crate::raft::generic::MessageSender;
use slog::{debug, warn, Logger};

/// HTTP client for sending messages to peers
pub struct HttpMessageSender {
    client: reqwest::Client,
    logger: Logger,
}

impl HttpMessageSender {
    pub fn new(logger: Logger) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("Failed to create HTTP client");

        Self { client, logger }
    }
}

#[tonic::async_trait]
impl MessageSender for HttpMessageSender {
    async fn send(&self, address: &str, message: GenericMessage) -> Result<(), TransportError> {
        // Convert to JSON message
        let cluster_id = message.cluster_id;
        let json_msg: JsonGenericMessage = message.into();

        // Construct URL
        let url = format!("{}/raft/message", address);

        debug!(self.logger, "Sending HTTP POST request";
            "url" => &url,
            "cluster_id" => cluster_id
        );

        // Send POST request
        let response = self
            .client
            .post(&url)
            .json(&json_msg)
            .send()
            .await
            .map_err(|e| {
                warn!(self.logger, "HTTP request failed"; "error" => %e, "url" => &url);
                TransportError::Other(format!("HTTP request failed: {}", e))
            })?;

        if !response.status().is_success() {
            warn!(self.logger, "HTTP request returned error status";
                "status" => response.status().as_u16(),
                "url" => &url
            );
            return Err(TransportError::Other(format!(
                "HTTP request returned status {}",
                response.status()
            )));
        }

        debug!(self.logger, "HTTP message sent successfully"; "url" => &url);
        Ok(())
    }
}
