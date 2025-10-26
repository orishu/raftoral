#!/bin/bash
#
# Test script for verifying two-node cluster functionality
#
# This script:
# 1. Starts a bootstrap node (node 1) as the initial cluster
# 2. Starts a second node (node 2) that joins the existing cluster
# 3. Executes a ping/pong workflow via gRPC to verify distributed consensus
# 4. Displays the results and cleans up
#
# Prerequisites:
#   - grpcurl must be installed (brew install grpcurl)
#
# Usage: ./scripts/test_two_node_cluster.sh
#

set -e

# Colors for output
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # No Color

echo -e "${BLUE}=== Raftoral Two-Node Cluster Test ===${NC}\n"

# Check if grpcurl is installed
if ! command -v grpcurl &> /dev/null; then
    echo -e "${RED}Error: grpcurl is not installed${NC}"
    echo ""
    echo "Install with:"
    echo "  macOS:  brew install grpcurl"
    echo "  Linux:  go install github.com/fullstorydev/grpcurl/cmd/grpcurl@latest"
    exit 1
fi

# Clean up any previous test logs
rm -f /tmp/raftoral_node1.log /tmp/raftoral_node2.log

# Build the binary first (dev mode for faster builds)
echo -e "${YELLOW}Building raftoral binary...${NC}"
cargo build --bin raftoral 2>&1 | grep -v "Compiling\|Finished" || true
echo -e "${GREEN}✓ Build complete${NC}\n"

# Start first node (bootstrap) in background
echo -e "${YELLOW}Starting Node 1 (bootstrap)...${NC}"
RUST_LOG=info cargo run --bin raftoral -- --listen 127.0.0.1:7001 --bootstrap > /tmp/raftoral_node1.log 2>&1 &
NODE1_PID=$!
echo -e "${GREEN}✓ Node 1 started (PID: $NODE1_PID)${NC}"

# Wait for node 1 to be ready
echo -e "${YELLOW}Waiting for Node 1 to initialize...${NC}"
sleep 3

# Start second node (join) in background
echo -e "${YELLOW}Starting Node 2 (join cluster)...${NC}"
RUST_LOG=info cargo run --bin raftoral -- --listen 127.0.0.1:7002 --peers 127.0.0.1:7001 > /tmp/raftoral_node2.log 2>&1 &
NODE2_PID=$!
echo -e "${GREEN}✓ Node 2 started (PID: $NODE2_PID)${NC}"

# Wait for cluster to stabilize
echo -e "${YELLOW}Waiting for cluster to stabilize...${NC}"
sleep 5

echo -e "\n${BLUE}=== Running Workflow Test ===${NC}\n"

# Run a workflow via gRPC
echo -e "${YELLOW}Step 1: Starting ping_pong workflow asynchronously...${NC}"
RESPONSE=$(grpcurl \
    -plaintext \
    -d '{
        "workflow_type": "ping_pong",
        "version": 1,
        "input_json": "\"ping\""
    }' \
    127.0.0.1:7001 \
    raftoral.WorkflowManagement/RunWorkflowAsync 2>&1)

echo "$RESPONSE"

# Extract workflow_id and execution_cluster_id from response
WORKFLOW_ID=$(echo "$RESPONSE" | grep -o '"workflowId": *"[^"]*"' | cut -d'"' -f4)
EXECUTION_CLUSTER_ID=$(echo "$RESPONSE" | grep -o '"executionClusterId": *"[^"]*"' | cut -d'"' -f4)

if [ -z "$WORKFLOW_ID" ]; then
    echo -e "${RED}✗ Failed to start workflow${NC}"
    echo -e "${YELLOW}Cleaning up...${NC}"
    kill $NODE1_PID $NODE2_PID 2>/dev/null || true
    exit 1
fi

echo -e "${GREEN}✓ Workflow started successfully${NC}"
echo -e "  Workflow ID: $WORKFLOW_ID"
echo -e "  Execution Cluster ID: $EXECUTION_CLUSTER_ID"

# Wait for workflow completion
echo -e "\n${YELLOW}Step 2: Waiting for workflow completion...${NC}"
RESULT=$(grpcurl \
    -plaintext \
    -d "{
        \"workflow_id\": \"$WORKFLOW_ID\",
        \"execution_cluster_id\": \"$EXECUTION_CLUSTER_ID\",
        \"timeout_seconds\": 30
    }" \
    127.0.0.1:7001 \
    raftoral.WorkflowManagement/WaitForWorkflowCompletion 2>&1)

echo "$RESULT"

# Check if workflow succeeded
if echo "$RESULT" | grep -q '"success": *true'; then
    echo -e "${GREEN}✓ Workflow completed successfully${NC}"

    # Extract and display the result
    RESULT_JSON=$(echo "$RESULT" | grep -o '"resultJson": *"[^"]*"' | cut -d'"' -f4)
    if [ -n "$RESULT_JSON" ]; then
        echo -e "  Result: $RESULT_JSON"
    fi
else
    echo -e "${RED}✗ Workflow failed${NC}"
fi

# Show cluster membership from logs
echo -e "\n${BLUE}=== Cluster Status ===${NC}\n"

if grep -q "Registered ping_pong workflow (v1)" /tmp/raftoral_node1.log; then
    echo -e "${GREEN}✓ Node 1: Workflow registered${NC}"
else
    echo -e "${YELLOW}⚠ Node 1: Workflow not registered${NC}"
fi

if grep -q "Registered ping_pong workflow (v1)" /tmp/raftoral_node2.log; then
    echo -e "${GREEN}✓ Node 2: Workflow registered${NC}"
else
    echo -e "${YELLOW}⚠ Node 2: Workflow not registered${NC}"
fi

# Cleanup
echo -e "\n${YELLOW}Cleaning up...${NC}"
kill $NODE1_PID $NODE2_PID 2>/dev/null || true
wait $NODE1_PID 2>/dev/null || true
wait $NODE2_PID 2>/dev/null || true
echo -e "${GREEN}✓ Processes stopped${NC}"

echo -e "\n${BLUE}=== Test Complete ===${NC}"
echo -e "Logs saved to:"
echo -e "  - /tmp/raftoral_node1.log"
echo -e "  - /tmp/raftoral_node2.log"
echo -e "\nTo view full logs, run:"
echo -e "  ${YELLOW}tail -f /tmp/raftoral_node1.log${NC}"
echo -e "  ${YELLOW}tail -f /tmp/raftoral_node2.log${NC}"
