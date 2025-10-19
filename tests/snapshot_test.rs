///! Integration test for Raft snapshotting
///!
///! This test verifies that:
///! 1. ManagementCommandExecutor can create snapshots
///! 2. Snapshots can be restored on a fresh executor
///! 3. All state is properly preserved (except TTL cache)

use raftoral::nodemanager::{
    ManagementCommandExecutor,
    ManagementCommand, CreateExecutionClusterData, WorkflowLifecycleData,
};
use raftoral::raft::generic::message::CommandExecutor;
use uuid::Uuid;
use slog::Drain;

fn create_test_logger() -> slog::Logger {
    let decorator = slog_term::PlainDecorator::new(slog_term::TestStdoutWriter);
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    slog::Logger::root(drain, slog::o!())
}

#[test]
fn test_snapshot_roundtrip() {
    let logger = create_test_logger();
    let executor = ManagementCommandExecutor::with_logger(logger);

    // Setup: Create a complex state with multiple clusters and workflows
    let cluster1 = Uuid::new_v4();
    let cluster2 = Uuid::new_v4();
    let workflow1 = Uuid::new_v4();
    let workflow2 = Uuid::new_v4();
    let workflow3 = Uuid::new_v4();

    // Create two execution clusters
    executor.apply(&ManagementCommand::CreateExecutionCluster(
        CreateExecutionClusterData {
            cluster_id: cluster1,
            initial_node_ids: vec![1, 2, 3],
        }
    )).expect("Failed to create cluster1");

    executor.apply(&ManagementCommand::CreateExecutionCluster(
        CreateExecutionClusterData {
            cluster_id: cluster2,
            initial_node_ids: vec![4, 5],
        }
    )).expect("Failed to create cluster2");

    // Start some workflows
    executor.apply(&ManagementCommand::ReportWorkflowStarted(
        WorkflowLifecycleData {
            workflow_id: workflow1,
            cluster_id: cluster1,
            workflow_type: "fibonacci".to_string(),
            version: 1,
            timestamp: 1000,
            result_json: None,
        }
    )).expect("Failed to start workflow1");

    executor.apply(&ManagementCommand::ReportWorkflowStarted(
        WorkflowLifecycleData {
            workflow_id: workflow2,
            cluster_id: cluster1,
            workflow_type: "factorial".to_string(),
            version: 1,
            timestamp: 2000,
            result_json: None,
        }
    )).expect("Failed to start workflow2");

    executor.apply(&ManagementCommand::ReportWorkflowStarted(
        WorkflowLifecycleData {
            workflow_id: workflow3,
            cluster_id: cluster2,
            workflow_type: "prime_check".to_string(),
            version: 2,
            timestamp: 3000,
            result_json: None,
        }
    )).expect("Failed to start workflow3");

    // Complete one workflow with result
    executor.apply(&ManagementCommand::ReportWorkflowEnded(
        WorkflowLifecycleData {
            workflow_id: workflow3,
            cluster_id: cluster2,
            workflow_type: "prime_check".to_string(),
            version: 2,
            timestamp: 4000,
            result_json: Some("{\"Ok\":true}".to_string()),
        }
    )).expect("Failed to end workflow3");

    // Capture state before snapshot
    let clusters_before = executor.get_all_clusters();
    let active_workflows_before = executor.get_total_active_workflows();

    println!("State before snapshot:");
    println!("  Clusters: {}", clusters_before.len());
    println!("  Active workflows: {}", active_workflows_before);
    println!("  Workflow1 location: {:?}", executor.get_workflow_location(&workflow1));
    println!("  Workflow2 location: {:?}", executor.get_workflow_location(&workflow2));
    println!("  Workflow3 location: {:?}", executor.get_workflow_location(&workflow3));
    println!("  Node 1 clusters: {:?}", executor.get_node_clusters(1));
    println!("  Node 4 clusters: {:?}", executor.get_node_clusters(4));

    // Create snapshot
    let snapshot_bytes = executor.create_snapshot(1000)
        .expect("Failed to create snapshot");

    println!("\nSnapshot created: {} bytes", snapshot_bytes.len());
    assert!(!snapshot_bytes.is_empty());
    assert!(snapshot_bytes.len() > 100); // Should be substantial

    // Create a fresh executor and restore
    let logger2 = create_test_logger();
    let executor2 = ManagementCommandExecutor::with_logger(logger2);

    // Verify fresh executor is empty
    assert_eq!(executor2.get_all_clusters().len(), 0);
    assert_eq!(executor2.get_total_active_workflows(), 0);

    // Restore from snapshot
    executor2.restore_from_snapshot(&snapshot_bytes)
        .expect("Failed to restore snapshot");

    println!("\nState after restore:");
    println!("  Clusters: {}", executor2.get_all_clusters().len());
    println!("  Active workflows: {}", executor2.get_total_active_workflows());

    // Verify all state was restored correctly
    assert_eq!(executor2.get_all_clusters().len(), 2);
    assert_eq!(executor2.get_total_active_workflows(), 2); // workflow3 is complete

    // Verify cluster details
    let cluster1_info = executor2.get_cluster_info(&cluster1).expect("Cluster1 not found");
    assert_eq!(cluster1_info.node_ids.len(), 3);
    assert!(cluster1_info.node_ids.contains(&1));
    assert!(cluster1_info.node_ids.contains(&2));
    assert!(cluster1_info.node_ids.contains(&3));
    assert_eq!(cluster1_info.active_workflows.len(), 2);
    assert!(cluster1_info.active_workflows.contains(&workflow1));
    assert!(cluster1_info.active_workflows.contains(&workflow2));

    let cluster2_info = executor2.get_cluster_info(&cluster2).expect("Cluster2 not found");
    assert_eq!(cluster2_info.node_ids.len(), 2);
    assert!(cluster2_info.node_ids.contains(&4));
    assert!(cluster2_info.node_ids.contains(&5));
    assert_eq!(cluster2_info.active_workflows.len(), 0); // workflow3 completed

    // Verify workflow locations
    assert_eq!(executor2.get_workflow_location(&workflow1), Some(cluster1));
    assert_eq!(executor2.get_workflow_location(&workflow2), Some(cluster1));
    assert_eq!(executor2.get_workflow_location(&workflow3), None); // Completed

    // Verify node memberships
    let node1_clusters = executor2.get_node_clusters(1);
    assert_eq!(node1_clusters.len(), 1);
    assert!(node1_clusters.contains(&cluster1));

    let node4_clusters = executor2.get_node_clusters(4);
    assert_eq!(node4_clusters.len(), 1);
    assert!(node4_clusters.contains(&cluster2));

    println!("\n✅ Snapshot roundtrip test passed!");
}

