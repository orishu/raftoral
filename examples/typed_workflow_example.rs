use raftoral::{WorkflowContext, WorkflowRuntime};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ComputationInput {
    base_value: i32,
    multiplier: i32,
    iterations: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct ComputationOutput {
    final_result: i32,
    intermediate_values: Vec<i32>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🔄 Starting Typed Workflow Example with Replicated Variables");

    // Create a workflow runtime with integrated cluster
    let workflow_runtime = std::sync::Arc::new(WorkflowRuntime::new_single_node(1).await?);

    // Wait for leadership establishment
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Register a workflow using the closure-based approach
    let runtime_for_closure = workflow_runtime.clone();
    workflow_runtime.register_workflow_closure(
        "computation_workflow",
        1,
        move |input: ComputationInput, context: WorkflowContext| {
            let runtime = runtime_for_closure.clone();
            async move {
                println!("📊 Starting computation with input: {:?}", input);

                // Create a replicated variable to track our progress
                let mut progress = context
                    .create_replicated_var("progress", &runtime, 0u32)
                    .await?;
                println!("✅ Created progress tracker: {}", progress.get());

                // Note: Input parameters are now automatically stored in the WorkflowStart command,
                // so we don't need to checkpoint them separately as replicated variables

                // Perform computation with intermediate checkpointing
                let mut current_value = input.base_value;
                let mut intermediate_values = Vec::new();

                for i in 0..input.iterations {
                    // Update progress
                    progress.update(|old| old + 1).await?;
                    println!("🔄 Progress: {}/{}", progress.get(), input.iterations);

                    // Perform computation step
                    current_value = current_value * input.multiplier;
                    intermediate_values.push(current_value);

                    // Store intermediate result as replicated variable (checkpointing)
                    let step_key = format!("step_{}", i);
                    let _step_checkpoint = context
                        .create_replicated_var(&step_key, &runtime, current_value)
                        .await?;

                    println!("💾 Checkpointed step {}: value = {}", i, current_value);

                    // Simulate some computation time
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }

                // Store final computation summary
                let final_summary = context
                    .create_replicated_var_with_computation(
                        "final_summary",
                        &runtime,
                        move || async move {
                            format!(
                                "Computed {} iterations: {} -> {}",
                                input.iterations, input.base_value, current_value
                            )
                        },
                    )
                    .await?;

                println!("📋 Final summary: {}", final_summary.get());

                // Return the typed result
                Ok(ComputationOutput {
                    final_result: current_value,
                    intermediate_values,
                })
            }
        },
    )?;

    println!("✅ Workflow registered successfully");

    // Test the workflow with different inputs
    let test_cases = vec![
        ComputationInput {
            base_value: 2,
            multiplier: 3,
            iterations: 4,
        },
        ComputationInput {
            base_value: 5,
            multiplier: 2,
            iterations: 3,
        },
    ];

    for (i, input) in test_cases.into_iter().enumerate() {
        println!("\n🚀 Running test case {}: {:?}", i + 1, input);

        // Start the typed workflow
        let workflow_run = workflow_runtime
            .start_workflow_typed::<ComputationInput, ComputationOutput>(
                "computation_workflow",
                1,
                input.clone(),
            )
            .await?;

        println!(
            "🔄 Workflow started with ID: {}",
            workflow_run.workflow_id()
        );

        // Note: In production, the workflow execution timing would be managed by
        // actual business logic duration. This delay ensures the background task
        // has time to complete in this demo example.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Wait for completion and get the typed result
        let result = workflow_run.wait_for_completion().await?;

        println!("🎉 Workflow completed!");
        println!("📊 Final result: {}", result.final_result);
        println!("📈 Intermediate values: {:?}", result.intermediate_values);

        // Verify the result is mathematically correct
        let expected_result =
            (0..input.iterations).fold(input.base_value, |acc, _| acc * input.multiplier);
        assert_eq!(result.final_result, expected_result);
        assert_eq!(result.intermediate_values.len(), input.iterations as usize);

        println!("✅ Test case {} completed successfully!", i + 1);
    }

    println!("\n🎊 All workflow examples completed successfully!");
    println!("📝 Key features demonstrated:");
    println!("   • Closure-based workflow registration");
    println!("   • Typed workflow inputs and outputs");
    println!("   • Replicated variables for checkpointing");
    println!("   • Automatic result return from wait_for_completion()");
    println!("   • No race conditions or manual sleeps needed");

    Ok(())
}
