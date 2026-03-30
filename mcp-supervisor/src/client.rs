use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::connection::{McpConnection, McpServerConfig, McpTransport};
use crate::protocol::McpToolInfo;

/// Client that manages connections to multiple MCP servers.
pub struct McpClient {
    connections: HashMap<String, McpConnection>,
    /// Original configs kept for reconnection
    configs: Vec<McpServerConfig>,
}

impl McpClient {
    /// Create a new MCP client and connect to all specified servers
    pub async fn connect_all(configs: Vec<McpServerConfig>) -> Result<Self> {
        let mut connections = HashMap::new();
        
        for config in &configs {
            let server_name = config.name.clone();
            
            info!("Connecting to MCP server '{}'", server_name);
            
            match McpConnection::connect(config.clone()).await {
                Ok(connection) => {
                    if let Err(e) = connection.initialize().await {
                        warn!("Failed to initialize MCP server '{}': {}", server_name, e);
                        continue;
                    }
                    
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

        Ok(Self { connections, configs })
    }

    /// Discover all tools from all connected servers
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
        
        info!("Discovered {} total tools from {} MCP servers", all_tools.len(), self.connections.len());
        all_tools
    }

    /// Call a tool on a specific server
    pub async fn call_tool(&self, server_name: &str, tool_name: &str, arguments: Value) -> Result<Value> {
        let connection = self.connections.get(server_name)
            .ok_or_else(|| anyhow::anyhow!("MCP server '{}' not connected", server_name))?;
        
        debug!("Calling tool '{}' on MCP server '{}' with args: {}", tool_name, server_name, arguments);
        
        connection.call_tool(tool_name, arguments).await
            .with_context(|| format!("Failed to call tool '{}' on MCP server '{}'", tool_name, server_name))
    }

    /// Get list of connected server names
    pub fn connected_servers(&self) -> Vec<String> {
        self.connections.keys().cloned().collect()
    }

    /// Check if a server is connected
    pub fn is_connected(&self, server_name: &str) -> bool {
        self.connections.contains_key(server_name)
    }

    /// Get number of connected servers
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Get configs for reconnection
    pub fn configs(&self) -> &[McpServerConfig] {
        &self.configs
    }

    /// Find TCP server configs that are disconnected or dead
    pub fn disconnected_tcp_servers(&mut self) -> Vec<McpServerConfig> {
        // First, remove dead connections (reader task exited)
        let dead: Vec<String> = self.connections.iter()
            .filter(|(_, conn)| !conn.is_alive())
            .map(|(name, _)| name.clone())
            .collect();
        
        for name in &dead {
            if let Some(mut conn) = self.connections.remove(name) {
                warn!("Removing dead connection '{}'", name);
                // Don't await shutdown — reader is already dead
                drop(conn);
            }
        }

        self.configs.iter()
            .filter(|c| matches!(&c.transport, McpTransport::Tcp { .. }))
            .filter(|c| !self.connections.contains_key(&c.name))
            .cloned()
            .collect()
    }

    /// Reconnect a single TCP server. Returns new tool list if successful.
    pub async fn reconnect_server(&mut self, config: &McpServerConfig) -> Result<Vec<McpToolInfo>> {
        let name = &config.name;
        
        // Remove old broken connection if present
        if let Some(mut old) = self.connections.remove(name.as_str()) {
            let _ = old.shutdown().await;
        }

        let connection = McpConnection::connect(config.clone()).await
            .with_context(|| format!("Reconnecting to '{}'", name))?;
        
        connection.initialize().await
            .with_context(|| format!("Reconnect initialize '{}'", name))?;
        
        let tools = connection.list_tools().await
            .with_context(|| format!("Reconnect list_tools '{}'", name))?;
        
        info!("Reconnected to '{}' — {} tools recovered", name, tools.len());
        self.connections.insert(name.clone(), connection);
        
        Ok(tools)
    }

    /// Shutdown all connections
    pub async fn shutdown_all(&mut self) -> Result<()> {
        info!("Shutting down all MCP connections");
        
        let server_names: Vec<_> = self.connections.keys().cloned().collect();
        
        for server_name in server_names {
            if let Some(mut connection) = self.connections.remove(&server_name) {
                if let Err(e) = connection.shutdown().await {
                    warn!("Error shutting down MCP server '{}': {}", server_name, e);
                } else {
                    debug!("MCP server '{}' shutdown successfully", server_name);
                }
            }
        }
        
        info!("All MCP connections shutdown");
        Ok(())
    }
}

/// Reconnect event: server name + newly discovered tools
pub struct ReconnectEvent {
    pub server_name: String,
    pub tools: Vec<McpToolInfo>,
}

/// Start a background reconnection loop.
/// Monitors an `Arc<Mutex<McpClient>>` for disconnected TCP servers and reconnects.
/// If `event_tx` is provided, sends ReconnectEvent on successful reconnection
/// so the caller can re-register tools.
pub fn start_reconnect_loop(client: Arc<Mutex<McpClient>>, interval: std::time::Duration) {
    start_reconnect_loop_with_events(client, interval, None);
}

/// Start reconnect loop with optional event channel for tool re-registration.
pub fn start_reconnect_loop_with_events(
    client: Arc<Mutex<McpClient>>,
    interval: std::time::Duration,
    event_tx: Option<tokio::sync::mpsc::Sender<ReconnectEvent>>,
) {
    tokio::spawn(async move {
        let mut backoff = interval;
        let max_backoff = std::time::Duration::from_secs(60);

        loop {
            tokio::time::sleep(backoff).await;

            let disconnected = {
                let mut c = client.lock().await;
                c.disconnected_tcp_servers()
            };

            if disconnected.is_empty() {
                backoff = interval; // all good, reset
                continue;
            }

            let mut any_success = false;
            for config in &disconnected {
                let mut c = client.lock().await;
                match c.reconnect_server(config).await {
                    Ok(tools) => {
                        info!("🔄 Reconnected '{}' — {} tools", config.name, tools.len());
                        
                        // Notify caller to re-register tools
                        if let Some(ref tx) = event_tx {
                            let _ = tx.send(ReconnectEvent {
                                server_name: config.name.clone(),
                                tools: tools.clone(),
                            }).await;
                        }
                        
                        any_success = true;
                    }
                    Err(e) => {
                        warn!("🔄 Reconnect '{}' failed: {}", config.name, e);
                    }
                }
            }

            if any_success {
                backoff = interval;
            } else {
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    });
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if !self.connections.is_empty() {
            warn!("McpClient dropped with {} active connections - connections may not be properly closed", 
                  self.connections.len());
        }
    }
}

/// Load MCP server configurations from a TOML file
pub async fn load_server_configs(config_path: &std::path::Path) -> Result<Vec<McpServerConfig>> {
    if !config_path.exists() {
        debug!("MCP config file {:?} does not exist, no servers to load", config_path);
        return Ok(Vec::new());
    }

    let config_content = tokio::fs::read_to_string(config_path).await
        .with_context(|| format!("Reading MCP config file {:?}", config_path))?;

    let parsed: toml::Value = toml::from_str(&config_content)
        .with_context(|| format!("Parsing MCP config file {:?}", config_path))?;

    let servers = parsed.get("servers")
        .and_then(|s| s.as_array())
        .ok_or_else(|| anyhow::anyhow!("MCP config file must contain a 'servers' array"))?;

    let mut configs = Vec::new();

    for server in servers {
        let name = server.get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| anyhow::anyhow!("Server entry missing 'name' field"))?;

        // Determine transport: TCP if host+port present, otherwise stdio
        let transport = if let (Some(host), Some(port)) = (
            server.get("host").and_then(|h| h.as_str()),
            server.get("port").and_then(|p| p.as_integer()),
        ) {
            McpTransport::Tcp {
                host: host.to_string(),
                port: port as u16,
            }
        } else {
            let command = server.get("command")
                .and_then(|c| c.as_array())
                .ok_or_else(|| anyhow::anyhow!("Server '{}' needs either host+port (TCP) or command (stdio)", name))?;

            let command: Result<Vec<String>, _> = command
                .iter()
                .map(|c| c.as_str()
                    .ok_or_else(|| anyhow::anyhow!("Command array must contain strings"))
                    .map(|s| s.to_string()))
                .collect();

            let mut env = HashMap::new();
            if let Some(env_table) = server.get("env").and_then(|e| e.as_table()) {
                for (key, value) in env_table {
                    let value_str = value.as_str()
                        .ok_or_else(|| anyhow::anyhow!("Environment variable '{}' must be a string", key))?;
                    env.insert(key.clone(), value_str.to_string());
                }
            }

            McpTransport::Stdio {
                command: command?,
                env,
            }
        };

        configs.push(McpServerConfig {
            name: name.to_string(),
            transport,
        });
    }

    info!("Loaded {} MCP server configurations from {:?}", configs.len(), config_path);
    Ok(configs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_mcp_client_empty() {
        let client = McpClient::connect_all(vec![]).await.unwrap();
        assert_eq!(client.connection_count(), 0);
        assert!(client.connected_servers().is_empty());
    }

    #[tokio::test]
    async fn test_mcp_client_invalid_server() {
        let config = McpServerConfig::stdio(
            "invalid".to_string(),
            vec!["nonexistent_command_12345".to_string()],
            HashMap::new(),
        );

        let client = McpClient::connect_all(vec![config]).await.unwrap();
        
        // Should succeed but with no connections
        assert_eq!(client.connection_count(), 0);
        assert!(!client.is_connected("invalid"));
    }

    #[tokio::test]
    async fn test_load_server_configs_nonexistent() {
        let path = std::path::Path::new("/nonexistent/path/mcp-servers.toml");
        let configs = load_server_configs(path).await.unwrap();
        assert!(configs.is_empty());
    }

    #[tokio::test]
    async fn test_load_server_configs() {
        let mut temp_file = NamedTempFile::new().unwrap();
        let config_content = r#"
[[servers]]
name = "example"
command = ["node", "example-server.js"]

[servers.env]
API_KEY = "test123"

[[servers]]
name = "another"
command = ["python", "server.py"]

[[servers]]
name = "remote-workspace"
host = "192.168.1.100"
port = 9000
"#;
        
        std::io::Write::write_all(&mut temp_file, config_content.as_bytes()).unwrap();
        std::io::Write::flush(&mut temp_file).unwrap();

        let configs = load_server_configs(temp_file.path()).await.unwrap();
        
        assert_eq!(configs.len(), 3);
        
        let first = &configs[0];
        assert_eq!(first.name, "example");
        assert!(matches!(&first.transport, McpTransport::Stdio { command, env }
            if command == &vec!["node", "example-server.js"]
            && env.get("API_KEY") == Some(&"test123".to_string())
        ));

        let second = &configs[1];
        assert_eq!(second.name, "another");
        assert!(matches!(&second.transport, McpTransport::Stdio { command, .. }
            if command == &vec!["python", "server.py"]
        ));

        let third = &configs[2];
        assert_eq!(third.name, "remote-workspace");
        assert!(matches!(&third.transport, McpTransport::Tcp { host, port }
            if host == "192.168.1.100" && *port == 9000
        ));
    }

    #[tokio::test]
    async fn test_load_server_configs_invalid_toml() {
        let mut temp_file = NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut temp_file, b"invalid toml content [[[").unwrap();
        std::io::Write::flush(&mut temp_file).unwrap();

        let result = load_server_configs(temp_file.path()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_load_server_configs_missing_fields() {
        let mut temp_file = NamedTempFile::new().unwrap();
        let config_content = r#"
[[servers]]
# missing name
command = ["node", "server.js"]
"#;
        
        std::io::Write::write_all(&mut temp_file, config_content.as_bytes()).unwrap();
        std::io::Write::flush(&mut temp_file).unwrap();

        let result = load_server_configs(temp_file.path()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_load_server_configs_array_format() {
        let dir = std::env::temp_dir().join("heart_test_mcp");
        std::fs::create_dir_all(&dir).ok();
        let config_path = dir.join("mcp-servers.toml");
        std::fs::write(&config_path, r#"
[[servers]]
name = "workspace"
command = ["node", "/some/path/index.js"]
"#).unwrap();
        
        let result = load_server_configs(&config_path).await;
        println!("Result: {:?}", result);
        let configs = result.unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "workspace");
        std::fs::remove_dir_all(&dir).ok();
    }
}
