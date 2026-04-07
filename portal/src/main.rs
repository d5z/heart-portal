//! Heart Portal — Being's gateway to the world.
//! 
//! A lightweight MCP server with built-in tools (exec, file, web).
//! Heart's MCP supervisor connects to Portal via TCP.
//! Portal can run on Town Home, a human's laptop, or anywhere.

mod config;
mod tools_flat;
mod protocol;
mod extensions;

use std::path::PathBuf;
use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpListener;
use tracing::{info, warn, error, debug};

use crate::config::PortalConfig;
use crate::protocol::{JsonRpcRequest, JsonRpcResponse, JsonRpcError};
use crate::tools_flat::ToolHost;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,heart_portal=debug".parse().unwrap())
        )
        .init();

    // Load config
    let config_path = std::env::args().nth(1)
        .unwrap_or_else(|| "portal.toml".to_string());
    
    let config = if PathBuf::from(&config_path).exists() {
        PortalConfig::load(&config_path)?
    } else {
        info!("No config file at {}, using defaults", config_path);
        PortalConfig::default()
    };

    if config.auth_token.is_some() {
        info!("Portal '{}' starting on {}:{} (token auth enabled)", config.name, config.bind_host, config.bind_port);
    } else {
        info!("Portal '{}' starting on {}:{} (open access — set token in portal.toml for auth)", config.name, config.bind_host, config.bind_port);
    }

    // Initialize tool host (built-in + custom + extensions)
    let tool_host = ToolHost::new(&config);

    // Load custom tools from workspace/tools/mcp.toml
    match tool_host.load_custom_tools().await {
        Ok(0) => info!("No custom tools loaded"),
        Ok(n) => info!("Loaded {} custom tools", n),
        Err(e) => warn!("Failed to load custom tools: {}", e),
    }

    // Initialize extensions
    match tool_host.initialize_extensions().await {
        Ok(()) => info!("Extensions initialized"),
        Err(e) => warn!("Failed to initialize extensions: {}", e),
    }

    let tool_list = tool_host.list_tools().await;
    info!("Portal tools: {}", tool_list.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", "));

    // Health endpoint (HTTP on port+1)
    let health_port = config.bind_port + 1;
    let health_name = config.name.clone();
    tokio::spawn(async move {
        if let Err(e) = run_health_server(&health_name, health_port).await {
            warn!("Health server failed: {}", e);
        }
    });

    // Track active connections
    let active_connections = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

    // Listen for MCP supervisor connections
    let addr = format!("{}:{}", config.bind_host, config.bind_port);
    let listener = TcpListener::bind(&addr).await?;
    info!("Portal listening on {} (health on :{})", addr, health_port);

    loop {
        let (stream, peer) = listener.accept().await?;
        let conn_count = active_connections.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        info!("MCP client connected from {} (active: {})", peer, conn_count);

        let tool_host = tool_host.clone();
        let portal_name = config.name.clone();
        let auth_token = config.auth_token.clone();
        let active = active_connections.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, &tool_host, &portal_name, auth_token.as_deref()).await {
                warn!("Connection from {} ended: {}", peer, e);
            } else {
                info!("Connection from {} closed cleanly", peer);
            }
            let remaining = active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) - 1;
            info!("Connection closed (active: {})", remaining);
        });
    }
}

/// Handle a single MCP client connection (JSON-RPC over newline-delimited TCP)
async fn handle_connection(
    stream: tokio::net::TcpStream,
    tool_host: &ToolHost,
    portal_name: &str,
    auth_token: Option<&str>,
) -> Result<()> {
    let mut authenticated = auth_token.is_none(); // No token = open access
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = BufWriter::new(write_half);
    let mut line = String::new();

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            debug!("Client disconnected (EOF)");
            return Ok(());
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        debug!("← {}", trimmed);

        let request: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                warn!("Invalid JSON-RPC: {}", e);
                let error_resp = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: None,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32700,
                        message: format!("Parse error: {}", e),
                        data: None,
                    }),
                };
                send_response(&mut writer, &error_resp).await?;
                continue;
            }
        };

        // Notifications (no id) — just ack
        if request.id.is_none() {
            debug!("Notification: {}", request.method);
            continue;
        }

        // Auth gate: before initialize completes, reject non-initialize requests
        if !authenticated && request.method != "initialize" {
            let error_resp = JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32600,
                    message: "Not authenticated. Send initialize with token first.".to_string(),
                    data: None,
                }),
            };
            send_response(&mut writer, &error_resp).await?;
            continue;
        }

        let response = handle_request(&request, tool_host, portal_name, auth_token, &mut authenticated).await;
        send_response(&mut writer, &response).await?;

        // After tool reload, close connection to trigger Hearth reconnect → re-discover tools
        if tool_host.needs_reconnect.load(std::sync::atomic::Ordering::SeqCst) {
            tool_host.needs_reconnect.store(false, std::sync::atomic::Ordering::SeqCst);
            info!("🔄 Closing connection after tools reload — Hearth will auto-reconnect");
            // Small delay to ensure result is flushed
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            return Ok(());
        }
    }
}

