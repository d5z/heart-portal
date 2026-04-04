//! Heart MCP Supervisor — independent process managing tool servers.
//!
//! Reads mcp-servers.toml, spawns tool server child processes,
//! exposes a Unix socket for clients (Cortex, Core, Sensorium) to call tools.

use heart_mcp::protocol;
#[allow(unused_imports)]
use heart_mcp::client;

use heart_mcp::ipc::IpcConnection;
use heart_mcp::mcp_ipc::{McpRequest, McpResponse};
use heart_shared::ToolSpec;
use client::{McpClient, load_server_configs};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Mutex, broadcast};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use anyhow::{Context, Result};
use tracing::{debug, info, warn, error};

/// Convert MCP tool info to shared ToolSpec format.
fn tool_info_to_spec(_server_name: &str, info: &protocol::McpToolInfo) -> ToolSpec {
    ToolSpec {
        name: info.name.clone(),
        description: info.description.clone(),
        parameters: info.input_schema.clone(),
    }
}

struct SupervisorState {
    mcp_client: Mutex<McpClient>,
    /// server_name → tool_name mapping for routing
    tool_routing: Mutex<HashMap<String, String>>,
    /// Current tool specs (for pushing to new clients)
    tool_specs: Mutex<Vec<ToolSpec>>,
    /// Broadcast channel for ToolList updates (hot reload)
    tool_update_tx: broadcast::Sender<Vec<ToolSpec>>,
}

impl SupervisorState {
    async fn refresh_tools(&self) -> Vec<ToolSpec> {
        let client = self.mcp_client.lock().await;
        let discovered = client.discover_tools().await;
        
        let mut routing = self.tool_routing.lock().await;
        routing.clear();
        
        let specs: Vec<ToolSpec> = discovered.iter()
            .map(|(server_name, info)| {
                routing.insert(info.name.clone(), server_name.clone());
                tool_info_to_spec(server_name, info)
            })
            .collect();
        
        let mut tool_specs = self.tool_specs.lock().await;
        *tool_specs = specs.clone();
        
        // Broadcast to all connected clients
        let _ = self.tool_update_tx.send(specs.clone());
        
        info!("Tool list refreshed: {} tools from {} servers", 
              specs.len(), client.connection_count());
        specs
    }
}

async fn handle_client(
    stream: UnixStream,
    state: Arc<SupervisorState>,
    client_id: u64,
) {
    let mut conn = IpcConnection::new(stream);
    
    // Push current tool list immediately
    {
        let specs = state.tool_specs.lock().await;
        if let Err(e) = conn.send(&McpResponse::ToolList { tools: specs.clone() }).await {
            warn!("Client {} — failed to push initial ToolList: {}", client_id, e);
            return;
        }
        info!("Client {} connected, pushed {} tools", client_id, specs.len());
    }
    
    // Subscribe to tool updates for push notifications
    let mut update_rx = state.tool_update_tx.subscribe();
    
    loop {
        tokio::select! {
            // Client request
            msg = conn.recv::<McpRequest>() => {
                match msg {
                    Ok(Some(McpRequest::ListTools)) => {
                        let specs = state.tool_specs.lock().await;
                        if let Err(e) = conn.send(&McpResponse::ToolList { tools: specs.clone() }).await {
                            warn!("Client {} — send error: {}", client_id, e);
                            break;
                        }
                    }
                    Ok(Some(McpRequest::CallTool { call_id, server_name, tool_name, arguments })) => {
                        // Route to the correct MCP server
                        let actual_server = if server_name.is_empty() {
                            // Look up server from routing table
                            let routing = state.tool_routing.lock().await;
                            routing.get(&tool_name).cloned()
                        } else {
                            Some(server_name)
                        };
                        
                        let response = match actual_server {
                            Some(server) => {
                                let client = state.mcp_client.lock().await;
                                match client.call_tool(&server, &tool_name, arguments).await {
                                    Ok(result) => McpResponse::ToolResult {
                                        call_id,
                                        result,
                                        is_error: false,
                                    },
                                    Err(e) => McpResponse::ToolResult {
                                        call_id,
                                        result: serde_json::json!({ "error": e.to_string() }),
                                        is_error: true,
                                    },
                                }
                            }
                            None => McpResponse::ToolResult {
                                call_id,
                                result: serde_json::json!({ "error": format!("No server found for tool '{}'", tool_name) }),
                                is_error: true,
                            },
                        };
                        
                        if let Err(e) = conn.send(&response).await {
                            warn!("Client {} — send error: {}", client_id, e);
                            break;
                        }
                    }
                    Ok(Some(McpRequest::Ping)) => {
                        let client = state.mcp_client.lock().await;
                        let specs = state.tool_specs.lock().await;
                        if let Err(e) = conn.send(&McpResponse::Pong {
                            server_count: client.connection_count(),
                            tool_count: specs.len(),
                        }).await {
                            warn!("Client {} — send error: {}", client_id, e);
                            break;
                        }
                    }
                    Ok(None) => {
                        info!("Client {} disconnected", client_id);
                        break;
                    }
                    Err(e) => {
                        warn!("Client {} — recv error: {}", client_id, e);
                        break;
                    }
                }
            }
            // Tool list update (hot reload broadcast)
            Ok(new_tools) = update_rx.recv() => {
                if let Err(e) = conn.send(&McpResponse::ToolList { tools: new_tools }).await {
                    warn!("Client {} — failed to push updated ToolList: {}", client_id, e);
                    break;
                }
                debug!("Client {} — pushed updated ToolList", client_id);
            }
        }
    }
}

