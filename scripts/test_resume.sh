#!/bin/bash
# Test script to demonstrate node restart with persistent storage

STORAGE_DIR="/tmp/raftoral_resume_test"

echo "=== Testing Node Resume from Persistent Storage ==="
echo

# Clean up any existing storage
if [ -d "$STORAGE_DIR" ]; then
    echo "Cleaning up previous test data..."
    rm -rf "$STORAGE_DIR"
fi

echo "Step 1: Starting node with persistent storage (bootstrap mode)"
echo "Command: cargo run -- --listen 127.0.0.1:5001 --bootstrap --storage-path $STORAGE_DIR"
echo "Wait 5 seconds for initialization, then press Ctrl+C to stop..."
echo

cargo run -- --listen 127.0.0.1:5001 --bootstrap --storage-path "$STORAGE_DIR" &
PID=$!

# Wait for the node to start and initialize
sleep 5

# Kill the process
echo
echo "Killing the node (simulating crash)..."
kill $PID
wait $PID 2>/dev/null

echo
echo "Storage created in: $STORAGE_DIR"
ls -la "$STORAGE_DIR"

echo
echo "Step 2: Restarting node - it should automatically resume from storage"
echo "Command: cargo run -- --listen 127.0.0.1:5001 --storage-path $STORAGE_DIR"
echo ""
echo "Watch for these log messages:"
echo "  - 'Checking for existing storage'"
echo "  - 'Found existing storage with node_id'"
echo "  - 'Resuming from existing storage'"
echo ""
echo "Notice: No --bootstrap or --peers needed - it detects existing storage!"
echo "Press Ctrl+C to stop..."
echo

cargo run -- --listen 127.0.0.1:5001 --storage-path "$STORAGE_DIR"

# Cleanup
echo
echo "Cleaning up test data..."
rm -rf "$STORAGE_DIR"
echo "Done!"
