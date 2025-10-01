use std::time::Duration;
use raftoral::{WorkflowRuntime, WorkflowRun, ReplicatedVar};

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

async fn fibonacci_workflow(workflow_runtime: &std::sync::Arc<WorkflowRuntime>, n: u32) -> Result<u32, Box<dyn std::error::Error>> {
    let workflow_id = format!("fibonacci_{}", n);

    // This block demonstrates automatic cleanup - WorkflowRun will auto-end when dropped
    {
        let workflow_run = WorkflowRun::start(&workflow_id, workflow_runtime, n).await?;

        // Store the input parameter using direct value
        let _input_var = ReplicatedVar::with_value("input", &workflow_run, n).await?;

        // Compute and store the result using side effect computation
        let result_var = ReplicatedVar::with_computation(
            "result",
            &workflow_run,
            move || async move {
                // Compute the Fibonacci number (side effect)
                compute_fibonacci(n).await
            }
        ).await?;

        // Return the result (workflow ends here)
        let result = result_var.get();
        workflow_run.finish_with(result).await?;
        return Ok(result);
    }
    // WorkflowRun goes out of scope here and the workflow would be automatically ended
    // But we already called finish_with() so it's already ended
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create a workflow runtime with integrated cluster
    let workflow_runtime = std::sync::Arc::new(WorkflowRuntime::new_single_node(1).await?);

    // Wait for leadership establishment
    tokio::time::sleep(Duration::from_millis(500)).await;

    println!("Computing Fibonacci numbers using fault-tolerant workflows...");

    // Compute multiple Fibonacci numbers using separate workflows
    for i in 5..=10 {
        let result = fibonacci_workflow(&workflow_runtime, i).await?;
        println!("fib({}) = {}", i, result);
    }

    // Demonstrate explicit workflow finish with update functionality
    {
        let workflow_run = WorkflowRun::start("test_early_exit", &workflow_runtime, ()).await?;

        // Start with a counter using the new interface
        let mut counter = ReplicatedVar::with_value("counter", &workflow_run, 0i32).await?;
        println!("Initial counter: {}", counter.get());

        // Update the counter using the update method
        let counter_value = counter.update(|val| val + 42).await?;
        println!("Updated counter to: {}", counter_value);
        println!("Counter value via get(): {}", counter.get());

        workflow_run.finish_with(()).await?;

        println!("Workflow finished explicitly (no auto-cleanup needed)...");
        // WorkflowRun is already finished, so Drop won't panic
    }

    println!("All workflows completed!");

    Ok(())
}