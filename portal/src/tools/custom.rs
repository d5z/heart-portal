//! Custom MCP tools — being-defined tools loaded from workspace/tools/mcp.toml.
//!
//! Portal acts as MCP supervisor: spawns stdio MCP servers that the being defines,
//! discovers their tools, and proxies calls through to Hearth.
//!
//! This gives beings the same DIY tool experience as local MCP servers.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use anyhow::Result;
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{info, warn, error, debug};

use heart_mcp::client::McpClient;
use heart_mcp::connection::{McpServerConfig, McpTransport};
use heart_mcp::protocol::McpToolInfo;

use super::ToolInfo;

/// Manages custom MCP tools defined by the being.
#[derive(Clone)]
pub struct CustomToolHost {
    client: Arc<Mutex<Option<Arc<Mutex<McpClient>>>>>,
    /// Map from tool name → server name (for routing calls)
    tool_routes: Arc<Mutex<Vec<(String, String)>>>,
    /// Cached tool list
    tools: Arc<Mutex<Vec<ToolInfo>>>,
}

impl CustomToolHost {
    pub fn new() -> Self {
        Self {
            client: Arc::new(Mutex::new(None)),
            tool_routes: Arc::new(Mutex::new(Vec::new())),
            tools: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Shutdown existing custom MCP servers
    pub async fn shutdown(&self) {
        *self.client.lock().await = None;
        self.tool_routes.lock().await.clear();
        self.tools.lock().await.clear();
        info!("Custom MCP servers shut down");
    }

    /// Load custom tools from workspace/tools/mcp.toml.
    /// Format is the same as Heart's mcp-servers.toml:
    ///
    /// ```toml
    /// [[servers]]
    /// name = "my-tool"
    /// command = ["node", "my-tool.js"]
    ///
    /// [[servers]]
    /// name = "another"
    /// command = ["python3", "another.py"]
    /// ```
    pub async fn load(&self, workspace_root: &Path) -> Result<usize> {
        let config_path = workspace_root.join("tools").join("mcp.toml");

        if !config_path.exists() {
            debug!("No custom tools config at {}", config_path.display());
            return Ok(0);
        }

        info!("Loading custom tools from {}", config_path.display());
        let content = tokio::fs::read_to_string(&config_path).await?;

        let configs = parse_custom_mcp_config(&content, workspace_root)?;
        if configs.is_empty() {
            info!("No custom MCP servers configured");
            return Ok(0);
        }

        info!("Connecting to {} custom MCP servers...", configs.len());
        let client = McpClient::connect_all(configs).await?;

        if client.connection_count() == 0 {
            warn!("No custom MCP servers connected successfully");
            return Ok(0);
        }

        // Discover tools
        let discovered = client.discover_tools().await;
        let tool_count = discovered.len();

        if tool_count == 0 {
            info!("Custom MCP servers connected but no tools discovered");
            return Ok(0);
        }

        // Build tool list and route map
        let mut tools = Vec::new();
        let mut routes = Vec::new();

        for (server_name, tool_info) in &discovered {
            // No prefix — Cortex MCP adapter adds "portal_" automatically.
            // Being defines "hello_world" → becomes "portal_hello_world" at Hearth.
            let tool = ToolInfo {
                name: tool_info.name.clone(),
                description: tool_info.description.clone(),
                input_schema: tool_info.input_schema.clone(),
            };
            routes.push((tool.name.clone(), server_name.clone()));
            tools.push(tool);
        }

        info!("Discovered {} custom tools: {}", tool_count,
            tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", "));

        // Store
        let client_arc = Arc::new(Mutex::new(client));
        *self.client.lock().await = Some(client_arc);
        *self.tool_routes.lock().await = routes;
        *self.tools.lock().await = tools;

        Ok(tool_count)
    }

    /// List all custom tools
    pub async fn list_tools(&self) -> Vec<ToolInfo> {
        self.tools.lock().await.clone()
    }

    /// Check if a tool name belongs to custom tools
    pub async fn has_tool(&self, name: &str) -> bool {
        self.tool_routes.lock().await.iter().any(|(n, _)| n == name)
    }

    /// Call a custom tool by proxying to the underlying MCP server
    pub async fn call(&self, tool_name: &str, arguments: Value) -> Result<Value> {
        let routes = self.tool_routes.lock().await;
        let (_, server_name) = routes.iter()
            .find(|(n, _)| n == tool_name)
            .ok_or_else(|| anyhow::anyhow!("Unknown custom tool: {}", tool_name))?;

        let client_lock = self.client.lock().await;
        let client_arc = client_lock.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Custom MCP client not initialized"))?;

        // Tool name matches MCP server's tool name directly (no prefix stripping needed)
        let real_name = tool_name;

        debug!("Proxying {} → server '{}' tool '{}'", tool_name, server_name, real_name);
        let result = client_arc.lock().await.call_tool(server_name, real_name, arguments).await?;

        // Wrap in MCP content format
        let text = match result.get("content") {
            Some(content) => serde_json::to_string(content)?,
            None => serde_json::to_string(&result)?,
        };

        let is_error = result.get("isError").and_then(|v| v.as_bool()).unwrap_or(false);

        Ok(serde_json::json!({
            "content": [{"type": "text", "text": text}],
            "isError": is_error
        }))
    }
}

/// Parse workspace/tools/mcp.toml into McpServerConfig list.
/// Resolves relative commands against the tools/ directory.
fn parse_custom_mcp_config(content: &str, workspace_root: &Path) -> Result<Vec<McpServerConfig>> {
    #[derive(serde::Deserialize)]
    struct McpConfig {
        #[serde(default)]
        servers: Vec<ServerEntry>,
    }

    #[derive(serde::Deserialize)]
    struct ServerEntry {
        name: String,
        #[serde(default)]
        command: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
    }

    let config: McpConfig = toml::from_str(content)?;
    let tools_dir = workspace_root.join("tools");

    let mut configs = Vec::new();
    for server in config.servers {
        if server.command.is_empty() {
            warn!("Custom MCP server '{}' has no command, skipping", server.name);
            continue;
        }

        // Resolve the first command element relative to tools/ dir if it's a relative path
        let mut command = server.command;
        if !command[0].starts_with('/') && !command[0].contains("node") && !command[0].contains("python") {
            // It's a script name — resolve relative to tools/
            let resolved = tools_dir.join(&command[0]);
            if resolved.exists() {
                command[0] = resolved.to_string_lossy().to_string();
            }
        }

        // Always set CWD to tools/ directory via env
        let mut env = server.env;
        env.entry("PORTAL_TOOLS_DIR".to_string())
            .or_insert_with(|| tools_dir.to_string_lossy().to_string());

        configs.push(McpServerConfig {
            name: server.name,
            transport: McpTransport::Stdio { command, env },
        });
    }

    Ok(configs)
}
