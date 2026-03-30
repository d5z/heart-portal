use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::Child;
use tokio::sync::{oneshot, Mutex};
use tracing::{debug, error, info, warn};

use crate::protocol::{JsonRpcRequest, JsonRpcResponse};

/// Transport configuration for an MCP server
#[derive(Debug, Clone)]
pub enum McpTransport {
    /// Spawn a child process and communicate via stdio
    Stdio {
        command: Vec<String>,
        env: HashMap<String, String>,
    },
    /// Connect to an already-running server via TCP
    Tcp {
        host: String,
        port: u16,
    },
}

/// Configuration for an MCP server
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
}

// Backward compat: keep command/env fields for existing code that constructs McpServerConfig directly
impl McpServerConfig {
    /// Create a stdio-based config (backward compatible)
    pub fn stdio(name: String, command: Vec<String>, env: HashMap<String, String>) -> Self {
        Self {
            name,
            transport: McpTransport::Stdio { command, env },
        }
    }

    /// Create a TCP-based config
    pub fn tcp(name: String, host: String, port: u16) -> Self {
        Self {
            name,
            transport: McpTransport::Tcp { host, port },
        }
    }
}

/// A connection to a single MCP server (via stdio or TCP)
pub struct McpConnection {
    /// Child process handle (only for stdio transport, None for TCP)
    child: Option<Child>,
    /// Writer half (works for both stdio and TCP)
    writer: Arc<Mutex<BufWriter<Box<dyn AsyncWrite + Send + Unpin>>>>,
    /// Pending responses waiting for their JSON-RPC response
    responses: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
    /// Counter for generating unique request IDs
    next_id: AtomicU64,
    /// Server config for debugging
    config: McpServerConfig,
    /// Whether the reader task is still alive (set to false on disconnect)
    alive: Arc<std::sync::atomic::AtomicBool>,
}

impl McpConnection {
    /// Connect to an MCP server using the configured transport
    pub async fn connect(config: McpServerConfig) -> Result<Self> {
        match &config.transport {
            McpTransport::Stdio { command, env } => {
                Self::spawn_stdio(config.name.clone(), command.clone(), env.clone()).await
            }
            McpTransport::Tcp { host, port } => {
                Self::connect_tcp_inner(&config.name, host, *port).await
                    .map(|mut conn| { conn.config = config; conn })
            }
        }
    }

    /// Spawn a stdio-based MCP server (original behavior)
    pub async fn spawn(config: McpServerConfig) -> Result<Self> {
        match &config.transport {
            McpTransport::Stdio { command, env } => {
                Self::spawn_stdio(config.name.clone(), command.clone(), env.clone()).await
            }
            McpTransport::Tcp { .. } => {
                anyhow::bail!("spawn() called on TCP config — use connect() instead")
            }
        }
    }

    async fn spawn_stdio(name: String, command: Vec<String>, env: HashMap<String, String>) -> Result<Self> {
        if command.is_empty() {
            anyhow::bail!("Empty command for MCP server '{}'", name);
        }

        let mut child = tokio::process::Command::new(&command[0])
            .args(&command[1..])
            .envs(&env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("Failed to spawn MCP server '{}' with command: {:?}", name, command))?;

        let stdin = child.stdin.take()
            .ok_or_else(|| anyhow::anyhow!("Failed to get stdin for MCP server '{}'", name))?;
        let stdout = child.stdout.take()
            .ok_or_else(|| anyhow::anyhow!("Failed to get stdout for MCP server '{}'", name))?;

        let responses = Arc::new(Mutex::new(HashMap::new()));

        let writer: Box<dyn AsyncWrite + Send + Unpin> = Box::new(stdin);
        let reader: Box<dyn AsyncRead + Send + Unpin> = Box::new(stdout);

        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let connection = Self {
            child: Some(child),
            writer: Arc::new(Mutex::new(BufWriter::new(writer))),
            responses: responses.clone(),
            next_id: AtomicU64::new(1),
            config: McpServerConfig::stdio(name.clone(), command, env),
            alive: alive.clone(),
        };

        // Spawn reader task
        let server_name = name.clone();
        tokio::spawn(async move {
            if let Err(e) = Self::reader_task(reader, responses, &server_name).await {
                error!("MCP server '{}' reader failed: {}", server_name, e);
            }
            alive.store(false, std::sync::atomic::Ordering::SeqCst);
        });

        debug!("MCP server '{}' spawned (stdio)", name);
        Ok(connection)
    }

    /// Connect to an MCP server via TCP
    pub async fn connect_tcp(name: &str, host: &str, port: u16) -> Result<Self> {
        Self::connect_tcp_inner(name, host, port).await
    }