#[test]
fn test_snapshot_interval_logic() {
    let executor = ManagementCommandExecutor::new();

    // Test the snapshot interval decision logic

    // Should NOT create snapshot at log index 0
    assert!(!executor.should_create_snapshot(0, 1000));

    // Should NOT create snapshot before interval
    assert!(!executor.should_create_snapshot(500, 1000));
    assert!(!executor.should_create_snapshot(999, 1000));

    // SHOULD create snapshot at interval boundaries
    assert!(executor.should_create_snapshot(1000, 1000));
    assert!(executor.should_create_snapshot(2000, 1000));
    assert!(executor.should_create_snapshot(3000, 1000));

    // Should NOT create snapshot between boundaries
    assert!(!executor.should_create_snapshot(1001, 1000));
    assert!(!executor.should_create_snapshot(1500, 1000));

    // Test with different interval (every 500 entries)
    assert!(executor.should_create_snapshot(500, 500));
    assert!(executor.should_create_snapshot(1000, 500));
    assert!(executor.should_create_snapshot(1500, 500));
    assert!(!executor.should_create_snapshot(501, 500));
    assert!(!executor.should_create_snapshot(999, 500));

    println!("✅ Snapshot interval logic test passed!");
}

#[test]
fn test_snapshot_serialization_format() {
    let logger = create_test_logger();
    let executor = ManagementCommandExecutor::with_logger(logger);

    let cluster_id = Uuid::new_v4();

    // Create minimal state
    executor.apply(&ManagementCommand::CreateExecutionCluster(
        CreateExecutionClusterData {
            cluster_id,
            initial_node_ids: vec![1],
        }
    )).expect("Failed to create cluster");

    // Create snapshot
    let snapshot_bytes = executor.create_snapshot(1)
        .expect("Failed to create snapshot");

    // Verify it's valid JSON
    let json_value: serde_json::Value = serde_json::from_slice(&snapshot_bytes)
        .expect("Snapshot should be valid JSON");

    // Verify expected fields exist
    assert!(json_value.get("execution_clusters").is_some());
    assert!(json_value.get("node_memberships").is_some());
    assert!(json_value.get("workflow_locations").is_some());

    // Verify TTL cache is NOT in snapshot
    assert!(json_value.get("completed_workflows").is_none());

    println!("Snapshot JSON structure:");
    println!("{}", serde_json::to_string_pretty(&json_value).unwrap());
    println!("\n✅ Snapshot serialization format test passed!");
}
