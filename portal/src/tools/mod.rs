//! Tool host — manages built-in + custom (being-defined) tools.
//! Built-in: exec, file, web. Custom: loaded from workspace/tools/mcp.toml.

mod exec;
mod file;
mod process;
mod search;
mod web;
mod web_search;
pub mod custom;

use crate::config::PortalConfig;
use crate::process_manager::ProcessManager;
use custom::CustomToolHost;
use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::info;

/// Tool metadata for tools/list response
#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Hosts all available tools (built-in + custom), dispatches calls
#[derive(Clone)]
pub struct ToolHost {
    config: PortalConfig,
    custom: CustomToolHost,
    process_manager: Arc<ProcessManager>,
    /// Set to true after reload — signals connection handler to close TCP
    pub needs_reconnect: Arc<AtomicBool>,
}

impl ToolHost {
    pub fn new(config: &PortalConfig) -> Self {
        Self {
            config: config.clone(),
            custom: CustomToolHost::new(),
            process_manager: Arc::new(ProcessManager::new()),
            needs_reconnect: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn kill_all_managed_processes(&self) {
        self.process_manager.kill_all().await;
    }

    pub async fn cleanup_background_sessions(&self) {
        self.process_manager.cleanup().await;
    }

    /// Load custom tools from workspace/tools/mcp.toml
    pub async fn load_custom_tools(&self) -> Result<usize> {
        if !self.config.tools.custom_tools_enabled {
            return Ok(0);
        }
        self.custom.load(&self.config.security.workspace_root).await
    }

    /// Reload custom tools and signal for reconnection
    pub async fn reload_custom_tools(&self) -> Result<(usize, Vec<String>)> {
        // Shutdown existing custom MCP servers
        self.custom.shutdown().await;
        if !self.config.tools.custom_tools_enabled {
            return Ok((0, vec![]));
        }
        // Reload from config
        let count = self.custom.load(&self.config.security.workspace_root).await?;
        let names: Vec<String> = self.custom.list_tools().await
            .iter().map(|t| t.name.clone()).collect();
        // Signal reconnection needed
        self.needs_reconnect.store(true, Ordering::SeqCst);
        info!("Tools reloaded: {} custom tools. Reconnect signaled.", count);
        Ok((count, names))
    }

    /// List all available tools (built-in + custom)
    pub async fn list_tools(&self) -> Vec<ToolInfo> {
        let mut tools = self.list_builtin_tools();
        let custom = self.custom.list_tools().await;
        tools.extend(custom);
        tools
    }

    /// List built-in tools only
    fn list_builtin_tools(&self) -> Vec<ToolInfo> {
        let mut tools = Vec::new();

        if self.config.tools.exec {
            tools.push(ToolInfo {
                name: "portal_exec".to_string(),
                description: "Execute a shell command".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "Shell command to execute"
                        },
                        "workdir": {
                            "type": "string",
                            "description": "Working directory (optional)"
                        },
                        "timeout_secs": {
                            "type": "integer",
                            "description": "Timeout in seconds (default: 30, sync mode only)"
                        },
                        "background": {
                            "type": "boolean",
                            "description": "If true, spawn in background and return session_id + pid (default: false)"
                        }
                    },
                    "required": ["command"]
                }),
            });

