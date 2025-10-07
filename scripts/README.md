# Raftoral Scripts

Utility scripts for testing and interacting with Raftoral clusters.

## Prerequisites

- **grpcurl**: Command-line gRPC client
  - macOS: `brew install grpcurl`
  - Linux: `go install github.com/fullstorydev/grpcurl/cmd/grpcurl@latest`

## Available Scripts

### `run_ping_pong.sh`

Execute the `ping_pong` workflow via gRPC using grpcurl.

**Features:**
- Uses gRPC reflection - no proto files needed!
- Works with any Raftoral node in the cluster
- Automatic service discovery

**Usage:**
```bash
./scripts/run_ping_pong.sh [node_address]
```

**Examples:**
```bash
# Use default address (127.0.0.1:5001)
./scripts/run_ping_pong.sh

# Specify custom address
./scripts/run_ping_pong.sh 127.0.0.1:7001
```

**Prerequisites:**
1. Start a Raftoral node:
   ```bash
   cargo run -- --listen 127.0.0.1:5001 --bootstrap
   ```

2. Run the script (from repo root):
   ```bash
   ./scripts/run_ping_pong.sh
   ```

**Expected Output:**
```json
{
  "success": true,
  "resultJson": "\"pong\""
}
```

## Using grpcurl Directly

Raftoral servers support gRPC reflection, so you can use grpcurl without proto files:

```bash
# List available services (using reflection)
grpcurl -plaintext 127.0.0.1:5001 list

# Describe a service (using reflection)
grpcurl -plaintext 127.0.0.1:5001 describe raftoral.WorkflowManagement

# Call RunWorkflowSync with custom input (using reflection)
grpcurl -plaintext \
    -d '{
        "workflow_type": "my_workflow",
        "version": 1,
        "input_json": "{\"key\": \"value\"}"
    }' \
    127.0.0.1:5001 \
    raftoral.WorkflowManagement/RunWorkflowSync
```

### Using Proto Files (Optional)

If you prefer to use proto files explicitly:

```bash
# List services with proto file
grpcurl -plaintext -import-path proto -proto raftoral.proto 127.0.0.1:5001 list

# Call with proto file
grpcurl -plaintext \
    -import-path proto \
    -proto raftoral.proto \
    -d '{"workflow_type": "ping_pong", "version": 1, "input_json": "\"ping\""}' \
    127.0.0.1:5001 \
    raftoral.WorkflowManagement/RunWorkflowSync
```

## Notes

- The `input_json` field must be a JSON-encoded string
- For simple string inputs, use escaped quotes: `"\"ping\""`
- For complex objects, escape the entire JSON: `"{\"key\": \"value\"}"`
- Workflow errors are returned in the response payload (`success: false, error: "..."`), not as gRPC status codes
