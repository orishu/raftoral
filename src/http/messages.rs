//! HTTP Message Serialization
//!
//! This module handles conversion between Raft protobuf messages and JSON
//! for HTTP transport. We serialize GenericMessage to protobuf bytes and
//! wrap them in JSON for HTTP transport.

use crate::grpc::proto;
use prost::Message;
use serde::{Deserialize, Serialize};

/// Wrapper for GenericMessage that can be JSON-serialized
/// Contains the entire GenericMessage encoded as protobuf bytes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonGenericMessage {
    pub message_bytes: Vec<u8>,
}

impl From<proto::GenericMessage> for JsonGenericMessage {
    fn from(msg: proto::GenericMessage) -> Self {
        let mut buf = Vec::new();
        msg.encode(&mut buf).expect("Failed to encode GenericMessage");
        Self {
            message_bytes: buf,
        }
    }
}

impl From<JsonGenericMessage> for proto::GenericMessage {
    fn from(msg: JsonGenericMessage) -> Self {
        proto::GenericMessage::decode(&msg.message_bytes[..])
            .expect("Failed to decode GenericMessage")
    }
}

/// Workflow execution request (JSON-friendly)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRunWorkflowRequest {
    pub name: String,
    pub version: u32,
    pub input: String, // JSON string
}

impl From<proto::RunWorkflowRequest> for JsonRunWorkflowRequest {
    fn from(req: proto::RunWorkflowRequest) -> Self {
        Self {
            name: req.workflow_type,
            version: req.version,
            input: req.input_json,
        }
    }
}

impl From<JsonRunWorkflowRequest> for proto::RunWorkflowRequest {
    fn from(req: JsonRunWorkflowRequest) -> Self {
        Self {
            workflow_type: req.name,
            version: req.version,
            input_json: req.input,
        }
    }
}

/// Workflow execution response (JSON-friendly)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRunWorkflowResponse {
    pub workflow_id: String,
    pub success: bool,
    pub execution_cluster_id: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
}

impl From<proto::RunWorkflowAsyncResponse> for JsonRunWorkflowResponse {
    fn from(resp: proto::RunWorkflowAsyncResponse) -> Self {
        Self {
            workflow_id: resp.workflow_id,
            success: resp.success,
            execution_cluster_id: if resp.execution_cluster_id.is_empty() {
                None
            } else {
                Some(resp.execution_cluster_id)
            },
            result: None,
            error: if resp.error.is_empty() { None } else { Some(resp.error) },
        }
    }
}

impl From<JsonRunWorkflowResponse> for proto::RunWorkflowAsyncResponse {
    fn from(resp: JsonRunWorkflowResponse) -> Self {
        Self {
            success: resp.success,
            workflow_id: resp.workflow_id,
            execution_cluster_id: resp.execution_cluster_id.unwrap_or_default(),
            error: resp.error.unwrap_or_default(),
        }
    }
}

/// Node registration for discovery
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterNodeRequest {
    pub address: String,
    pub node_id: Option<u64>,
}

/// Response to node registration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterNodeResponse {
    pub node_id: u64,
    pub peers: Vec<PeerInfo>,
}

/// Peer information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub node_id: u64,
    pub address: String,
}

/// Health check response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub node_id: u64,
    pub is_leader: bool,
}

/// Workflow wait/result response (JSON-friendly)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonWorkflowResultResponse {
    pub success: bool,
    pub result: Option<String>,
    pub error: Option<String>,
}

impl From<proto::RunWorkflowResponse> for JsonWorkflowResultResponse {
    fn from(resp: proto::RunWorkflowResponse) -> Self {
        Self {
            success: resp.success,
            result: if resp.result_json.is_empty() {
                None
            } else {
                Some(resp.result_json)
            },
            error: if resp.error.is_empty() { None } else { Some(resp.error) },
        }
    }
}

