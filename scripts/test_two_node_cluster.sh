#!/bin/bash
#
# Test script for verifying two-node cluster functionality with automatic node discovery
#
# This script demonstrates:
# 1. Bootstrap node (Node 1) creating a single-node management cluster
# 2. Joining node (Node 2) automatically discovering and joining via AddNode RPC
# 3. Both nodes sharing the same workflow registry
# 4. Workflow execution on the cluster
#
# Architecture:
# - Management Cluster (cluster_id=0): Both nodes participate for cluster coordination
# - Execution Cluster (cluster_id=1): Automatically created and managed by ClusterManager
# - Shared Workflow Registry: Both nodes can register and execute workflows
#
# Note: ClusterManager automatically creates execution clusters and assigns nodes to them.
# The bootstrap node triggers cluster creation when it adds itself to the management cluster.
# Joining nodes are automatically added to existing clusters based on ClusterManager's
# placement policy (prefers adding to clusters below target size before creating new ones).
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

# Cleanup function for graceful exit
cleanup() {
    echo -e "\n${YELLOW}Cleaning up...${NC}"
    if [ ! -z "$NODE1_PID" ]; then
        kill $NODE1_PID 2>/dev/null || true
        wait $NODE1_PID 2>/dev/null || true
    fi
    if [ ! -z "$NODE2_PID" ]; then
        kill $NODE2_PID 2>/dev/null || true
        wait $NODE2_PID 2>/dev/null || true
    fi
    echo -e "${GREEN}✓ Processes stopped${NC}"
}

# Set trap for cleanup on exit
trap cleanup EXIT

# Clean up any previous test logs
rm -f /tmp/raftoral_node1.log /tmp/raftoral_node2.log

# Build the binary first (dev mode for faster builds)
echo -e "${YELLOW}Building raftoral binary...${NC}"
cargo build --bin raftoral 2>&1 | grep -v "Compiling\|Finished" || true
echo -e "${GREEN}✓ Build complete${NC}\n"

# Start first node (bootstrap) in background
echo -e "${YELLOW}Starting Node 1 (bootstrap mode)...${NC}"
RUST_LOG=info cargo run --bin raftoral -- \
    --listen 127.0.0.1:7001 \
    --node-id 1 \
    --bootstrap \
    > /tmp/raftoral_node1.log 2>&1 &
NODE1_PID=$!
echo -e "${GREEN}✓ Node 1 started (PID: $NODE1_PID)${NC}"

# Wait for node 1 to be ready and for ClusterManager to create execution cluster
echo -e "${YELLOW}Waiting for Node 1 to initialize and ClusterManager to create execution cluster...${NC}"
sleep 4

# Verify node 1 is ready
if grep -q "ClusterManager action: CreateCluster" /tmp/raftoral_node1.log; then
    echo -e "${GREEN}✓ Node 1 execution cluster created by ClusterManager${NC}"
elif grep -q "FullNode started" /tmp/raftoral_node1.log; then
    echo -e "${GREEN}✓ Node 1 started${NC}"
else
    echo -e "${RED}⚠ Node 1 may not be ready${NC}"
    echo -e "${YELLOW}Checking logs...${NC}"
    tail -20 /tmp/raftoral_node1.log
fi

# Start second node (join mode - automatic discovery and AddNode)
echo -e "\n${YELLOW}Starting Node 2 (join mode with automatic discovery)...${NC}"
RUST_LOG=info cargo run --bin raftoral -- \
    --listen 127.0.0.1:7002 \
    --peers 127.0.0.1:7001 \
    > /tmp/raftoral_node2.log 2>&1 &
NODE2_PID=$!
echo -e "${GREEN}✓ Node 2 started (PID: $NODE2_PID)${NC}"

# Wait for node 2 to discover peers and join
echo -e "${YELLOW}Waiting for Node 2 to discover peers and join cluster...${NC}"
sleep 5

# Verify node 2 joined successfully
echo -e "\n${BLUE}=== Cluster Membership Status ===${NC}\n"

if grep -q "Assigned node ID" /tmp/raftoral_node2.log; then
    NODE2_ID=$(grep "Assigned node ID" /tmp/raftoral_node2.log | grep -o 'node_id: [0-9]*' | cut -d' ' -f2)
    echo -e "${GREEN}✓ Node 2 discovered cluster and got ID: $NODE2_ID${NC}"
else
    echo -e "${RED}✗ Node 2 failed to discover cluster${NC}"
    echo -e "${YELLOW}Node 2 logs:${NC}"
    tail -30 /tmp/raftoral_node2.log
    exit 1
fi

if grep -q "Successfully added to management cluster" /tmp/raftoral_node2.log; then
    echo -e "${GREEN}✓ Node 2 joined management cluster via AddNode RPC${NC}"
