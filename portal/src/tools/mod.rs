//! Tool host — manages built-in, custom (being-defined), and kit tools.
//! Built-in: exec, file, web. Custom: loaded from workspace/tools/mcp.toml.

mod exec;
mod file;
mod process;
mod screenshot;
mod search;
mod web;
mod web_search;
pub mod custom;

use crate::config::PortalConfig;
use crate::kits::{loader, manager::KitManager};
use crate::process_manager::ProcessManager;
use custom::CustomToolHost;
use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

/// Tool metadata for tools/list response
#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Hosts all available tools (built-in + custom + kit), dispatches calls
#[derive(Clone)]
pub struct ToolHost {
    config: PortalConfig,
    custom: CustomToolHost,
    kits: KitManager,
    pub process_manager: Arc<ProcessManager>,
    /// Set to true after reload — signals connection handler to close TCP
    pub needs_reconnect: Arc<AtomicBool>,
}

impl ToolHost {
    pub fn new(config: &PortalConfig) -> Self {
        let loaded_kits = match loader::load_kits(config) {
            Ok(kits) => {
                if !kits.is_empty() {
                    info!("Loaded {} kit manifest(s)", kits.len());
                }
                kits
            }
            Err(err) => {
                warn!("Failed to load kits: {}", err);
                Vec::new()
            }
        };

        Self {
            config: config.clone(),
            custom: CustomToolHost::new(),
            kits: KitManager::new(loaded_kits),
            process_manager: Arc::new(ProcessManager::new()),
            needs_reconnect: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn kill_all_managed_processes(&self) {
        self.process_manager.kill_all().await;
        self.kits.shutdown().await;
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
        let kit_tools = self.kits.list_healthy_tools().await;
        tools.extend(kit_tools);
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
                description: "Read file contents. Returns text for text files, base64 for images. For large files, use offset (0-based line number) and limit to read specific sections instead of portal_exec head/tail.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path (relative to workspace root)"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Start reading from this line number (0-based, default: 0). Use with limit for large files."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of lines to return. Omit to read all remaining lines from offset."
                        }
                    },
                    "required": ["path"]
                }),
            });

            tools.push(ToolInfo {
                name: "portal_file_write".to_string(),
                description: "Write content to a file (creates parent dirs automatically). THE preferred way to write files — no shell escaping issues.\n\nModes:\n- Default: overwrite file with content\n- append=true: add to end of existing file\n- encoding=\"base64\": decode base64 content before writing (for binary files)\n- raw=true: skip escape sequence processing (write \\n as literal backslash-n)".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path (relative to workspace root)"
                        },
                        "content": {
                            "type": "string",
                            "description": "File content. Use real newlines — they pass through correctly. No need for \\n or base64 workarounds for text files."
                        },
                        "append": {
                            "type": "boolean",
                            "description": "If true, append to file instead of overwriting (default: false)"
                        },
                        "encoding": {
                            "type": "string",
                            "description": "Content encoding: utf8 (default) or base64 (decode before writing, for binary files)"
                        },
                        "raw": {
                            "type": "boolean",
                            "description": "If true, write content exactly as-is without processing escape sequences (default: false)"
                        }
                    },
                    "required": ["path", "content"]
                }),
            });

            tools.push(ToolInfo {
                name: "portal_file_edit".to_string(),
                description: "Find and replace exact text in a file. Safer than sed — no shell escaping issues. Shows line-numbered context after replacement.\n\nBehavior:\n- Single match: replaces it, shows surrounding context\n- Multiple matches with count=1 (default): errors asking for more specific text\n- count=-1: replace ALL occurrences".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path (relative to workspace root)"
                        },
                        "old_text": {
                            "type": "string",
                            "description": "Exact text to find (multi-line supported with real newlines)"
                        },
                        "new_text": {
                            "type": "string",
                            "description": "Replacement text"
                        },
                        "count": {
                            "type": "integer",
                            "description": "How many occurrences to replace (default: 1). Use -1 for all. If 1 and multiple matches found, will error asking for more context."
                        }
                    },
                    "required": ["path", "old_text", "new_text"]
                }),
            });

            tools.push(ToolInfo {
                name: "portal_file_list".to_string(),
                description: "List files and directories. Returns JSON array with name, size, is_dir, modified (unix timestamp) for each entry.".to_string(),
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

        if self.config.tools.screenshot {
            tools.push(ToolInfo {
                name: "portal_screenshot".to_string(),
                description: "Capture a screenshot of the screen or a specific window/region".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Output file path (relative to workspace). Defaults to '.screenshots/capture-<timestamp>.png'"
                        },
                        "region": {
                            "type": "string",
                            "description": "Capture region: 'full' (entire screen), 'window' (frontmost window), or 'x,y,w,h' (rectangle). Default: 'full'"
                        },
                        "display": {
                            "type": "integer",
                            "description": "Display number for multi-monitor (0-indexed). Default: main display"
                        }
                    }
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

        if let Some((kit_name, real_tool_name)) = self.kits.resolve_tool(tool_name).await {
            return self
                .kits
                .call_tool(&kit_name, &real_tool_name, arguments)
                .await;
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
            "portal_file_edit" => file::edit(&self.config, arguments).await,
            "portal_screenshot" => {
                if !self.config.tools.screenshot {
                    anyhow::bail!("portal_screenshot is disabled in configuration");
                }
                screenshot::capture(&self.config, arguments).await
            }
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