    async fn connect_tcp_inner(name: &str, host: &str, port: u16) -> Result<Self> {
        let addr = format!("{}:{}", host, port);
        info!("Connecting to MCP server '{}' via TCP at {}", name, addr);

        let stream = tokio::net::TcpStream::connect(&addr).await
            .with_context(|| format!("Failed to connect to MCP server '{}' at {}", name, addr))?;

        let (read_half, write_half) = stream.into_split();

        let responses = Arc::new(Mutex::new(HashMap::new()));

        let writer: Box<dyn AsyncWrite + Send + Unpin> = Box::new(write_half);
        let reader: Box<dyn AsyncRead + Send + Unpin> = Box::new(read_half);

        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let connection = Self {
            child: None,
            writer: Arc::new(Mutex::new(BufWriter::new(writer))),
            responses: responses.clone(),
            next_id: AtomicU64::new(1),
            config: McpServerConfig::tcp(name.to_string(), host.to_string(), port),
            alive: alive.clone(),
        };

        // Spawn reader task
        let server_name = name.to_string();
        tokio::spawn(async move {
            if let Err(e) = Self::reader_task(reader, responses, &server_name).await {
                error!("MCP server '{}' TCP reader failed: {}", server_name, e);
            }
            alive.store(false, std::sync::atomic::Ordering::SeqCst);
            warn!("MCP server '{}' reader exited — connection dead", server_name);
        });

        info!("MCP server '{}' connected via TCP at {}", name, addr);
        Ok(connection)
    }

    /// Background task that reads JSON-RPC responses (works for both stdio and TCP)
    async fn reader_task(
        reader: Box<dyn AsyncRead + Send + Unpin>,
        responses: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
        server_name: &str,
    ) -> Result<()> {
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();

        loop {
            line.clear();
            let bytes_read = buf_reader.read_line(&mut line).await
                .with_context(|| format!("Reading from MCP server '{}'", server_name))?;

            if bytes_read == 0 {
                debug!("MCP server '{}' connection closed", server_name);
                break;
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            debug!("MCP server '{}' response: {}", server_name, trimmed);

            let response: JsonRpcResponse = match serde_json::from_str(trimmed) {
                Ok(resp) => resp,
                Err(e) => {
                    warn!("MCP server '{}' sent invalid JSON: {} (line: {})", server_name, e, trimmed);
                    continue;
                }
            };

            if let Some(id) = response.id {
                let mut pending = responses.lock().await;
                if let Some(sender) = pending.remove(&id) {
                    if let Err(_) = sender.send(response) {
                        warn!("MCP server '{}' response receiver dropped for id {}", server_name, id);
                    }
                } else {
                    warn!("MCP server '{}' sent response for unknown id {}", server_name, id);
                }
            } else {
                debug!("MCP server '{}' sent notification: {}", server_name, trimmed);
            }
        }

        Ok(())
    }

    /// Send a JSON-RPC request and wait for response
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let request = JsonRpcRequest::new(method, params, &self.next_id);
        let request_id = request.id.unwrap();

        let (tx, rx) = oneshot::channel();

        {
            let mut pending = self.responses.lock().await;
            pending.insert(request_id, tx);
        }

        let request_json = serde_json::to_string(&request)
            .with_context(|| format!("Serializing request for MCP server '{}'", self.config.name))?;

        debug!("MCP server '{}' request: {}", self.config.name, request_json);

        {
            let mut writer = self.writer.lock().await;
            writer.write_all(request_json.as_bytes()).await
                .with_context(|| format!("Writing to MCP server '{}'", self.config.name))?;
            writer.write_all(b"\n").await
                .with_context(|| format!("Writing newline to MCP server '{}'", self.config.name))?;
            writer.flush().await
                .with_context(|| format!("Flushing MCP server '{}'", self.config.name))?;
        }

        let response = tokio::time::timeout(std::time::Duration::from_secs(30), rx).await
            .with_context(|| format!("Timeout waiting for response from MCP server '{}'", self.config.name))?
            .with_context(|| format!("Response channel closed for MCP server '{}'", self.config.name))?;

        if let Some(error) = response.error {
            anyhow::bail!("MCP server '{}' returned error: {} (code: {})",
                self.config.name, error.message, error.code);
        }

        response.result
            .ok_or_else(|| anyhow::anyhow!("MCP server '{}' response missing result field", self.config.name))
    }

    /// Send a JSON-RPC notification (no response expected)
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let request = JsonRpcRequest::notification(method, params);

        let request_json = serde_json::to_string(&request)
            .with_context(|| format!("Serializing notification for MCP server '{}'", self.config.name))?;

        debug!("MCP server '{}' notification: {}", self.config.name, request_json);

        let mut writer = self.writer.lock().await;
        writer.write_all(request_json.as_bytes()).await
            .with_context(|| format!("Writing notification to MCP server '{}'", self.config.name))?;
        writer.write_all(b"\n").await
            .with_context(|| format!("Writing newline to MCP server '{}'", self.config.name))?;
        writer.flush().await
            .with_context(|| format!("Flushing MCP server '{}' after notification", self.config.name))?;

