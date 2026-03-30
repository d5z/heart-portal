use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

/// JSON-RPC 2.0 Request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<u64>,
    pub method: String,
    pub params: Value,
}

/// JSON-RPC 2.0 Response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC 2.0 Error
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcRequest {
    /// Create a new request with auto-incremented ID
    pub fn new(method: impl Into<String>, params: Value, id_counter: &AtomicU64) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Some(id_counter.fetch_add(1, Ordering::SeqCst)),
            method: method.into(),
            params,
        }
    }

    /// Create a new notification (no ID)
    pub fn notification(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: method.into(),
            params,
        }
    }
}

impl JsonRpcResponse {
    /// Create a success response
    pub fn success(id: Option<u64>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Create an error response
    pub fn error(id: Option<u64>, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }

    /// Check if response is successful
    pub fn is_success(&self) -> bool {
        self.error.is_none()
    }

    /// Check if response is an error
    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }
}

impl JsonRpcError {
    /// Create a new JSON-RPC error
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Create an error with additional data
    pub fn with_data(code: i32, message: impl Into<String>, data: Value) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(data),
        }
    }
}

/// MCP tool information from server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    #[serde(default, alias = "inputSchema")]
    pub input_schema: Value,
}

/// MCP server initialization capabilities
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpCapabilities {
    pub tools: Option<McpToolsCapability>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolsCapability {
    #[serde(default)]
    pub list_changed: bool,
}

impl Default for McpCapabilities {
    fn default() -> Self {
        Self {
            tools: Some(McpToolsCapability { list_changed: false }),
        }
    }
}

/// MCP server information from initialization
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerInfo {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub capabilities: McpCapabilities,
}

/// Helper functions for common MCP operations
pub struct McpProtocol;

impl McpProtocol {
    /// Create an initialize request
    pub fn initialize_request(
        client_name: &str,
        client_version: &str,
        id_counter: &AtomicU64,
    ) -> JsonRpcRequest {
        let params = json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "clientInfo": {
                "name": client_name,
                "version": client_version
            }
        });
        
        JsonRpcRequest::new("initialize", params, id_counter)
    }

    /// Create an initialized notification
    pub fn initialized_notification() -> JsonRpcRequest {
        JsonRpcRequest::notification("notifications/initialized", json!({}))
    }

    /// Create a tools/list request
    pub fn tools_list_request(id_counter: &AtomicU64) -> JsonRpcRequest {
        JsonRpcRequest::new("tools/list", json!({}), id_counter)
    }

    /// Create a tools/call request
    pub fn tools_call_request(
        tool_name: &str,
        arguments: Value,
        id_counter: &AtomicU64,
    ) -> JsonRpcRequest {
        let params = json!({
            "name": tool_name,
            "arguments": arguments
        });
        
        JsonRpcRequest::new("tools/call", params, id_counter)
    }

    /// Parse server info from initialize response
    pub fn parse_server_info(response: &JsonRpcResponse) -> anyhow::Result<McpServerInfo> {
        let result = response.result.as_ref()
            .ok_or_else(|| anyhow::anyhow!("No result in initialize response"))?;
            
        serde_json::from_value(result.clone())
            .map_err(|e| anyhow::anyhow!("Failed to parse server info: {}", e))
    }

    /// Parse tool list from tools/list response
    pub fn parse_tool_list(response: &JsonRpcResponse) -> anyhow::Result<Vec<McpToolInfo>> {
        let result = response.result.as_ref()
            .ok_or_else(|| anyhow::anyhow!("No result in tools/list response"))?;
            
        let tools = result["tools"].as_array()
            .ok_or_else(|| anyhow::anyhow!("No tools array in response"))?;
            
        tools.iter()
            .map(|tool| serde_json::from_value(tool.clone()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("Failed to parse tool info: {}", e))
    }

    /// Parse tool call result from tools/call response
    pub fn parse_tool_result(response: &JsonRpcResponse) -> anyhow::Result<Value> {
        if let Some(error) = &response.error {
            anyhow::bail!("Tool call error: {} (code: {})", error.message, error.code);
        }
        
        response.result.clone()
            .ok_or_else(|| anyhow::anyhow!("No result in tool call response"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_json_rpc_request_new() {
        let counter = AtomicU64::new(1);
        let req = JsonRpcRequest::new("test", json!({"param": "value"}), &counter);
        
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.id, Some(1));
        assert_eq!(req.method, "test");
        assert_eq!(req.params["param"], "value");
        
        let req2 = JsonRpcRequest::new("test2", json!({}), &counter);
        assert_eq!(req2.id, Some(2));
    }

    #[test]
    fn test_json_rpc_notification() {
        let req = JsonRpcRequest::notification("notify", json!({"data": "test"}));
        
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.id, None);
        assert_eq!(req.method, "notify");
    }

    #[test]
    fn test_json_rpc_response_success() {
        let resp = JsonRpcResponse::success(Some(1), json!({"result": "ok"}));
        
        assert_eq!(resp.jsonrpc, "2.0");
        assert_eq!(resp.id, Some(1));
        assert!(resp.is_success());
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["result"], "ok");
    }

    #[test]
    fn test_json_rpc_response_error() {
        let error = JsonRpcError::new(-1, "Test error");
        let resp = JsonRpcResponse::error(Some(1), error);
        
        assert!(resp.is_error());
        assert!(!resp.is_success());
        assert_eq!(resp.error.unwrap().message, "Test error");
    }

    #[test]
    fn test_serialize_request() {
        let counter = AtomicU64::new(1);
        let req = JsonRpcRequest::new("test", json!({"param": "value"}), &counter);
        
        let serialized = serde_json::to_string(&req).unwrap();
        let expected = r#"{"jsonrpc":"2.0","id":1,"method":"test","params":{"param":"value"}}"#;
        
        assert_eq!(serialized, expected);
    }

    #[test]
    fn test_deserialize_response() {
        let json_str = r#"{"jsonrpc":"2.0","id":1,"result":{"data":"test"}}"#;
        let resp: JsonRpcResponse = serde_json::from_str(json_str).unwrap();
        
        assert_eq!(resp.jsonrpc, "2.0");
        assert_eq!(resp.id, Some(1));
        assert!(resp.is_success());
        assert_eq!(resp.result.unwrap()["data"], "test");
    }

    #[test]
    fn test_mcp_protocol_initialize() {
        let counter = AtomicU64::new(1);
        let req = McpProtocol::initialize_request("test-client", "1.0", &counter);
        
        assert_eq!(req.method, "initialize");
        assert!(req.params["clientInfo"]["name"].as_str().unwrap() == "test-client");
        assert!(req.params["protocolVersion"].as_str().unwrap() == "2024-11-05");
    }

    #[test]
    fn test_parse_tool_list() {
        let response_json = json!({
            "tools": [
                {
                    "name": "test_tool",
                    "description": "A test tool",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "param": {"type": "string"}
                        }
                    }
                }
            ]
        });
        
        let response = JsonRpcResponse::success(Some(1), response_json);
        let tools = McpProtocol::parse_tool_list(&response).unwrap();
        
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "test_tool");
        assert_eq!(tools[0].description, "A test tool");
        // Verify inputSchema (camelCase from MCP) is correctly deserialized
        assert_eq!(tools[0].input_schema["type"], "object");
        assert_eq!(tools[0].input_schema["properties"]["param"]["type"], "string");
    }
}