async fn watch_config(config_path: PathBuf, state: Arc<SupervisorState>) {
    use notify::{Watcher, RecursiveMode, Event, EventKind};
    
    let (tx, mut rx) = mpsc::channel::<()>(1);
    
    let mut watcher = match notify::recommended_watcher(move |res: std::result::Result<Event, notify::Error>| {
        if let Ok(event) = res {
            if matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                let _ = tx.blocking_send(());
            }
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            warn!("Failed to create file watcher: {}", e);
            return;
        }
    };
    
    if let Err(e) = watcher.watch(&config_path, RecursiveMode::NonRecursive) {
        warn!("Failed to watch {}: {}", config_path.display(), e);
        return;
    }
    
    info!("Watching {} for changes", config_path.display());
    
    while let Some(()) = rx.recv().await {
        // Debounce: wait a bit for writes to settle
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        // Drain any additional notifications
        while rx.try_recv().is_ok() {}
        
        info!("Config file changed, reloading MCP servers...");
        
        match load_server_configs(&config_path).await {
            Ok(configs) => {
                // Shutdown old, start new
                let mut client = state.mcp_client.lock().await;
                if let Err(e) = client.shutdown_all().await {
                    warn!("Error shutting down old MCP servers: {}", e);
                }
                
                match McpClient::connect_all(configs).await {
                    Ok(new_client) => {
                        *client = new_client;
                        drop(client); // release lock before refresh_tools
                        state.refresh_tools().await;
                        info!("MCP servers reloaded successfully");
                    }
                    Err(e) => {
                        error!("Failed to reload MCP servers: {}", e);
                    }
                }
            }
            Err(e) => {
                error!("Failed to parse config: {}", e);
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    
    let config_path = PathBuf::from(
        std::env::var("HEART_MCP_CONFIG")
            .unwrap_or_else(|_| "mcp-servers.toml".to_string())
    );
    let socket_path = std::env::var("HEART_MCP_SOCKET")
        .unwrap_or_else(|_| "/tmp/heart-mcp.sock".to_string());
    
    info!("Heart MCP Supervisor starting");
    info!("Config: {}", config_path.display());
    info!("Socket: {}", socket_path);
    
    // Load and connect to MCP servers
    let configs = load_server_configs(&config_path).await
        .context("Failed to load MCP server configs")?;
    
    let mcp_client = McpClient::connect_all(configs).await
        .context("Failed to initialize MCP client")?;
    
    let (tool_update_tx, _) = broadcast::channel(16);
    
    let state = Arc::new(SupervisorState {
        mcp_client: Mutex::new(mcp_client),
        tool_routing: Mutex::new(HashMap::new()),
        tool_specs: Mutex::new(Vec::new()),
        tool_update_tx,
    });
    
    // Discover initial tools
    state.refresh_tools().await;
    
    // Clean up stale socket
    let _ = tokio::fs::remove_file(&socket_path).await;
    
    // Start Unix socket server
    let listener = UnixListener::bind(&socket_path)
        .context(format!("Failed to bind Unix socket at {}", socket_path))?;
    info!("Listening on {}", socket_path);
    
    // Watch config for hot reload
    let config_watcher_state = state.clone();
    tokio::spawn(async move {
        watch_config(config_path, config_watcher_state).await;
    });
    
    // Handle SIGTERM for graceful shutdown
    let state_for_shutdown = state.clone();
    let socket_path_for_cleanup = socket_path.clone();
    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                info!("Received shutdown signal");
                let mut client = state_for_shutdown.mcp_client.lock().await;
                if let Err(e) = client.shutdown_all().await {
                    warn!("Error during shutdown: {}", e);
                }
                let _ = tokio::fs::remove_file(&socket_path_for_cleanup).await;
                info!("MCP Supervisor shutdown complete");
                std::process::exit(0);
            }
            Err(e) => {
                error!("Failed to listen for shutdown signal: {}", e);
            }
        }
    });
    
    // Accept client connections
    let mut client_counter: u64 = 0;
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                client_counter += 1;
                let client_id = client_counter;
                let client_state = state.clone();
                tokio::spawn(async move {
                    handle_client(stream, client_state, client_id).await;
                });
            }
            Err(e) => {
                warn!("Failed to accept connection: {}", e);
            }
        }
    }
}
