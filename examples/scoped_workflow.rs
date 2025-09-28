use std::time::Duration;
use raftoral::{WorkflowRuntime, ReplicatedVar};

async fn compute_fibonacci(n: u32) -> u32 {
    // Simulate some computation time
    tokio::time::sleep(Duration::from_millis(100)).await;

    match n {
        0 => 0,
        1 => 1,
        n => {
            let mut a = 0;
            let mut b = 1;
            for _ in 2..=n {
                let temp = a + b;
                a = b;
                b = temp;
            }
            b
        }
    }
}

async fn fibonacci_workflow(workflow_runtime: &WorkflowRuntime, n: u32) -> Result<u32, Box<dyn std::error::Error>> {
    let workflow_id = format!("fibonacci_{}", n);

    // This block demonstrates automatic cleanup - WorkflowRun will auto-end when dropped
    {
        let workflow_run = workflow_runtime.start(&workflow_id).await?;

        // Store the input parameter
        let input_var = ReplicatedVar::new("input", &workflow_run, 0u32);
        input_var.set(n).await?;

        // Compute the result
        let result = compute_fibonacci(n).await;

        // Store the result in a replicated variable
        let result_var = ReplicatedVar::new("result", &workflow_run, 0u32);
        let stored_result = result_var.set(result).await?;

        // Return the result (workflow ends here)
        return Ok(workflow_run.finish_with(stored_result).await?);
    }
    // WorkflowRun goes out of scope here and the workflow would be automatically ended
    // But we already called finish_with() so it's already ended
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create a workflow runtime with integrated cluster
    let workflow_runtime = WorkflowRuntime::new_single_node(1).await?;

    // Wait for leadership establishment
    tokio::time::sleep(Duration::from_millis(500)).await;

    println!("Computing Fibonacci numbers using fault-tolerant workflows...");

    // Compute multiple Fibonacci numbers using separate workflows
    for i in 5..=10 {
        let result = fibonacci_workflow(&workflow_runtime, i).await?;
        println!("fib({}) = {}", i, result);
    }

    // Demonstrate explicit workflow finish within scope
    {
        let workflow_run = workflow_runtime.start("test_early_exit").await?;

        let counter = ReplicatedVar::new("counter", &workflow_run, 0i32);
        let counter_value = counter.set(42).await?;
        println!("Set counter to: {}", counter_value);

        workflow_run.finish().await?;

        println!("Workflow finished explicitly (no auto-cleanup needed)...");
        // WorkflowRun is already finished, so Drop won't panic
    }

    println!("All workflows completed!");

    Ok(())
}