elif grep -q "Calling AddNode RPC" /tmp/raftoral_node2.log; then
    echo -e "${YELLOW}⚠ Node 2 attempted AddNode but may have failed${NC}"
    grep "AddNode" /tmp/raftoral_node2.log | tail -5
else
    echo -e "${RED}⚠ Node 2 did not attempt to join${NC}"
fi

# Wait for cluster to fully stabilize and for ClusterManager to add Node 2 to execution cluster
echo -e "\n${YELLOW}Waiting for clusters to stabilize and ClusterManager to assign Node 2...${NC}"
sleep 6

# Verify workflow registry on both nodes
if grep -q "Registered ping_pong workflow" /tmp/raftoral_node1.log; then
    echo -e "${GREEN}✓ Node 1: ping_pong workflow registered${NC}"
else
    echo -e "${YELLOW}⚠ Node 1: ping_pong workflow not found in logs${NC}"
fi

if grep -q "Registered ping_pong workflow" /tmp/raftoral_node2.log; then
    echo -e "${GREEN}✓ Node 2: ping_pong workflow registered${NC}"
else
    echo -e "${YELLOW}⚠ Node 2: ping_pong workflow not registered yet${NC}"
fi

echo -e "\n${BLUE}=== Running Workflow Test ===${NC}\n"

# Run a workflow via gRPC to Node 1 (should distribute across cluster)
echo -e "${YELLOW}Step 1: Starting ping_pong workflow asynchronously on Node 1...${NC}"
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
    echo -e "${YELLOW}Response was:${NC}"
    echo "$RESPONSE"
    exit 1
fi

echo -e "${GREEN}✓ Workflow started successfully${NC}"
echo -e "  Workflow ID: ${WORKFLOW_ID}"
echo -e "  Execution Cluster ID: ${EXECUTION_CLUSTER_ID}"

# Wait for workflow completion
echo -e "\n${YELLOW}Step 2: Waiting for workflow completion (max 30 seconds)...${NC}"
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
    echo -e "\n${GREEN}✓✓✓ Workflow completed successfully! ✓✓✓${NC}"

    # Extract and display the result
    RESULT_JSON=$(echo "$RESULT" | grep -o '"resultJson": *"[^"]*"' | cut -d'"' -f4)
    if [ -n "$RESULT_JSON" ]; then
        echo -e "${GREEN}  Result: $RESULT_JSON${NC}"

        # Check if result is "pong"
        if echo "$RESULT_JSON" | grep -q "pong"; then
            echo -e "${GREEN}  ✓ Ping/pong workflow executed correctly across cluster!${NC}"
        fi
    fi
else
    echo -e "${RED}✗ Workflow failed or timed out${NC}"
    echo -e "${YELLOW}Checking logs for details...${NC}"
    echo -e "\n${YELLOW}Node 1 logs (last 20 lines):${NC}"
    tail -20 /tmp/raftoral_node1.log
    echo -e "\n${YELLOW}Node 2 logs (last 20 lines):${NC}"
    tail -20 /tmp/raftoral_node2.log
fi

# Final cluster status
echo -e "\n${BLUE}=== Final Cluster Status ===${NC}\n"

echo -e "${YELLOW}Node 1 (bootstrap):${NC}"
echo -e "  - Management cluster leader: $(grep -q 'became leader' /tmp/raftoral_node1.log && echo 'YES' || echo 'NO')"
echo -e "  - Workflow registered: $(grep -q 'Registered ping_pong' /tmp/raftoral_node1.log && echo 'YES' || echo 'NO')"
echo -e "  - Execution cluster (ClusterManager): $(grep -q 'ClusterManager action: CreateCluster' /tmp/raftoral_node1.log && echo 'CREATED' || echo 'UNKNOWN')"

echo -e "\n${YELLOW}Node 2 (joined):${NC}"
echo -e "  - Discovered peers: $(grep -q 'Assigned node ID' /tmp/raftoral_node2.log && echo 'YES' || echo 'NO')"
echo -e "  - Joined management cluster: $(grep -q 'Successfully added to management cluster' /tmp/raftoral_node2.log && echo 'YES' || echo 'UNKNOWN')"
echo -e "  - Added to execution cluster: $(grep -q 'ClusterManager action: AddNodeToCluster' /tmp/raftoral_node1.log && echo 'YES (by ClusterManager)' || echo 'UNKNOWN')"
echo -e "  - Workflow registered: $(grep -q 'Registered ping_pong' /tmp/raftoral_node2.log && echo 'YES' || echo 'NO')"

echo -e "\n${BLUE}=== Test Complete ===${NC}"
echo -e "Logs saved to:"
echo -e "  - /tmp/raftoral_node1.log"
echo -e "  - /tmp/raftoral_node2.log"
echo -e "\nTo view full logs, run:"
echo -e "  ${YELLOW}tail -f /tmp/raftoral_node1.log${NC}"
echo -e "  ${YELLOW}tail -f /tmp/raftoral_node2.log${NC}"
