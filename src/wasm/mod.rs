//! WASM Entry Points and Public API
//!
//! This module provides WebAssembly bindings for Raftoral, enabling it to run
//! in JavaScript environments (Node.js and browsers).
//!
//! Note: For Milestone 2, workflows must be pre-registered in Rust code.
//! Dynamic TypeScript workflow registration comes in Milestone 4.

use wasm_bindgen::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

// Re-export types that will be used in the public API
pub use crate::full_node::FullNode;
pub use crate::workflow::{WorkflowRegistry, WorkflowError};

/// WASM-compatible Raftoral node
///
/// This wraps the core FullNode and provides a JavaScript-friendly API.
/// Note: In browser environments, nodes cannot listen on network ports,
/// so this is primarily for Node.js WASM usage in Milestone 2.
#[wasm_bindgen]
pub struct WasmRaftoralNode {
    /// The inner FullNode (stored as Option for initialization control)
    #[wasm_bindgen(skip)]
    inner: Option<Arc<FullNode>>,

    /// Node ID
    node_id: u64,

    /// Listen address
    address: String,

    /// Bootstrap mode flag
    bootstrap: bool,
}

#[wasm_bindgen]
impl WasmRaftoralNode {
    /// Create a new WASM Raftoral node
    ///
    /// # Arguments
    /// * `node_id` - Unique identifier for this node (use 1 for first node)
    /// * `listen_address` - Address to listen on (e.g., "127.0.0.1:8080")
    /// * `bootstrap` - Whether to bootstrap a new cluster (true for first node)
    ///
    /// # Returns
    /// A new WasmRaftoralNode instance
    ///
    /// # Example (JavaScript)
    /// ```javascript
    /// const node = new WasmRaftoralNode(1, "127.0.0.1:8080", true);
    /// await node.start();
    /// ```
    #[wasm_bindgen(constructor)]
    pub fn new(node_id: u64, listen_address: String, bootstrap: bool) -> WasmRaftoralNode {
        // Set panic hook for better error messages in browser console
        #[cfg(feature = "console_error_panic_hook")]
        console_error_panic_hook::set_once();

        WasmRaftoralNode {
            inner: None,
            node_id,
            address: listen_address,
            bootstrap,
        }
    }

    /// Start the node
    ///
    /// This initializes the Raft node, starts the HTTP server, and begins
    /// participating in the cluster.
    ///
    /// # Returns
    /// Promise that resolves when the node is started, or rejects with an error
    pub async fn start(&mut self) -> Result<(), JsValue> {
        // Create a basic logger for WASM
        let logger = create_wasm_logger();

        // In WASM, we always use in-memory storage (no RocksDB)
        let storage_path = None;

        // Create FullNode with HTTP transport
        let node = if self.bootstrap {
            FullNode::new_with_http(self.node_id, self.address.clone(), storage_path, logger)
                .await
                .map_err(|e| JsValue::from_str(&format!("Failed to create node: {}", e)))?
        } else {
            // For joining mode, we'd need seed addresses
            // This is a limitation for Milestone 2 - we'll enhance this later
            return Err(JsValue::from_str(
                "Join mode not yet supported in WASM. Use bootstrap mode for now.",
            ));
        };

        self.inner = Some(Arc::new(node));
        Ok(())
    }

    /// Get the node ID
    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    /// Execute a pre-registered workflow
    ///
    /// # Arguments
    /// * `workflow_name` - Name of the workflow to execute
    /// * `input_json` - JSON-serialized input data
    ///
    /// # Returns
    /// Promise that resolves with JSON-serialized output, or rejects with an error
    ///
    /// Note: Workflows must be pre-registered in Rust code for Milestone 2.
    /// Dynamic workflow registration from TypeScript comes in Milestone 4.
    pub async fn execute_workflow(
        &self,
        workflow_name: String,
        input_json: String,
    ) -> Result<String, JsValue> {
        let node = self
            .inner
            .as_ref()
            .ok_or_else(|| JsValue::from_str("Node not started. Call start() first."))?;

        // For Milestone 2, workflow execution is simplified
        // We'll need to get the workflow runtime and execute the workflow
        // This is a placeholder that will be implemented more fully
        Err(JsValue::from_str(
            "Workflow execution not yet fully implemented in WASM. Coming in next iteration.",
        ))
    }

    /// Shutdown the node gracefully
    ///
    /// This stops the Raft node and HTTP server.
    pub async fn shutdown(&self) {
        if let Some(node) = &self.inner {
            node.shutdown().await;
        }
    }

    /// Get the listen address
    pub fn address(&self) -> String {
        self.address.clone()
    }
}

/// Create a logger suitable for WASM environment
///
/// In WASM, we use console logging instead of terminal output.
fn create_wasm_logger() -> slog::Logger {
    use slog::{o, Drain, Logger};
    use std::sync::Mutex;

    // Simple discard drain for now - we can enhance this later with console logging
    // For Milestone 2, just getting compilation working is the priority
    let drain = slog::Discard;
    Logger::root(drain, o!())
}

/// Initialize the WASM module
///
/// This is called automatically when the WASM module is loaded.
/// It sets up panic hooks and other initialization.
#[wasm_bindgen(start)]
pub fn wasm_init() {
    // Set panic hook for better error messages
    #[cfg(feature = "console_error_panic_hook")]
    console_error_panic_hook::set_once();
}

// Helper types for serialization (will be used in later milestones)

/// Workflow execution request
#[derive(Serialize, Deserialize)]
pub struct WorkflowRequest {
    pub workflow_name: String,
    pub version: u32,
    pub input: serde_json::Value,
}

/// Workflow execution result
#[derive(Serialize, Deserialize)]
pub struct WorkflowResult {
    pub success: bool,
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
}