            tools.push(ToolInfo {
                name: "portal_process".to_string(),
                description: "Manage background shell sessions: list, poll output, log, write stdin, kill. Responses include idle_s (seconds since last stdout/stderr) and total_output_bytes so you can tell silence from steady output.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "description": "list | poll | log | write | kill",
                            "enum": ["list", "poll", "log", "write", "kill"]
                        },
                        "session_id": { "type": "string", "description": "Session id (required for poll, log, write, kill)" },
                        "timeout_ms": { "type": "integer", "description": "poll: wait up to this many ms for new output (default 5000, max 300000)" },
                        "offset": { "type": "integer", "description": "Byte offset into captured output (poll/log)" },
                        "limit": { "type": "integer", "description": "Max bytes for log" },
                        "data": { "type": "string", "description": "Data to write to stdin (write action, max 256KiB)" }
                    },
                    "required": ["action"]
                }),
            });
        }

        if self.config.tools.web_fetch {
            tools.push(ToolInfo {
                name: "portal_web_fetch".to_string(),
                description: "Fetch content from a URL".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "URL to fetch"
                        },
                        "max_chars": {
                            "type": "integer",
                            "description": "Maximum characters to return (default: 50000)"
                        }
                    },
                    "required": ["url"]
                }),
            });

            tools.push(ToolInfo {
                name: "portal_web_search".to_string(),
                description: "Search the web. Returns titles, URLs, and snippets.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query"
                        },
                        "count": {
                            "type": "integer",
                            "description": "Number of results (default 5, max 10)"
                        }
                    },
                    "required": ["query"]
                }),
            });
        }

        if self.config.tools.file {
            tools.push(ToolInfo {
                name: "portal_file_read".to_string(),
                description: "Read a file's contents".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path (relative to workspace root)"
                        }
                    },
                    "required": ["path"]
                }),
            });

            tools.push(ToolInfo {
                name: "portal_file_write".to_string(),
                description: "Write content to a file".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path (relative to workspace root)"
                        },
                        "content": {
                            "type": "string",
                            "description": "Content to write"
                        }
                    },
                    "required": ["path", "content"]
                }),
            });

            tools.push(ToolInfo {
                name: "portal_file_list".to_string(),
                description: "List files in a directory".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory path (relative to workspace root)"
                        }
                    },
                    "required": ["path"]
                }),
            });
        }

        if self.config.tools.search {
            tools.push(ToolInfo {
                name: "portal_search".to_string(),
                description: "Recursively search text files under the workspace for a regex pattern (ripgrep-style Rust regex).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Rust regex pattern to match against each line"
                        },
                        "path": {
                            "type": "string",
                            "description": "Subdirectory or file to search under workspace root (default: entire workspace)"
                        },
                        "max_matches": {
                            "type": "integer",
                            "description": "Maximum matches to return (default 200, max 2000)"
                        }
                    },
                    "required": ["pattern"]
                }),
            });
        }

        // Always include tools_reload
        tools.push(ToolInfo {
            name: "portal_tools_reload".to_string(),
            description: "Reload custom tools from workspace/tools/mcp.toml. Call after adding or modifying custom tool scripts.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        });

        tools
    }

    /// Execute a tool call (built-in or custom)
    pub async fn call(&self, tool_name: &str, arguments: Value) -> Result<Value> {
        // Check custom tools first
        if self.custom.has_tool(tool_name).await {
            return self.custom.call(tool_name, arguments).await;
        }

        // Built-in tools
        match tool_name {
            "portal_exec" => {
                if !self.config.tools.exec {
                    anyhow::bail!("portal_exec is disabled in configuration");
                }
                exec::execute(&self.config, &self.process_manager, arguments).await
            }
            "portal_process" => {
                if !self.config.tools.exec {
                    anyhow::bail!("portal_process is disabled in configuration");
                }
                process::handle(&self.process_manager, arguments).await
            }
            "portal_file_read" => file::read(&self.config, arguments).await,
            "portal_file_write" => file::write(&self.config, arguments).await,
            "portal_file_list" => file::list(&self.config, arguments).await,
            "portal_search" => search::search(&self.config, arguments).await,
            "portal_web_fetch" => web::fetch(arguments).await,
            "portal_web_search" => web_search::search(arguments).await,
            "portal_tools_reload" => self.handle_tools_reload().await,
            _ => anyhow::bail!("Unknown tool: {}", tool_name),
        }
    }

    /// Reload custom tools and return summary
    async fn handle_tools_reload(&self) -> Result<Value> {
        match self.reload_custom_tools().await {
            Ok((count, names)) => {
                let msg = if count == 0 {
                    "Reloaded. No custom tools found in workspace/tools/mcp.toml.".to_string()
                } else {
                    format!("Reloaded {} custom tools: {}. Connection will reset to apply changes.",
                        count, names.join(", "))
                };
                Ok(serde_json::json!({
                    "content": [{"type": "text", "text": msg}]
                }))
            }
            Err(e) => {
                Ok(serde_json::json!({
                    "content": [{"type": "text", "text": format!("Reload failed: {}", e)}],
                    "isError": true
                }))
            }
        }
    }
}
