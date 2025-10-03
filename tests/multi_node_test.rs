use raftoral::raft::generic::transport::{ClusterTransport, InMemoryClusterTransport};
use raftoral::workflow::{WorkflowCommandExecutor, WorkflowRuntime, WorkflowContext};
use std::time::Duration;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FibonacciInput {
    n: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct FibonacciOutput {
    result: u64,
}

#[tokio::test]
async fn test_three_node_cluster_workflow_execution() {
    // Create a 3-node cluster transport
    let transport = InMemoryClusterTransport::<WorkflowCommandExecutor>::new(vec![1, 2, 3]);
    transport.start().await.expect("Transport should start");

    // Create clusters and runtimes for all 3 nodes
    let runtime1 = WorkflowRuntime::new(transport.create_cluster(1).await.expect("Should create cluster 1"));
    let runtime2 = WorkflowRuntime::new(transport.create_cluster(2).await.expect("Should create cluster 2"));
    let runtime3 = WorkflowRuntime::new(transport.create_cluster(3).await.expect("Should create cluster 3"));

    // Register the same fibonacci workflow on all 3 nodes
    let fibonacci_fn = |input: FibonacciInput, _context: WorkflowContext| async move {
        let mut a = 0u64;
        let mut b = 1u64;
        for _ in 2..=input.n {
            let temp = a + b;
            a = b;
            b = temp;
        }
        Ok(FibonacciOutput { result: b })
    };

    runtime1.register_workflow_closure("fibonacci", 1, fibonacci_fn.clone())
        .expect("Should register workflow on runtime 1");
    runtime2.register_workflow_closure("fibonacci", 1, fibonacci_fn.clone())
        .expect("Should register workflow on runtime 2");
    runtime3.register_workflow_closure("fibonacci", 1, fibonacci_fn)
        .expect("Should register workflow on runtime 3");

    // Give nodes time to elect a leader
    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;

    // Start workflow from any node (consensus handles leader forwarding)
    println!("Starting fibonacci workflow test...");
    let input = FibonacciInput { n: 10 };

    let start_result = tokio::time::timeout(
        Duration::from_secs(30),
        runtime1.start_workflow::<FibonacciInput, FibonacciOutput>("fibonacci", 1, input.clone())
    ).await;

    match start_result {
        Ok(Ok(workflow_run)) => {
            println!("✓ Workflow started successfully!");

            // Wait for workflow to complete with timeout
            let completion_result = tokio::time::timeout(
                Duration::from_secs(30),
                workflow_run.wait_for_completion()
            ).await;

            match completion_result {
                Ok(Ok(result)) => {
                    // Verify the fibonacci result
                    let expected_fib_10 = 55; // Fibonacci(10) = 55
                    assert_eq!(result.result, expected_fib_10, "Fibonacci(10) should be 55");

                    println!("✓ Multi-node workflow execution successful!");
                    println!("  Workflow executed across 3 nodes with result: {}", result.result);
                },
                Ok(Err(e)) => {
                    panic!("Workflow execution failed: {:?}", e);
                },
                Err(_) => {
                    panic!("Workflow completion timed out after 30 seconds");
                }
            }
        },
        Ok(Err(e)) => {
            panic!("Workflow start failed: {:?}", e);
        },
        Err(_) => {
            panic!("Workflow start timed out after 30 seconds");
        }
    }
}

#[tokio::test]
async fn test_three_node_cluster_basic_consensus() {
    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct SimpleInput {}

    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct SimpleOutput {}

    // Create a 3-node cluster transport
    let transport = InMemoryClusterTransport::<WorkflowCommandExecutor>::new(vec![1, 2, 3]);
    transport.start().await.expect("Transport should start");

    // Create clusters and runtimes for all 3 nodes
    let runtime1 = WorkflowRuntime::new(transport.create_cluster(1).await.expect("Should create cluster 1"));
    let runtime2 = WorkflowRuntime::new(transport.create_cluster(2).await.expect("Should create cluster 2"));
    let runtime3 = WorkflowRuntime::new(transport.create_cluster(3).await.expect("Should create cluster 3"));

    // Register a simple workflow on all nodes
    let simple_fn = |_input: SimpleInput, _context: WorkflowContext| async move {
        Ok(SimpleOutput {})
    };

    runtime1.register_workflow_closure("simple", 1, simple_fn.clone())
        .expect("Should register workflow on runtime 1");
    runtime2.register_workflow_closure("simple", 1, simple_fn.clone())
        .expect("Should register workflow on runtime 2");
    runtime3.register_workflow_closure("simple", 1, simple_fn)
        .expect("Should register workflow on runtime 3");

    // Give nodes time to elect a leader
    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;

    // Debug: Check which node is leader
    println!("Node 1 is leader: {}", runtime1.cluster.is_leader().await);
    println!("Node 2 is leader: {}", runtime2.cluster.is_leader().await);
    println!("Node 3 is leader: {}", runtime3.cluster.is_leader().await);

    // Start workflow from node 1 (any node works - Raft handles leader routing)
    let input = SimpleInput {};

    let start_result = tokio::time::timeout(
        Duration::from_secs(30),
        runtime1.start_workflow::<SimpleInput, SimpleOutput>("simple", 1, input)
    ).await;

    match start_result {
        Ok(Ok(workflow_run)) => {
            println!("✓ Workflow started successfully!");

            // Wait a bit for consensus to propagate
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

            // Get workflow ID from the run
            let workflow_id = workflow_run.workflow_id();

            // Verify all nodes have the workflow in their state (either Running or Completed)
            let status1 = runtime1.cluster.executor.get_workflow_status(workflow_id);
            let status2 = runtime2.cluster.executor.get_workflow_status(workflow_id);
            let status3 = runtime3.cluster.executor.get_workflow_status(workflow_id);

            println!("Node 1 status: {:?}", status1);
            println!("Node 2 status: {:?}", status2);
            println!("Node 3 status: {:?}", status3);

            // All nodes should have received and applied the WorkflowStart command
            assert!(status1.is_some(), "Node 1 should have workflow status");
            assert!(status2.is_some(), "Node 2 should have workflow status");
            assert!(status3.is_some(), "Node 3 should have workflow status");

            println!("✓ Multi-node consensus test passed!");
            println!("  All 3 nodes synchronized workflow state via Raft consensus");
        },
        Ok(Err(e)) => {
            panic!("Workflow start failed with error: {:?}", e);
        },
        Err(_) => {
            panic!("Workflow start timed out after 30 seconds");
        }
    }
}

#[tokio::test]
async fn test_cluster_node_count() {
    let transport = InMemoryClusterTransport::<WorkflowCommandExecutor>::new(vec![1, 2, 3, 4, 5]);
    assert_eq!(transport.node_ids(), vec![1, 2, 3, 4, 5]);

    transport.start().await.expect("Transport should start");

    let cluster1 = transport.create_cluster(1).await
        .expect("Should create cluster 1");

    // Cluster should report correct node count (all nodes in the configuration)
    assert_eq!(cluster1.node_count(), 5);

    println!("✓ Cluster configuration test passed!");
}
