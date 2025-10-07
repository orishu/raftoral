#!/bin/bash
#
# Run the ping_pong workflow using grpcurl against a running Raftoral node.
#
# Uses gRPC reflection, so no proto files are needed!
#
# Prerequisites:
#   - grpcurl installed (brew install grpcurl or go install github.com/fullstorydev/grpcurl/cmd/grpcurl@latest)
#   - A Raftoral node running (e.g., cargo run -- --listen 127.0.0.1:5001 --bootstrap)
#
# Usage:
#   ./scripts/run_ping_pong.sh [node_address]
#
# Example:
#   ./scripts/run_ping_pong.sh 127.0.0.1:5001

set -e

# Default to 127.0.0.1:5001 if no address provided
NODE_ADDRESS="${1:-127.0.0.1:5001}"

# Check if grpcurl is installed
if ! command -v grpcurl &> /dev/null; then
    echo "Error: grpcurl is not installed"
    echo ""
    echo "Install with:"
    echo "  macOS:  brew install grpcurl"
    echo "  Linux:  go install github.com/fullstorydev/grpcurl/cmd/grpcurl@latest"
    exit 1
fi

echo "=== Running ping_pong workflow via gRPC ==="
echo ""
echo "Node address: $NODE_ADDRESS"
echo "Workflow: ping_pong (v1)"
echo "Input: \"ping\""
echo ""

# Run the workflow using grpcurl with reflection (no proto file needed!)
grpcurl \
    -plaintext \
    -d '{
        "workflow_type": "ping_pong",
        "version": 1,
        "input_json": "\"ping\""
    }' \
    "$NODE_ADDRESS" \
    raftoral.WorkflowManagement/RunWorkflowSync

echo ""
echo "Done!"
