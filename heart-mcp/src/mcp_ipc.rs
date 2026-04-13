//! MCP Supervisor ↔ client wire types (Unix socket framing uses [`crate::ipc::IpcConnection`]).
use serde::{Deserialize, Serialize};

/// A tool spec: name, description, JSON Schema parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Messages sent from Client → MCP Supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum McpRequest {
    /// List all available tools.
    ListTools,
    /// Call a tool on a specific server.
    CallTool {
        call_id: String,
        server_name: String,
        tool_name: String,
        arguments: serde_json::Value,
    },
    /// Health check.
    Ping,
}

/// Messages sent from MCP Supervisor → Client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum McpResponse {
    /// Full tool list (pushed on connect + after hot reload).
    ToolList { tools: Vec<ToolSpec> },
    /// Tool call result.
    ToolResult {
        call_id: String,
        result: serde_json::Value,
        is_error: bool,
    },
    /// Health reply.
    Pong {
        server_count: usize,
        tool_count: usize,
    },
}