impl From<JsonWorkflowResultResponse> for proto::RunWorkflowResponse {
    fn from(resp: JsonWorkflowResultResponse) -> Self {
        Self {
            success: resp.success,
            result_json: resp.result.unwrap_or_default(),
            error: resp.error.unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generic_message_serialization() {
        // Create a GenericMessage
        let msg = proto::GenericMessage {
            cluster_id: 42,
            message: None,
        };

        // Convert to JSON wrapper
        let json_msg: JsonGenericMessage = msg.clone().into();

        // Verify it can be serialized to JSON
        let json_str = serde_json::to_string(&json_msg).expect("Failed to serialize to JSON");
        assert!(json_str.contains("message_bytes"));

        // Deserialize back
        let deserialized: JsonGenericMessage = serde_json::from_str(&json_str).expect("Failed to deserialize from JSON");

        // Convert back to GenericMessage
        let recovered_msg: proto::GenericMessage = deserialized.into();

        // Verify cluster_id is preserved
        assert_eq!(recovered_msg.cluster_id, 42);
    }

    #[test]
    fn test_workflow_request_conversion() {
        let proto_req = proto::RunWorkflowRequest {
            workflow_type: "test_workflow".to_string(),
            version: 1,
            input_json: r#"{"key": "value"}"#.to_string(),
        };

        // Convert to JSON-friendly format
        let json_req: JsonRunWorkflowRequest = proto_req.clone().into();

        assert_eq!(json_req.name, "test_workflow");
        assert_eq!(json_req.version, 1);
        assert_eq!(json_req.input, r#"{"key": "value"}"#);

        // Convert back
        let recovered: proto::RunWorkflowRequest = json_req.into();

        assert_eq!(recovered.workflow_type, "test_workflow");
        assert_eq!(recovered.version, 1);
        assert_eq!(recovered.input_json, r#"{"key": "value"}"#);
    }

    #[test]
    fn test_workflow_response_conversion() {
        let json_resp = JsonRunWorkflowResponse {
            workflow_id: "wf-123".to_string(),
            success: true,
            execution_cluster_id: Some("1".to_string()),
            result: Some("result data".to_string()),
            error: None,
        };

        // Convert to proto
        let proto_resp: proto::RunWorkflowAsyncResponse = json_resp.clone().into();

        assert_eq!(proto_resp.workflow_id, "wf-123");
        assert!(proto_resp.success);
        assert_eq!(proto_resp.execution_cluster_id, "1");
        assert_eq!(proto_resp.error, "");

        // Convert back
        let json_resp2: JsonRunWorkflowResponse = proto_resp.into();

        assert_eq!(json_resp2.workflow_id, "wf-123");
        assert!(json_resp2.success);
        assert_eq!(json_resp2.execution_cluster_id, Some("1".to_string()));
        assert_eq!(json_resp2.error, None);
    }

    #[test]
    fn test_workflow_result_response_conversion() {
        // Test successful result
        let json_resp = JsonWorkflowResultResponse {
            success: true,
            result: Some(r#"{"output": "pong"}"#.to_string()),
            error: None,
        };

        // Convert to proto
        let proto_resp: proto::RunWorkflowResponse = json_resp.clone().into();

        assert!(proto_resp.success);
        assert_eq!(proto_resp.result_json, r#"{"output": "pong"}"#);
        assert_eq!(proto_resp.error, "");

        // Convert back
        let json_resp2: JsonWorkflowResultResponse = proto_resp.into();

        assert!(json_resp2.success);
        assert_eq!(json_resp2.result, Some(r#"{"output": "pong"}"#.to_string()));
        assert_eq!(json_resp2.error, None);

        // Test error result
        let error_resp = JsonWorkflowResultResponse {
            success: false,
            result: None,
            error: Some("Workflow failed".to_string()),
        };

        let proto_error: proto::RunWorkflowResponse = error_resp.clone().into();
        assert!(!proto_error.success);
        assert_eq!(proto_error.result_json, "");
        assert_eq!(proto_error.error, "Workflow failed");
    }
}
