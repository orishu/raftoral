use clap::Parser;
use raftoral::checkpoint_compute;
use raftoral::full_node::FullNode;
use raftoral::workflow::WorkflowError;
use slog::{info, o, Drain};
use tokio::signal;

#[derive(Parser, Debug)]
#[command(name = "raftoral")]
#[command(about = "Raftoral distributed workflow orchestration", long_about = None)]
struct Args {
    /// Address to listen on for gRPC connections (e.g., 0.0.0.0:5001)
    #[arg(short = 'l', long)]
    listen: String,

    /// Advertised address for other nodes to connect to (e.g., 192.168.1.10:5001)
    /// If not specified, uses the listen address
    #[arg(short = 'a', long)]
    advertise: Option<String>,

    /// Node ID (optional, will be auto-assigned if joining existing cluster)
    #[arg(short, long)]
    node_id: Option<u64>,

    /// Addresses of peer nodes to discover (e.g., 192.168.1.10:5001,192.168.1.11:5001)
    #[arg(short, long, value_delimiter = ',')]
    peers: Vec<String>,

    /// Bootstrap a new cluster (use this for the first node)
    #[arg(short, long, default_value_t = false)]
    bootstrap: bool,

    /// Path for persistent storage (optional, uses in-memory storage if not provided)
    #[arg(short = 's', long)]
    storage_path: Option<String>,
}

// Workflow uses simple String input/output for compatibility with scripts

fn create_logger() -> slog::Logger {
    let decorator = slog_term::PlainDecorator::new(std::io::stdout());
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    slog::Logger::root(drain, o!())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let logger = create_logger();

    info!(logger, "Starting Raftoral node";
        "listen" => &args.listen,
        "bootstrap" => args.bootstrap
    );

    // Determine address (advertise takes precedence if provided)
    let address = args.advertise.as_ref().unwrap_or(&args.listen).clone();

    // Convert storage_path to PathBuf if provided
    let storage_path = args.storage_path.map(std::path::PathBuf::from);

    // Create FullNode based on mode
    let node = if args.bootstrap {
        let node_id = args.node_id.unwrap_or(1);
        info!(logger, "Bootstrap mode"; "node_id" => node_id, "storage_path" => ?storage_path);

        FullNode::new(node_id, address, storage_path, logger.clone()).await?
    } else {
        if args.peers.is_empty() {
            return Err("--peers is required when not in bootstrap mode".into());
        }

        info!(logger, "Join mode"; "peers" => ?args.peers, "storage_path" => ?storage_path);

        let node = FullNode::new_joining(address, args.peers, storage_path, logger.clone()).await?;

        info!(logger, "Node joined with ID"; "node_id" => node.node_id());
        node
    };

    info!(logger, "FullNode started"; "node_id" => node.node_id());

    // Register ping/pong workflow on shared registry
    // This is now done before waiting for execution cluster creation
    // All workflow runtimes share this registry, so workflows only need to be registered once
    // Input: "ping" (String) -> Output: "pong" (String)
    {
        let registry = node.workflow_registry();
        let mut registry_guard = registry.lock().await;
        registry_guard
            .register_closure(
                "ping_pong",
                1,
                |input: String, ctx| async move {
                    // Step 1: Store input in a variable after 1 second using checkpoint_compute
                    let input_clone = input.clone();
                    let stored_input = checkpoint_compute!(ctx, "stored_input", || async move {
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                        input_clone
                    });

                    // Step 2: Read it back
                    let read_value = stored_input.get().clone();

                    // Step 3: Use checkpoint_compute again to verify and wait another second
                    let read_value_clone = read_value.clone();
                    let result = checkpoint_compute!(ctx, "final_result", || async move {
                        // Verify it's still "ping"
                        if read_value_clone == "ping" {
                            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                            Ok("pong".to_string())
                        } else {
                            Err(WorkflowError::ClusterError(format!(
                                "Expected 'ping', got '{}'",
                                read_value_clone
                            )))
                        }
                    });

                    // Step 4: Return "pong" as the result
                    result.get().clone()
                },
            )
            .map_err(|e| format!("Failed to register ping_pong workflow: {}", e))?;
    }

    info!(logger, "Registered ping_pong workflow (v1) on shared registry");
    info!(logger, "Press Ctrl+C to shutdown gracefully");
    info!(logger, "Use gRPC calls to run workflows (see scripts/run_ping_pong.sh)");

    // Wait for shutdown signal
    signal::ctrl_c().await?;

    info!(logger, "Shutting down...");
    node.shutdown().await;

    Ok(())
}
