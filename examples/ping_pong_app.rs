//! Ping Pong Example Application
//!
//! This example demonstrates how to create an application that connects to Raftoral
//! in sidecar mode and handles workflow execution requests.
//!
//! The ping_pong workflow:
//! - Receives input text
//! - If input is "ping", responds with "pong"
//! - If input is "pong", responds with "ping"
//! - Otherwise, echoes the input back
//!
//! Usage:
//!   cargo run --example ping_pong_app -- [SIDECAR_URL]
//!
//! Example:
//!   cargo run --example ping_pong_app -- http://127.0.0.1:9001

use raftoral_client::{RaftoralClient, proto::ExecuteWorkflowRequest};
use slog::{Drain, Logger, info, o};
use std::env;

/// Create a logger for the example
fn create_logger() -> Logger {
    let decorator = slog_term::PlainDecorator::new(std::io::stdout());
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    Logger::root(drain, o!())
}

/// Ping-pong workflow handler
///
/// This is the core business logic that executes when a workflow is triggered.
/// It runs on ALL nodes in the cluster (both owner and followers) to maintain
/// fault tolerance.
async fn ping_pong_handler(req: ExecuteWorkflowRequest) -> raftoral_client::error::Result<Vec<u8>> {
    // Parse input
    let input_text = String::from_utf8_lossy(&req.input);

    info!(create_logger(), "Ping-pong workflow executing";
        "workflow_id" => &req.workflow_id,
        "is_owner" => req.is_owner,
        "input" => &input_text.to_string()
    );

    // Ping-pong logic
    let output = match input_text.trim() {
        "ping" => "pong",
        "pong" => "ping",
        other => other,
    };

    info!(create_logger(), "Ping-pong workflow result";
        "workflow_id" => &req.workflow_id,
        "output" => output
    );

    Ok(output.as_bytes().to_vec())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let logger = create_logger();

    // Get sidecar URL from command line or use default
    let args: Vec<String> = env::args().collect();
    let sidecar_url = if args.len() > 1 {
        &args[1]
    } else {
        "http://127.0.0.1:9001"
    };

    info!(logger, "Ping-pong application starting";
        "sidecar_url" => sidecar_url
    );

    // Connect to Raftoral sidecar
    let client = RaftoralClient::connect(sidecar_url, logger.clone()).await?;

    info!(logger, "Connected to Raftoral sidecar");

    // Register ping_pong workflow handler (version 1)
    client.register_handler("ping_pong", 1, ping_pong_handler).await;

    info!(logger, "Registered ping_pong workflow handler (version 1)");
    info!(logger, "Application ready - waiting for workflow execution requests...");
    info!(logger, "Press Ctrl+C to exit");

    // Keep the application running
    // The client will automatically handle incoming workflow requests in the background
    tokio::signal::ctrl_c().await?;

    info!(logger, "Shutting down ping-pong application");

    Ok(())
}
