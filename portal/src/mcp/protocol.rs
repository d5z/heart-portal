use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};

/// JSON-RPC 2.0 request sent to an MCP server.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<u64>,
    pub method: String,
    pub params: Value,
}

/// JSON-RPC 2.0 response received from an MCP server.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC 2.0 error object.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcRequest {
    /// Create a request with an auto-incremented id.
    pub fn new(method: impl Into<String>, params: Value, id_counter: &AtomicU64) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Some(id_counter.fetch_add(1, Ordering::SeqCst)),
            method: method.into(),
            params,
        }
    }

    /// Create a notification, which does not receive a response.
    pub fn notification(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: method.into(),
            params,
        }
    }
}

/// MCP tool metadata returned by `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    #[serde(default, alias = "inputSchema")]
    pub input_schema: Value,
}
