use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use tracing::{debug, info, warn};

use super::connection::{McpConnection, McpServerConfig};
use super::protocol::McpToolInfo;

/// Client that manages connections to multiple custom MCP servers.
pub struct McpClient {
    connections: HashMap<String, McpConnection>,
}

impl McpClient {
    /// Create a new MCP client and connect to all specified servers.
    pub async fn connect_all(configs: Vec<McpServerConfig>) -> Result<Self> {
        let mut connections = HashMap::new();

        for config in configs {
            let server_name = config.name.clone();
            info!("Connecting to MCP server '{}'", server_name);

            match McpConnection::spawn(config).await {
                Ok(connection) => {
                    connections.insert(server_name.clone(), connection);
                    info!("Successfully connected to MCP server '{}'", server_name);
                }
                Err(e) => {
                    warn!("Failed to connect to MCP server '{}': {}", server_name, e);
                }
            }
        }

        if connections.is_empty() {
            warn!("No MCP servers connected successfully");
        } else {
            info!("MCP client connected to {} servers", connections.len());
        }

        Ok(Self { connections })
    }

    /// Get number of connected servers.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Discover all tools from all connected servers.
    pub async fn discover_tools(&self) -> Vec<(String, McpToolInfo)> {
        let mut all_tools = Vec::new();

        for (server_name, connection) in &self.connections {
            debug!("Discovering tools from MCP server '{}'", server_name);

            match connection.list_tools().await {
                Ok(tools) => {
                    info!("MCP server '{}' provides {} tools", server_name, tools.len());
                    for tool in tools {
                        debug!("  - {}: {}", tool.name, tool.description);
                        all_tools.push((server_name.clone(), tool));
                    }
                }
                Err(e) => {
                    warn!("Failed to list tools from MCP server '{}': {}", server_name, e);
                }
            }
        }

        info!(
            "Discovered {} total tools from {} MCP servers",
            all_tools.len(),
            self.connections.len()
        );
        all_tools
    }

    /// Call a tool on a specific server.
    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: Value,
    ) -> Result<Value> {
        let connection = self
            .connections
            .get(server_name)
            .ok_or_else(|| anyhow::anyhow!("MCP server '{}' not connected", server_name))?;

        debug!(
            "Calling tool '{}' on MCP server '{}' with args: {}",
            tool_name, server_name, arguments
        );

        connection
            .call_tool(tool_name, arguments)
            .await
            .with_context(|| {
                format!(
                    "Failed to call tool '{}' on MCP server '{}'",
                    tool_name, server_name
                )
            })
    }
}
