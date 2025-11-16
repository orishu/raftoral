//! WASM HTTP Client for sending Raft messages in browser/WASM environment

use crate::grpc::proto::GenericMessage;
use crate::http::messages::JsonGenericMessage;
use crate::raft::generic::errors::TransportError;
use crate::raft::generic::MessageSender;
use slog::{debug, warn, Logger};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Request, RequestInit, RequestMode, Response};

/// WASM HTTP client for sending messages to peers using browser fetch API
pub struct WasmHttpClient {
    logger: Logger,
}

impl WasmHttpClient {
    pub fn new(logger: Logger) -> Self {
        Self { logger }
    }
}

// WASM is single-threaded, so we use ?Send
#[async_trait::async_trait(?Send)]
impl MessageSender for WasmHttpClient {
    async fn send(&self, address: &str, message: GenericMessage) -> Result<(), TransportError> {
        // Convert to JSON message
        let cluster_id = message.cluster_id;
        let json_msg: JsonGenericMessage = message.into();

        // Construct URL
        let url = format!("{}/raft/message", address);

        debug!(self.logger, "Sending HTTP POST request via WASM fetch";
            "url" => &url,
            "cluster_id" => cluster_id
        );

        // Serialize message to JSON
        let json_body = serde_json::to_string(&json_msg).map_err(|e| {
            TransportError::Other(format!("Failed to serialize message: {}", e))
        })?;

        // Create request options
        let mut opts = RequestInit::new();
        opts.method("POST");
        opts.mode(RequestMode::Cors);

        // Set body
        let js_body = JsValue::from_str(&json_body);
        opts.body(Some(&js_body));

        // Create request
        let request = Request::new_with_str_and_init(&url, &opts).map_err(|e| {
            warn!(self.logger, "Failed to create HTTP request"; "error" => format!("{:?}", e), "url" => &url);
            TransportError::Other(format!("Failed to create request: {:?}", e))
        })?;

        // Set headers
        request
            .headers()
            .set("Content-Type", "application/json")
            .map_err(|e| {
                TransportError::Other(format!("Failed to set Content-Type header: {:?}", e))
            })?;

        // Get window object
        let window = web_sys::window().ok_or_else(|| {
            warn!(self.logger, "No window object available");
            TransportError::Other("No window object available".to_string())
        })?;

        // Make fetch request
        let response_value = JsFuture::from(window.fetch_with_request(&request))
            .await
            .map_err(|e| {
                warn!(self.logger, "HTTP fetch failed"; "error" => format!("{:?}", e), "url" => &url);
                TransportError::Other(format!("HTTP fetch failed: {:?}", e))
            })?;

        // Convert to Response
        let response: Response = response_value.dyn_into().map_err(|e| {
            TransportError::Other(format!("Response is not a Response object: {:?}", e))
        })?;

        // Check status
        if !response.ok() {
            warn!(self.logger, "HTTP request returned error status";
                "status" => response.status(),
                "url" => &url
            );
            return Err(TransportError::Other(format!(
                "HTTP request returned status {}",
                response.status()
            )));
        }

        debug!(self.logger, "HTTP message sent successfully via WASM"; "url" => &url);
        Ok(())
    }
}
