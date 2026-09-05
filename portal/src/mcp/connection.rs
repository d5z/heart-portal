use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::Child;
use tokio::sync::{oneshot, Mutex};
use tracing::{debug, error, warn};

use super::protocol::{JsonRpcRequest, JsonRpcResponse, McpToolInfo};

/// Default timeout for MCP handshake and metadata requests (initialize, tools/list).
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(30);
/// Timeout for tool calls — tools may run for minutes (code review, web fetch, etc.).
const TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(600);

/// Configuration for a stdio MCP server process.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub command: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: Option<PathBuf>,
}

/// A stdio JSON-RPC connection to a single MCP server.
pub struct McpConnection {
    child: Option<Child>,
    writer: Arc<Mutex<BufWriter<Box<dyn AsyncWrite + Send + Unpin>>>>,
    responses: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
    next_id: AtomicU64,
    config: McpServerConfig,
    alive: Arc<AtomicBool>,
}

impl McpConnection {
    /// Spawn and initialize a stdio MCP server.
    pub async fn spawn(config: McpServerConfig) -> Result<Self> {
        if config.command.is_empty() {
            anyhow::bail!("Empty command for MCP server '{}'", config.name);
        }

        // Rust handles .cmd/.bat invocation and Windows argument escaping.
        // An extra `cmd /C call` layer would expand arguments a second time.
        let mut command = tokio::process::Command::new(&config.command[0]);
        command.args(&config.command[1..]);
        #[cfg(windows)]
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW

        command
            .envs(&config.env)
            .env_remove("HEART_PORTAL_SUPERVISED")
            .env_remove("PORTAL_CONNECT_LINK")
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // Preserve kit diagnostics. A kit can fail while Portal remains
            // healthy; forwarding stderr makes the actual startup/exit cause
            // visible in Portal logs instead of only reporting a closed RPC
            // response channel.
            .stderr(std::process::Stdio::piped());

        if let Some(cwd) = &config.cwd {
            command.current_dir(cwd);
        }

        let mut child = command.spawn().with_context(|| {
            format!(
                "Failed to spawn MCP server '{}' with command: {:?}",
                config.name, config.command
            )
        })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            anyhow::anyhow!("Failed to get stdin for MCP server '{}'", config.name)
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            anyhow::anyhow!("Failed to get stdout for MCP server '{}'", config.name)
        })?;
        let stderr = child.stderr.take();

        let responses = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let connection = Self {
            child: Some(child),
            writer: Arc::new(Mutex::new(BufWriter::new(Box::new(stdin)))),
            responses: responses.clone(),
            next_id: AtomicU64::new(1),
            config,
            alive: alive.clone(),
        };

        let server_name = connection.config.name.clone();
        tokio::spawn(async move {
            if let Err(e) =
                Self::reader_task(BufReader::new(stdout), responses, alive, &server_name).await
            {
                error!("MCP server '{}' reader failed: {}", server_name, e);
            }
        });

        if let Some(stderr) = stderr {
            let server_name = connection.config.name.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = Vec::new();
                loop {
                    line.clear();
                    match reader.read_until(b'\n', &mut line).await {
                        Ok(0) => break,
                        Ok(_) => {
                            // Windows command shims may write using the OEM
                            // code page rather than UTF-8. Preserve useful
                            // diagnostics instead of aborting the stderr task.
                            let decoded = String::from_utf8_lossy(&line);
                            let message = decoded.trim_end();
                            if !message.is_empty() {
                                warn!("MCP server '{}' stderr: {}", server_name, message);
                            }
                        }
                        Err(err) => {
                            warn!("MCP server '{}' stderr read failed: {}", server_name, err);
                            break;
                        }
                    }
                }
            });
        }

        if let Err(e) = connection.initialize().await {
            let mut connection = connection;
            let _ = connection.shutdown().await;
            return Err(e);
        }

        debug!(
            "MCP server '{}' spawned and initialized",
            connection.config.name
        );
        Ok(connection)
    }

    async fn reader_task<R>(
        reader: BufReader<R>,
        responses: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
        alive: Arc<AtomicBool>,
        server_name: &str,
    ) -> Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        let result = Self::read_responses(reader, responses.clone(), server_name).await;
        // Mark dead before waking callers, on both EOF and read errors. Otherwise
        // a broken pipe/invalid UTF-8 can leave tool calls waiting for ten minutes.
        alive.store(false, Ordering::SeqCst);
        Self::drop_pending(responses, server_name).await;
        result
    }

    async fn read_responses<R>(
        mut reader: BufReader<R>,
        responses: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
        server_name: &str,
    ) -> Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        let mut line = String::new();

        loop {
            line.clear();
            let bytes_read = reader
                .read_line(&mut line)
                .await
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
                    warn!(
                        "MCP server '{}' sent invalid JSON: {} (line: {})",
                        server_name, e, trimmed
                    );
                    continue;
                }
            };

            if let Some(id) = response.id {
                let mut pending = responses.lock().await;
                if let Some(sender) = pending.remove(&id) {
                    if sender.send(response).is_err() {
                        warn!(
                            "MCP server '{}' response receiver dropped for id {}",
                            server_name, id
                        );
                    }
                } else {
                    warn!(
                        "MCP server '{}' sent response for unknown id {}",
                        server_name, id
                    );
                }
            } else {
                debug!(
                    "MCP server '{}' sent notification: {}",
                    server_name, trimmed
                );
            }
        }

        Ok(())
    }

    async fn drop_pending(
        responses: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
        server_name: &str,
    ) {
        let mut pending = responses.lock().await;
        if !pending.is_empty() {
            warn!(
                "MCP server '{}' reader closed: dropping {} pending response(s)",
                server_name,
                pending.len()
            );
        }
        pending.clear();
    }

    async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let request = JsonRpcRequest::new(method, params, &self.next_id);
        let request_id = request
            .id
            .ok_or_else(|| anyhow::anyhow!("MCP request missing id"))?;
        let (tx, rx) = oneshot::channel();

        {
            let mut pending = self.responses.lock().await;
            // The reader marks itself dead before clearing this same map.
            // Do not enqueue after EOF cleanup: no reader remains to wake us.
            if !self.is_alive() {
                anyhow::bail!("MCP server '{}' has closed stdout", self.config.name);
            }
            pending.insert(request_id, tx);
        }

        let request_json = serde_json::to_string(&request).with_context(|| {
            format!("Serializing request for MCP server '{}'", self.config.name)
        })?;

        debug!(
            "MCP server '{}' request: {}",
            self.config.name, request_json
        );

        {
            let mut writer = self.writer.lock().await;
            let write_result = async {
                writer
                    .write_all(request_json.as_bytes())
                    .await
                    .with_context(|| format!("Writing to MCP server '{}'", self.config.name))?;
                writer.write_all(b"\n").await.with_context(|| {
                    format!("Writing newline to MCP server '{}'", self.config.name)
                })?;
                writer
                    .flush()
                    .await
                    .with_context(|| format!("Flushing MCP server '{}'", self.config.name))?;
                anyhow::Ok(())
            }
            .await;

            if let Err(e) = write_result {
                self.responses.lock().await.remove(&request_id);
                return Err(e);
            }
        }

        let response = match tokio::time::timeout(timeout, rx).await {
            Ok(result) => result.with_context(|| {
                let state = if self.is_alive() {
                    "response receiver was dropped"
                } else {
                    "the kit process exited or closed stdout"
                };
                format!(
                    "Response channel closed for MCP server '{}': {}",
                    self.config.name, state
                )
            })?,
            Err(_) => {
                // Clean up pending request on timeout
                self.responses.lock().await.remove(&request_id);
                anyhow::bail!(
                    "Timeout ({}s) waiting for response from MCP server '{}'",
                    timeout.as_secs(),
                    self.config.name
                );
            }
        };

        if let Some(error) = response.error {
            anyhow::bail!(
                "MCP server '{}' returned error: {} (code: {})",
                self.config.name,
                error.message,
                error.code
            );
        }

        response.result.ok_or_else(|| {
            anyhow::anyhow!(
                "MCP server '{}' response missing result field",
                self.config.name
            )
        })
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_with_timeout(method, params, DEFAULT_RPC_TIMEOUT)
            .await
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let request = JsonRpcRequest::notification(method, params);
        let request_json = serde_json::to_string(&request).with_context(|| {
            format!(
                "Serializing notification for MCP server '{}'",
                self.config.name
            )
        })?;

        debug!(
            "MCP server '{}' notification: {}",
            self.config.name, request_json
        );

        let mut writer = self.writer.lock().await;
        writer
            .write_all(request_json.as_bytes())
            .await
            .with_context(|| {
                format!("Writing notification to MCP server '{}'", self.config.name)
            })?;
        writer
            .write_all(b"\n")
            .await
            .with_context(|| format!("Writing newline to MCP server '{}'", self.config.name))?;
        writer.flush().await.with_context(|| {
            format!(
                "Flushing MCP server '{}' after notification",
                self.config.name
            )
        })?;

        Ok(())
    }

    async fn initialize(&self) -> Result<()> {
        debug!("Initializing MCP server '{}'", self.config.name);

        let init_result = self
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "clientInfo": {
                        "name": "heart-cortex",
                        "version": "1.0.0"
                    }
                }),
            )
            .await?;

        debug!(
            "MCP server '{}' initialize result: {}",
            self.config.name, init_result
        );

        self.notify("notifications/initialized", serde_json::json!({}))
            .await?;

        debug!("MCP server '{}' initialization complete", self.config.name);
        Ok(())
    }

    /// Get the list of tools from this server.
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>> {
        let result = self.request("tools/list", serde_json::json!({})).await?;

        let tools = result["tools"].as_array().ok_or_else(|| {
            anyhow::anyhow!(
                "MCP server '{}' tools/list response missing 'tools' array",
                self.config.name
            )
        })?;

        let mut parsed_tools = Vec::new();
        for tool in tools {
            let tool_info: McpToolInfo =
                serde_json::from_value(tool.clone()).with_context(|| {
                    format!("Parsing tool info from MCP server '{}'", self.config.name)
                })?;
            parsed_tools.push(tool_info);
        }

        debug!(
            "MCP server '{}' provides {} tools",
            self.config.name,
            parsed_tools.len()
        );
        Ok(parsed_tools)
    }

    /// Call a tool on this server.
    pub async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<Value> {
        debug!(
            "Calling tool '{}' on MCP server '{}'",
            tool_name, self.config.name
        );

        let result = self
            .request_with_timeout(
                "tools/call",
                serde_json::json!({
                    "name": tool_name,
                    "arguments": arguments
                }),
                TOOL_CALL_TIMEOUT,
            )
            .await?;

        debug!(
            "Tool '{}' on MCP server '{}' returned: {}",
            tool_name, self.config.name, result
        );
        Ok(result)
    }

    /// Check whether the connection reader task is still alive.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Shutdown the child process.
    pub async fn shutdown(&mut self) -> Result<()> {
        debug!("Shutting down MCP server '{}'", self.config.name);

        if let Some(ref mut child) = self.child {
            terminate_child(child, &self.config.name).await;

            match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
                Ok(Ok(status)) => {
                    debug!(
                        "MCP server '{}' process exited with status: {}",
                        self.config.name, status
                    );
                }
                Ok(Err(e)) => {
                    warn!(
                        "Error waiting for MCP server '{}' process: {}",
                        self.config.name, e
                    );
                }
                Err(_) => {
                    warn!(
                        "Timeout waiting for MCP server '{}' process to exit",
                        self.config.name
                    );
                    if let Err(e) = child.kill().await {
                        warn!(
                            "Failed to kill MCP server '{}' process: {}",
                            self.config.name, e
                        );
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(unix)]
async fn terminate_child(child: &mut Child, server_name: &str) {
    let Some(pid) = child.id() else {
        return;
    };

    let rc = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    if rc == 0 {
        debug!("Sent SIGTERM to MCP server '{}'", server_name);
    } else {
        warn!(
            "Failed to send SIGTERM to MCP server '{}': {}",
            server_name,
            std::io::Error::last_os_error()
        );
    }
}

#[cfg(windows)]
async fn terminate_child(child: &mut Child, server_name: &str) {
    if let Some(pid) = child.id() {
        match tokio::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW, including cleanup
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
        {
            Ok(status) if status.success() => {
                debug!("Sent taskkill to MCP server '{}'", server_name);
                return;
            }
            Ok(status) => warn!(
                "taskkill for MCP server '{}' exited with status: {}",
                server_name, status
            ),
            Err(e) => warn!(
                "Failed to run taskkill for MCP server '{}': {}",
                server_name, e
            ),
        }
    }

    match child.kill().await {
        Ok(_) => debug!("MCP server '{}' process killed", server_name),
        Err(e) => warn!("Failed to kill MCP server '{}' process: {}", server_name, e),
    }
}

#[cfg(not(any(unix, windows)))]
async fn terminate_child(child: &mut Child, server_name: &str) {
    match child.kill().await {
        Ok(_) => debug!("MCP server '{}' process killed", server_name),
        Err(e) => warn!("Failed to kill MCP server '{}' process: {}", server_name, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_after_reader_exit_fails_without_waiting_for_timeout() {
        let connection = McpConnection {
            child: None,
            writer: Arc::new(Mutex::new(BufWriter::new(Box::new(tokio::io::sink())))),
            responses: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            config: McpServerConfig {
                name: "closed-reader".into(),
                command: vec![],
                env: HashMap::new(),
                cwd: None,
            },
            alive: Arc::new(AtomicBool::new(false)),
        };
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            connection.call_tool("ping", serde_json::json!({})),
        )
        .await
        .expect("a closed reader must not leave calls pending");
        assert!(result.unwrap_err().to_string().contains("closed stdout"));
        assert!(connection.responses.lock().await.is_empty());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn cmd_kit_handles_spaces_and_literal_metacharacters() {
        let dir = std::env::temp_dir().join(format!("portal kit ({})", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let script = dir.join("kit shim.cmd");
        std::fs::write(
            &script,
            concat!(
                "@echo off\r\n",
                "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"%~dp0server.ps1\" \"%~1\"\r\n",
            ),
        )
        .unwrap();
        // cmd's `set /p` can discard buffered piped input. Use an actual line
        // reader so the initialized notification and next request can coalesce.
        std::fs::write(
            dir.join("server.ps1"),
            r#"
param([string]$Value)
while ($null -ne ($line = [Console]::ReadLine())) {
    $request = $line | ConvertFrom-Json
    if ($null -ne $request.id) {
        @{ jsonrpc = '2.0'; id = $request.id; result = @{ argument = $Value } } |
            ConvertTo-Json -Compress -Depth 5
    }
}
"#,
        )
        .unwrap();
        let mut connection = McpConnection::spawn(McpServerConfig {
            name: "cmd-test".into(),
            command: vec![
                script.to_string_lossy().into_owned(),
                "space & value".into(),
            ],
            env: HashMap::new(),
            cwd: Some(dir.clone()),
        })
        .await
        .unwrap();
        let result = connection.request("ping", serde_json::json!({})).await;
        connection.shutdown().await.unwrap();
        assert_eq!(result.unwrap()["argument"], "space & value");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn closed_reader_drops_pending_on_eof_and_read_error() {
        for input in [&b""[..], &b"\xff\n"[..]] {
            let (sender, receiver) = oneshot::channel();
            let responses = Arc::new(Mutex::new(HashMap::from([(1, sender)])));
            let alive = Arc::new(AtomicBool::new(true));
            let result = McpConnection::reader_task(
                BufReader::new(input),
                responses.clone(),
                alive.clone(),
                "test",
            )
            .await;
            assert_eq!(result.is_err(), !input.is_empty());
            assert!(!alive.load(Ordering::SeqCst));
            assert!(responses.lock().await.is_empty());
            assert!(receiver.await.is_err());
        }
    }
}
