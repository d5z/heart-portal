//! Heart Portal — Being's gateway to the world.
//! 
//! A lightweight MCP server with built-in tools (exec, file, web).
//! Heart's MCP supervisor connects to Portal via TCP.
//! Portal can run on Town Home, a human's laptop, or anywhere.

mod config;
mod exec_policy;
mod process_manager;
mod tools;
mod protocol;
mod cowork;
mod relay_client;

use std::path::PathBuf;
use std::time::Duration;
use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpListener;
use tracing::{info, warn, error, debug};

use crate::config::PortalConfig;
use crate::protocol::{JsonRpcRequest, JsonRpcResponse, JsonRpcError};
use crate::tools::ToolHost;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,heart_portal=debug".parse().unwrap_or_else(|e| {
                    eprintln!("Failed to parse default log filter: {}", e);
                    tracing_subscriber::EnvFilter::new("info")
                }))
        )
        .init();

    // CLI: --connect <loom_url> for reverse relay; --config / -c / positional for portal.toml
    let (connect_link, config_path) = parse_cli().context("Invalid command-line arguments")?;
    
    let mut config = if PathBuf::from(&config_path).exists() {
        PortalConfig::load(&config_path)?
    } else {
        info!("No config file at {}, using defaults", config_path);
        PortalConfig::default()
    };

    if let Ok(t) = std::env::var("PORTAL_MCP_TOKEN") {
        if !t.is_empty() {
            config.portal_mcp_token = Some(t);
        }
    }

    if config.portal_mcp_token.is_none() {
        warn!("PORTAL_MCP_TOKEN is not set — MCP TCP connections are unauthenticated (set token for public deployments)");
    }

    if connect_link.is_some() {
        info!(
            "Portal '{}' connect mode (Cowork HTTP on :{}) — MCP via Hearth relay :4000",
            config.name, config.cowork.http_port
        );
    } else {
        info!("Portal '{}' starting on {}:{}", config.name, config.bind_host, config.bind_port);
    }

    // Initialize tool host (built-in + custom)
    let tool_host = ToolHost::new(&config);

    let cleanup_host = tool_host.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            cleanup_host.cleanup_background_sessions().await;
        }
    });

    // Load custom tools from workspace/tools/mcp.toml
    match tool_host.load_custom_tools().await {
        Ok(0) => info!("No custom tools loaded"),
        Ok(n) => info!("Loaded {} custom tools", n),
        Err(e) => warn!("Failed to load custom tools: {}", e),
    }

    let tool_list = tool_host.list_tools().await;
    info!("Portal tools: {}", tool_list.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", "));

    // Cowork HTTP server (replaces old health endpoint)
    if config.cowork.enabled {
        if cowork::cowork_token().is_none() {
            warn!("══════════════════════════════════════════════════════════════════════");
            warn!("Cowork token not set (PORTAL_TOKEN / LOOM_TOKEN): HTTP API and WebSocket are");
            warn!("unauthenticated. Set a token for any network-exposed deployment.");
            warn!("══════════════════════════════════════════════════════════════════════");
        }
        let cowork_config = config.clone();
        let workspace = config.security.workspace_root.clone();
        let (file_tx, _) = tokio::sync::broadcast::channel::<cowork::FileEvent>(256);
        
        // Start file watcher
        cowork::start_file_watcher(workspace.clone(), file_tx.clone());
        
        let state = cowork::CoworkState {
            config: cowork_config.clone(),
            workspace,
            file_events: file_tx,
        };
        let router = cowork::cowork_router(state);
        let http_port = cowork_config.cowork.http_port;
        let http_addr = format!("{}:{}", cowork_config.bind_host, http_port);
        info!("Cowork HTTP server starting on {}", http_addr);
        
        tokio::spawn(async move {
            match tokio::net::TcpListener::bind(&http_addr).await {
                Ok(listener) => {
                    if let Err(e) = axum::serve(listener, router).await {
                        error!("Cowork server failed: {}", e);
                    }
                }
                Err(e) => {
                    error!("Failed to bind cowork HTTP server to {}: {}", http_addr, e);
                }
            }
        });
    } else {
        // Fallback: simple health endpoint
        let health_port = config.bind_port + 1;
        let health_name = config.name.clone();
        tokio::spawn(async move {
            if let Err(e) = run_health_server(&health_name, health_port).await {
                warn!("Health server failed: {}", e);
            }
        });
    }

    if let Some(ref loom) = connect_link {
        let tool_shutdown = tool_host.clone();
        tokio::select! {
            _ = async {
                let _ = tokio::signal::ctrl_c().await;
            } => {
                info!("Portal shutting down (Ctrl+C)");
                tool_shutdown.kill_all_managed_processes().await;
            }
            _ = relay_client::connect_and_serve(loom, &tool_host, &config.name) => {}
        }
        return Ok(());
    }

    // Track active connections
    let active_connections = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

    // Listen for MCP supervisor connections
    let addr = format!("{}:{}", config.bind_host, config.bind_port);
    let listener = TcpListener::bind(&addr).await?;
    let http_port = config.cowork.http_port;
    info!("Portal listening on {} (HTTP on :{})", addr, http_port);

    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
    let mut shutdown_rx = shutdown_tx.subscribe();
    let shutdown_cleanup = tool_host.clone();
    tokio::spawn({
        let shutdown_tx = shutdown_tx.clone();
        async move {
            tokio::select! {
                r = tokio::signal::ctrl_c() => {
                    let _ = r;
                }
                _ = wait_sigterm() => {}
            }
            info!("Portal shutting down");
            let _ = shutdown_tx.send(());
        }
    });
    drop(shutdown_tx);

    loop {
        tokio::select! {
            biased;
            res = shutdown_rx.recv() => {
                match res {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
                shutdown_cleanup.kill_all_managed_processes().await;
                break;
            }
            accept = listener.accept() => {
                let (stream, peer) = accept.context("MCP listener accept failed")?;
                let conn_count = active_connections.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                info!("MCP client connected from {} (active: {})", peer, conn_count);

                let tool_host = tool_host.clone();
                let portal_name = config.name.clone();
                let mcp_token = config.portal_mcp_token.clone();
                let active = active_connections.clone();

                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, &tool_host, &portal_name, mcp_token.as_deref()).await {
                        warn!("Connection from {} ended: {}", peer, e);
                    } else {
                        info!("Connection from {} closed cleanly", peer);
                    }
                    let remaining = active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) - 1;
                    info!("Connection closed (active: {})", remaining);
                });
            }
        }
    }

    drop(listener);
    Ok(())
}

