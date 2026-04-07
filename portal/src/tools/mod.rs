//! Tool host — manages built-in + custom (being-defined) tools.
//! Built-in: exec, file, web. Custom: loaded from workspace/tools/mcp.toml.

mod exec;
mod file;
mod web;
mod search;
pub mod custom;

use crate::config::PortalConfig;
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
    /// Set to true after reload — signals connection handler to close TCP
    pub needs_reconnect: Arc<AtomicBool>,
}

impl ToolHost {
    pub fn new(config: &PortalConfig) -> Self {
        Self {
            config: config.clone(),
            custom: CustomToolHost::new(),
            needs_reconnect: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Load custom tools from workspace/tools/mcp.toml
    pub async fn load_custom_tools(&self) -> Result<usize> {
        self.custom.load(&self.config.security.workspace_root).await
    }

    /// Reload custom tools and signal for reconnection
    pub async fn reload_custom_tools(&self) -> Result<(usize, Vec<String>)> {
        // Shutdown existing custom MCP servers
        self.custom.shutdown().await;
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
                            "description": "Timeout in seconds (default: 30)"
                        }
                    },
                    "required": ["command"]
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
                description: "Search the web using Google Custom Search API".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query"
                        },
                        "count": {
                            "type": "integer",
                            "description": "Number of results to return (default: 5, max: 10)"
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
            "portal_exec" => exec::execute(&self.config, arguments).await,
            "portal_file_read" => file::read(&self.config, arguments).await,
            "portal_file_write" => file::write(&self.config, arguments).await,
            "portal_file_list" => file::list(&self.config, arguments).await,
            "portal_web_fetch" => web::fetch(arguments).await,
            "portal_web_search" => search::search(arguments).await,
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
