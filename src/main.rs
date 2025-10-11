use clap::Parser;
use log::{info, error};
use raftoral::runtime::{RaftoralConfig, RaftoralGrpcRuntime};
use raftoral::workflow::{WorkflowContext, WorkflowError};
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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logger
    env_logger::init();

    let args = Args::parse();

    // Build configuration from CLI args
    let mut config = if args.bootstrap {
        RaftoralConfig::bootstrap(args.listen, args.node_id)
    } else {
        RaftoralConfig::join(args.listen, args.peers)
    };

    if let Some(advertise) = args.advertise {
        config = config.with_advertise_address(advertise);
    }

    if !args.bootstrap {
        if let Some(node_id) = args.node_id {
            config = config.with_node_id(node_id);
        }
    }

    // Start the runtime
    let runtime = RaftoralGrpcRuntime::start(config).await?;

    // Register ping/pong workflow
    // Input: "ping" -> Output: "pong"
    // Any other input returns an error
    let ping_pong_fn = |input: String, _context: WorkflowContext| async move {
        if input == "ping" {
            Ok::<String, WorkflowError>("pong".to_string())
        } else {
            Err(WorkflowError::ClusterError(format!(
                "Expected 'ping', got '{}'",
                input
            )))
        }
    };

    runtime
        .workflow_runtime()
        .register_workflow_closure("ping_pong", 1, ping_pong_fn)
        .expect("Failed to register ping_pong workflow");
    info!("Registered ping_pong workflow (v1)");

    info!("Press Ctrl+C to shutdown gracefully");

    // Example: Execute ping/pong workflow to test the system
    // Note: This works from any node - start_workflow automatically routes to the leader
    info!("Testing ping/pong workflow...");

    match runtime
        .workflow_runtime()
        .start_workflow::<String, String>("ping_pong", 1, "ping".to_string())
        .await
    {
        Ok(workflow_run) => {
            info!("Started ping/pong workflow: {}", workflow_run.workflow_id());

            match workflow_run.wait_for_completion().await {
                Ok(output) => {
                    info!("Workflow completed: got '{}'", output);
                }
                Err(e) => {
                    error!("Workflow failed: {}", e);
                }
            }
        }
        Err(e) => {
            error!("Failed to start workflow: {}", e);
        }
    }

    // Wait for shutdown signal
    signal::ctrl_c().await?;

    // Gracefully shutdown
    runtime.shutdown().await?;

    Ok(())
}
