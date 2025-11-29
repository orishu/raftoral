#!/bin/bash
#
# Run the ping_pong workflow using HTTP REST API against a running Raftoral node.
#
# Uses the HTTP /workflow/execute endpoint.
#
# Prerequisites:
#   - curl installed (usually pre-installed on macOS/Linux)
#   - A Raftoral node running (e.g., cargo run --bin raftoral -- --transport http --listen 127.0.0.1:7001 --bootstrap)
#
# Usage:
#   ./scripts/run_ping_pong_http.sh [node_address]
#
# Example:
#   ./scripts/run_ping_pong_http.sh 127.0.0.1:7001

set -e

# Default to 127.0.0.1:7001 if no address provided
NODE_ADDRESS="${1:-127.0.0.1:7001}"

# Check if curl is installed
if ! command -v curl &> /dev/null; then
    echo "Error: curl is not installed"
    exit 1
fi

echo "=== Running ping_pong workflow via HTTP REST API ==="
echo ""
echo "Node address: $NODE_ADDRESS"
echo "Workflow: ping_pong (v1)"
echo "Input: \"ping\""
echo ""

# Start the workflow via HTTP POST
echo "Step 1: Starting workflow asynchronously..."
RESPONSE=$(curl -s -X POST "http://${NODE_ADDRESS}/workflow/execute" \
    -H "Content-Type: application/json" \
    -d '{
        "name": "ping_pong",
        "version": 1,
        "input": "\"ping\""
    }')

echo "$RESPONSE" | jq .

# Extract workflow_id and execution_cluster_id from response
WORKFLOW_ID=$(echo "$RESPONSE" | jq -r '.workflow_id')
EXECUTION_CLUSTER_ID=$(echo "$RESPONSE" | jq -r '.execution_cluster_id')

if [ -z "$WORKFLOW_ID" ] || [ "$WORKFLOW_ID" = "null" ]; then
    echo ""
    echo "Error: Failed to get workflow_id from response"
    exit 1
fi

if [ -z "$EXECUTION_CLUSTER_ID" ] || [ "$EXECUTION_CLUSTER_ID" = "null" ]; then
    echo ""
    echo "Error: Failed to get execution_cluster_id from response"
    exit 1
fi

echo ""
echo "Step 2: Waiting for workflow completion..."
echo "Workflow ID: $WORKFLOW_ID"
echo "Execution Cluster ID: $EXECUTION_CLUSTER_ID"
echo ""

curl -s -X GET "http://${NODE_ADDRESS}/workflow/wait?workflow_id=${WORKFLOW_ID}&execution_cluster_id=${EXECUTION_CLUSTER_ID}&timeout_seconds=30" \
    | jq .

echo ""
echo "Done!"