        Ok(())
    }

    /// Initialize the MCP connection with handshake
    pub async fn initialize(&self) -> Result<()> {
        debug!("Initializing MCP server '{}'", self.config.name);

        let init_result = self.request("initialize", serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "clientInfo": {
                "name": "heart-cortex",
                "version": "1.0.0"
            }
        })).await?;

        debug!("MCP server '{}' initialize result: {}", self.config.name, init_result);

        self.notify("notifications/initialized", serde_json::json!({})).await?;

        debug!("MCP server '{}' initialization complete", self.config.name);
        Ok(())
    }

    /// Get the list of tools from this server
    pub async fn list_tools(&self) -> Result<Vec<crate::protocol::McpToolInfo>> {
        let result = self.request("tools/list", serde_json::json!({})).await?;

        let tools = result["tools"].as_array()
            .ok_or_else(|| anyhow::anyhow!("MCP server '{}' tools/list response missing 'tools' array", self.config.name))?;

        let mut parsed_tools = Vec::new();
        for tool in tools {
            let tool_info: crate::protocol::McpToolInfo = serde_json::from_value(tool.clone())
                .with_context(|| format!("Parsing tool info from MCP server '{}'", self.config.name))?;
            parsed_tools.push(tool_info);
        }

        debug!("MCP server '{}' provides {} tools", self.config.name, parsed_tools.len());
        Ok(parsed_tools)
    }

    /// Call a tool on this server
    pub async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<Value> {
        debug!("Calling tool '{}' on MCP server '{}'", tool_name, self.config.name);

        let result = self.request("tools/call", serde_json::json!({
            "name": tool_name,
            "arguments": arguments
        })).await?;

        debug!("Tool '{}' on MCP server '{}' returned: {}", tool_name, self.config.name, result);
        Ok(result)
    }

    /// Check if the connection's reader task is still alive
    pub fn is_alive(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Shutdown the connection gracefully
    pub async fn shutdown(&mut self) -> Result<()> {
        debug!("Shutting down MCP server '{}'", self.config.name);

        if let Some(ref mut child) = self.child {
            match child.kill().await {
                Ok(_) => {
                    debug!("MCP server '{}' process killed", self.config.name);
                }
                Err(e) => {
                    warn!("Failed to kill MCP server '{}' process: {}", self.config.name, e);
                }
            }

            match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
                Ok(Ok(status)) => {
                    debug!("MCP server '{}' process exited with status: {}", self.config.name, status);
                }
                Ok(Err(e)) => {
                    warn!("Error waiting for MCP server '{}' process: {}", self.config.name, e);
                }
                Err(_) => {
                    warn!("Timeout waiting for MCP server '{}' process to exit", self.config.name);
                }
            }
        } else {
            // TCP connection — just drop the writer (which closes the socket)
            debug!("MCP server '{}' TCP connection closed", self.config.name);
        }

        Ok(())
    }

    /// Get server name for debugging
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// Check if this is a TCP connection
    pub fn is_tcp(&self) -> bool {
        matches!(self.config.transport, McpTransport::Tcp { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_connection_spawn_stdio() {
        let config = McpServerConfig::stdio(
            "test".to_string(),
            vec!["cat".to_string()],
            HashMap::new(),
        );

        let mut connection = McpConnection::connect(config).await.unwrap();
        assert!(!connection.is_tcp());
        connection.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_connection_invalid_command() {
        let config = McpServerConfig::stdio(
            "test".to_string(),
            vec!["nonexistent_command_12345".to_string()],
            HashMap::new(),
        );

        let result = McpConnection::connect(config).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_tcp_connection_refused() {
        // Try connecting to a port that's not listening
        let config = McpServerConfig::tcp(
            "test".to_string(),
            "127.0.0.1".to_string(),
            19999,
        );

        let result = McpConnection::connect(config).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_tcp_roundtrip() {
        // Start a simple echo TCP server
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Server task: read a line, parse as JSON-RPC request, return a success response
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut line = String::new();

            while reader.read_line(&mut line).await.unwrap() > 0 {
                let trimmed = line.trim();
                if trimmed.is_empty() { line.clear(); continue; }

                // Parse request and echo back a success response
                if let Ok(req) = serde_json::from_str::<JsonRpcRequest>(trimmed) {
                    let response = JsonRpcResponse::success(req.id, serde_json::json!({
                        "echo": req.params
                    }));
                    let resp_json = serde_json::to_string(&response).unwrap();
                    write_half.write_all(resp_json.as_bytes()).await.unwrap();
                    write_half.write_all(b"\n").await.unwrap();
                    write_half.flush().await.unwrap();
                }
                line.clear();
            }
        });

        // Client: connect via TCP and send a request
        let conn = McpConnection::connect_tcp("test-echo", "127.0.0.1", addr.port()).await.unwrap();
        assert!(conn.is_tcp());

        let result = conn.request("test/echo", serde_json::json!({"hello": "world"})).await.unwrap();
        assert_eq!(result["echo"]["hello"], "world");
    }

    #[test]
    fn test_server_config_constructors() {
        let stdio = McpServerConfig::stdio(
            "s1".into(),
            vec!["node".into(), "server.js".into()],
            [("KEY".into(), "val".into())].into(),
        );
        assert!(matches!(stdio.transport, McpTransport::Stdio { .. }));

        let tcp = McpServerConfig::tcp("s2".into(), "localhost".into(), 8080);
        assert!(matches!(tcp.transport, McpTransport::Tcp { host, port } if host == "localhost" && port == 8080));
    }
}