fn parse_cli() -> Result<(Option<String>, String)> {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    let mut cfg: Option<String> = None;
    let mut connect: Option<String> = None;
    while i < args.len() {
        let a = &args[i];
        if a == "--connect" {
            let url = args
                .get(i + 1)
                .ok_or_else(|| anyhow::anyhow!("--connect requires a Loom URL"))?;
            connect = Some(url.clone());
            i += 2;
        } else if let Some(url) = a.strip_prefix("--connect=") {
            if url.is_empty() {
                anyhow::bail!("--connect= requires a non-empty URL");
            }
            connect = Some(url.to_string());
            i += 1;
        } else if a == "--config" || a == "-c" {
            let path = args
                .get(i + 1)
                .ok_or_else(|| anyhow::anyhow!("--config requires a path"))?;
            cfg = Some(path.clone());
            i += 2;
        } else if let Some(p) = a.strip_prefix("--config=") {
            if p.is_empty() {
                anyhow::bail!("--config= requires a non-empty path");
            }
            cfg = Some(p.to_string());
            i += 1;
        } else if a.starts_with('-') {
            anyhow::bail!("Unknown argument: {}", a);
        } else if cfg.is_none() {
            cfg = Some(a.clone());
            i += 1;
        } else {
            anyhow::bail!("Unexpected positional argument: {}", a);
        }
    }
    Ok((connect, cfg.unwrap_or_else(|| "portal.toml".to_string())))
}

#[cfg(unix)]
async fn wait_sigterm() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(_) => std::future::pending::<()>().await,
    }
}

#[cfg(not(unix))]
async fn wait_sigterm() {
    std::future::pending::<()>().await
}

/// Handle a single MCP client connection (JSON-RPC over newline-delimited TCP)
pub(crate) async fn handle_connection<S>(
    stream: S,
    tool_host: &ToolHost,
    portal_name: &str,
    expected_token: Option<&str>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut writer = BufWriter::new(write_half);
    let mut line = String::new();

    if let Some(expected) = expected_token.filter(|t| !t.is_empty()) {
        loop {
            line.clear();
            let bytes_read = reader.read_line(&mut line).await?;
            if bytes_read == 0 {
                debug!("Client disconnected before auth (EOF)");
                return Ok(());
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            debug!("← auth line: {}", trimmed);

            let value: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
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
                    anyhow::bail!("MCP auth: invalid JSON");
                }
            };

            let method = value.get("method").and_then(|v| v.as_str()).unwrap_or_else(|| {
                debug!("Missing or invalid 'method' field in JSON-RPC request");
                ""
            });
            let id = value.get("id").cloned();
            if method != "auth" {
                let error_resp = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: id.as_ref().and_then(|v| v.as_u64()),
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32001,
                        message: "Unauthorized: first message must be {\"method\":\"auth\",\"params\":{\"token\":\"...\"}}"
                            .to_string(),
                        data: None,
                    }),
                };
                send_response(&mut writer, &error_resp).await?;
                anyhow::bail!("MCP auth: expected auth as first message");
            }

            let token = value
                .get("params")
                .and_then(|p| p.get("token"))
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| {
                    debug!("Missing or invalid token in auth params");
                    ""
                });
            if token != expected {
                let error_resp = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: id.as_ref().and_then(|v| v.as_u64()),
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32002,
                        message: "Unauthorized: invalid token".to_string(),
                        data: None,
                    }),
                };
                send_response(&mut writer, &error_resp).await?;
                anyhow::bail!("MCP auth: invalid token");
            }

            let ok = JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: id.as_ref().and_then(|v| v.as_u64()),
                result: Some(serde_json::json!({ "authenticated": true })),
                error: None,
            };
            send_response(&mut writer, &ok).await?;
            break;
        }
    }

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

        let response = handle_request(&request, tool_host, portal_name).await;
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
) -> JsonRpcResponse {
    let id = request.id;

    match request.method.as_str() {
        "initialize" => {
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
                .unwrap_or_else(|| {
                    debug!("Missing or invalid tool name in tools/call request");
                    ""
                });
            let arguments = request.params.get("arguments")
                .cloned()
                .and_then(|v| if v.is_object() { Some(v) } else { None })
                .unwrap_or_else(|| {
                    debug!("Missing or invalid arguments in tools/call request, using empty object");
                    serde_json::json!({})
                });

            let start = std::time::Instant::now();
            info!("⚡ {} called", tool_name);

            let result = tool_host.call(tool_name, arguments).await;
            let elapsed = start.elapsed();

            match result {
                Ok(value) => {
                    let is_error = value.get("isError").and_then(|v| v.as_bool()).unwrap_or_else(|| {
                        debug!("Missing or invalid isError field in tool result, assuming success");
                        false
                    });
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
async fn send_response<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut BufWriter<W>,
    response: &JsonRpcResponse,
) -> Result<()> {
    let json = serde_json::to_string(response)?;
    debug!("→ {}", json);
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}