/// Route a JSON-RPC request to the appropriate handler
async fn handle_request(
    request: &JsonRpcRequest,
    tool_host: &ToolHost,
    portal_name: &str,
    auth_token: Option<&str>,
    authenticated: &mut bool,
) -> JsonRpcResponse {
    let id = request.id;

    match request.method.as_str() {
        "initialize" => {
            // Token auth check
            if let Some(expected) = auth_token {
                let provided = request.params.get("token")
                    .or_else(|| request.params.get("clientInfo")
                        .and_then(|ci| ci.get("token")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if provided != expected {
                    return JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: None,
                        error: Some(JsonRpcError {
                            code: -32600,
                            message: "Invalid token".to_string(),
                            data: None,
                        }),
                    };
                }
            }
            *authenticated = true;

            JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": { "listChanged": false }
                    },
                    "serverInfo": {
                        "name": format!("heart-portal-{}", portal_name),
                        "version": env!("CARGO_PKG_VERSION")
                    }
                })),
                error: None,
            }
        }

        "tools/list" => {
            let tools: Vec<serde_json::Value> = tool_host.list_tools().await.iter().map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema
                })
            }).collect();

            JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(serde_json::json!({ "tools": tools })),
                error: None,
            }
        }

        "tools/call" => {
            let tool_name = request.params.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let arguments = request.params.get("arguments")
                .cloned()
                .and_then(|v| if v.is_object() { Some(v) } else { None })
                .unwrap_or(serde_json::json!({}));

            let start = std::time::Instant::now();
            info!("⚡ {} called", tool_name);

            let result = tool_host.call(tool_name, arguments).await;
            let elapsed = start.elapsed();

            match result {
                Ok(value) => {
                    let is_error = value.get("isError").and_then(|v| v.as_bool()).unwrap_or(false);
                    if is_error {
                        warn!("⚡ {} → error ({:?})", tool_name, elapsed);
                    } else {
                        info!("⚡ {} → ok ({:?})", tool_name, elapsed);
                    }
                    JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: Some(value),
                        error: None,
                    }
                }
                Err(e) => {
                    warn!("⚡ {} → fail: {} ({:?})", tool_name, e, elapsed);
                    JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: None,
                        error: Some(JsonRpcError {
                            code: -1,
                            message: format!("Tool error: {}", e),
                            data: None,
                        }),
                    }
                }
            }
        }

        "ping" => {
            JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(serde_json::json!({})),
                error: None,
            }
        }

        "extensions/status" => {
            let status = tool_host.get_extension_status().await;
            JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(serde_json::json!({ "extensions": status })),
                error: None,
            }
        }

        "extensions/reload" => {
            match tool_host.reload_extensions().await {
                Ok(changes) => {
                    info!("Extensions reloaded with {} changes", changes.len());
                    JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: Some(serde_json::json!({ 
                            "changes": changes,
                            "reloaded": true 
                        })),
                        error: None,
                    }
                }
                Err(e) => {
                    warn!("Failed to reload extensions: {}", e);
                    JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: None,
                        error: Some(JsonRpcError {
                            code: -1,
                            message: format!("Failed to reload extensions: {}", e),
                            data: None,
                        }),
                    }
                }
            }
        }

        "extensions/start" => {
            let extension_name = request.params.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            
            match tool_host.start_extension(extension_name).await {
                Ok(()) => {
                    info!("Extension '{}' started", extension_name);
                    JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: Some(serde_json::json!({ 
                            "extension": extension_name,
                            "started": true 
                        })),
                        error: None,
                    }
                }
                Err(e) => {
                    warn!("Failed to start extension '{}': {}", extension_name, e);
                    JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: None,
                        error: Some(JsonRpcError {
                            code: -1,
                            message: format!("Failed to start extension '{}': {}", extension_name, e),
                            data: None,
                        }),
                    }
                }
            }
        }

        "extensions/stop" => {
            let extension_name = request.params.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            
            match tool_host.stop_extension(extension_name).await {
                Ok(()) => {
                    info!("Extension '{}' stopped", extension_name);
                    JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: Some(serde_json::json!({ 
                            "extension": extension_name,
                            "stopped": true 
                        })),
                        error: None,
                    }
                }
                Err(e) => {
                    warn!("Failed to stop extension '{}': {}", extension_name, e);
                    JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: None,
                        error: Some(JsonRpcError {
                            code: -1,
                            message: format!("Failed to stop extension '{}': {}", extension_name, e),
                            data: None,
                        }),
                    }
                }
            }
        }

        "extensions/restart" => {
            let extension_name = request.params.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            
            match tool_host.restart_extension(extension_name).await {
                Ok(()) => {
                    info!("Extension '{}' restarted", extension_name);
                    JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: Some(serde_json::json!({ 
                            "extension": extension_name,
                            "restarted": true 
                        })),
                        error: None,
                    }
                }
                Err(e) => {
                    warn!("Failed to restart extension '{}': {}", extension_name, e);
                    JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: None,
                        error: Some(JsonRpcError {
                            code: -1,
                            message: format!("Failed to restart extension '{}': {}", extension_name, e),
                            data: None,
                        }),
                    }
                }
            }
        }

        _ => {
            JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32601,
                    message: format!("Method not found: {}", request.method),
                    data: None,
                }),
            }
        }
    }
}

/// Simple HTTP health endpoint
async fn run_health_server(portal_name: &str, port: u16) -> Result<()> {
    let listener = TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
    info!("Health server on :{}", port);

    let name = portal_name.to_string();
    loop {
        let (mut stream, _) = listener.accept().await?;
        let name = name.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let body = format!("{{\"status\":\"ok\",\"name\":\"{}\",\"version\":\"{}\"}}", name, env!("CARGO_PKG_VERSION"));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(), body
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
    }
}

/// Send a JSON-RPC response (newline-delimited)
async fn send_response(
    writer: &mut BufWriter<tokio::net::tcp::OwnedWriteHalf>,
    response: &JsonRpcResponse,
) -> Result<()> {
    let json = serde_json::to_string(response)?;
    debug!("→ {}", json);
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}
