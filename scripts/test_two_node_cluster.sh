#!/bin/bash
#
# Test script for verifying two-node cluster functionality
#
# This script:
# 1. Starts a bootstrap node (node 1) as the initial cluster
# 2. Starts a second node (node 2) that joins the existing cluster
# 3. Both nodes execute the ping/pong workflow to verify distributed consensus
# 4. Displays the results and cleans up
#
# Usage: ./scripts/test_two_node_cluster.sh
#

set -e

# Colors for output
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo -e "${BLUE}=== Raftoral Two-Node Cluster Test ===${NC}\n"

# Clean up any previous test logs
rm -f /tmp/raftoral_node1.log /tmp/raftoral_node2.log

# Build the binary first (dev mode for faster builds)
echo -e "${YELLOW}Building raftoral binary...${NC}"
cargo build --bin raftoral 2>&1 | grep -v "Compiling\|Finished" || true
echo -e "${GREEN}✓ Build complete${NC}\n"

# Start first node (bootstrap) in background
echo -e "${YELLOW}Starting Node 1 (bootstrap)...${NC}"
cargo run --bin raftoral -- --listen 127.0.0.1:7001 --bootstrap > /tmp/raftoral_node1.log 2>&1 &
NODE1_PID=$!
echo -e "${GREEN}✓ Node 1 started (PID: $NODE1_PID)${NC}"

# Wait for node 1 to be ready
echo -e "${YELLOW}Waiting for Node 1 to initialize...${NC}"
sleep 3

# Start second node (join) in background
echo -e "${YELLOW}Starting Node 2 (join cluster)...${NC}"
cargo run --bin raftoral -- --listen 127.0.0.1:7002 --peers 127.0.0.1:7001 > /tmp/raftoral_node2.log 2>&1 &
NODE2_PID=$!
echo -e "${GREEN}✓ Node 2 started (PID: $NODE2_PID)${NC}"

# Wait for both nodes to run and execute workflows
echo -e "${YELLOW}Waiting for cluster to stabilize and workflows to execute...${NC}"
sleep 5

echo -e "\n${BLUE}=== Test Results ===${NC}\n"

# Show the ping/pong test results from both nodes
echo -e "${BLUE}--- Node 1 (Leader) ---${NC}"
if grep -q "Workflow completed: got 'pong'" /tmp/raftoral_node1.log; then
    echo -e "${GREEN}✓ Node 1 workflow test: PASSED${NC}"
    grep "Workflow completed" /tmp/raftoral_node1.log | head -1
else
    echo -e "${YELLOW}⚠ Node 1 workflow test: FAILED${NC}"
    grep "Testing ping" /tmp/raftoral_node1.log || echo "No workflow test found"
fi

echo -e "\n${BLUE}--- Node 2 (Follower) ---${NC}"
if grep -q "Workflow completed: got 'pong'" /tmp/raftoral_node2.log; then
    echo -e "${GREEN}✓ Node 2 workflow test: PASSED${NC}"
    grep "Workflow completed" /tmp/raftoral_node2.log | head -1
else
    echo -e "${YELLOW}⚠ Node 2 workflow test: FAILED${NC}"
    grep "Testing ping" /tmp/raftoral_node2.log || echo "No workflow test found"
fi

# Show cluster membership
echo -e "\n${BLUE}--- Cluster Membership ---${NC}"
echo -ne "${YELLOW}Node 1 cluster size: ${NC}"
grep "Cluster size:" /tmp/raftoral_node1.log | head -1 | awk '{print $4, $5}'

echo -ne "${YELLOW}Node 2 cluster size: ${NC}"
grep "Cluster size:" /tmp/raftoral_node2.log | head -1 | awk '{print $4, $5}'

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
