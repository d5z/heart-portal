//! Tool host — manages built-in, custom (being-defined), and kit tools.
//! Built-in: exec, file, web. Custom: loaded from workspace/tools/mcp.toml.

mod exec;
mod file;
mod oauth;
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
    /// A controlled restart requested through the built-in portal_restart tool.
    restart_requested: Arc<AtomicBool>,
    restart_notify: Arc<tokio::sync::Notify>,
    restart_supported: bool,
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
            restart_requested: Arc::new(AtomicBool::new(false)),
            restart_notify: Arc::new(tokio::sync::Notify::new()),
            restart_supported: std::env::var("HEART_PORTAL_SUPERVISED")
                .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
        }
    }

    /// Wait until a tool caller has requested a controlled Portal restart.
    pub async fn wait_for_restart(&self) {
        self.restart_notify.notified().await;
    }

    pub async fn kill_all_managed_processes(&self) {
        let cleanup = async {
            tokio::join!(
                self.process_manager.kill_all(),
                self.kits.shutdown(),
                self.custom.shutdown(),
            );
        };
        if tokio::time::timeout(std::time::Duration::from_secs(10), cleanup)
            .await
            .is_err()
        {
            warn!("Portal shutdown cleanup timed out; exiting so the supervisor can restart it");
        }
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

    /// Pre-spawn eager kits (manifest.eager == true) so the first call has
    /// no cold-start latency. Failures are logged, not fatal.
    pub async fn warmup_kits(&self) {
        self.kits.warmup().await
    }

    /// Periodically re-scan the kits directory and refresh manifests in place.
    pub fn start_kit_refresh_task(&self) -> tokio::task::JoinHandle<()> {
        let kits = self.kits.clone();
        let kits_dir = loader::kits_dir(&self.config);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                match loader::load_kits_from_dir(&kits_dir) {
                    Ok(fresh) => kits.refresh_kits(fresh).await,
                    Err(err) => warn!("Failed to scan kits directory for refresh: {}", err),
                }
            }
        })
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
                description: "Execute a shell command. With background=true it returns a session_id immediately and, when the task finishes, Portal notifies you automatically — you will be woken with the exit code and output, so you can let go of it instead of polling. Prefer background=true for anything slow (builds, tests, long downloads).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "Shell command to execute"
                        },
                        "shell": {
                            "type": "string",
                            "enum": ["default", "powershell"],
                            "description": "default: cmd.exe on Windows, sh elsewhere. powershell: Windows PowerShell with UTF-8 output and text file defaults; pass PowerShell script directly. Works in foreground and background."
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
                description: "Manage background shell sessions: list, poll output, log, write stdin, kill. Responses include idle_s (seconds since last stdout/stderr) and total_output_bytes so you can tell silence from steady output. Background tasks notify you on their own when they finish, so polling is optional. 'kill' ends a session deliberately and suppresses that completion notification.".to_string(),
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
                description: "Write content to a file (creates parent dirs automatically). THE preferred way to write files — no shell escaping issues.\n\nModes:\n- Default: overwrite file with content\n- append=true: add to end of existing file\n- encoding=\"base64\": decode base64 content before writing (for binary files)\n- unescape=true: process escape sequences (\\n to newline, \\t to tab). Default: content written as-is".to_string(),
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
                        "unescape": {
                            "type": "boolean",
                            "description": "If true, process escape sequences (\n→newline, \t→tab). Default: false (content written as-is)."
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
                        },
                        "unescape": {
                            "type": "boolean",
                            "description": "If true, process escape sequences in old_text/new_text (\n→newline, \t→tab). Default: false (match/replace as-is)."
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

        tools.push(ToolInfo {
            name: "portal_oauth_authorize".to_string(),
            description: "Start OAuth Authorization Code + PKCE flow. Opens browser for user authorization and returns tokens.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "provider": { "type": "string", "enum": ["openai"], "description": "OAuth provider" },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default 120)", "default": 120 }
                },
                "required": ["provider"]
            }),
        });

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

        if self.restart_supported {
            tools.push(ToolInfo {
                name: "portal_restart".to_string(),
                description: "Gracefully exit Portal after returning a response so its OS supervisor can restart it with the same name and load updated kits. Use this instead of taskkill or starting another supervisor.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            });
        }

        if self.config.kits_enabled {
            tools.push(ToolInfo {
                name: "portal_kit_usage".to_string(),
                description: "Returns accumulated call counts per kit since last drain, then resets counters to zero".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            });
        }

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
            "portal_oauth_authorize" => oauth::authorize(arguments).await,
            "portal_tools_reload" => self.handle_tools_reload().await,
            "portal_restart" => self.handle_restart().await,
            "portal_kit_usage" => {
                let counts = self.kits.drain_usage_counts().await;
                let text = serde_json::to_string(&counts)?;
                Ok(serde_json::json!({
                    "content": [{"type": "text", "text": text}]
                }))
            }
            _ => anyhow::bail!("Unknown tool: {}", tool_name),
        }
    }

    /// Request a restart; the connection handler schedules it after flushing
    /// the JSON-RPC response, never while it is still being constructed.
    async fn handle_restart(&self) -> Result<Value> {
        if !self.restart_supported {
            anyhow::bail!(
                "Portal restart is unavailable because no external supervisor is configured"
            );
        }
        let already_scheduled = self.restart_requested.swap(true, Ordering::AcqRel);

        let message = if already_scheduled {
            "Portal restart is already scheduled."
        } else {
            "Portal restart scheduled. The supervisor will relaunch it with the same name and load updated kits."
        };
        Ok(serde_json::json!({
            "content": [{"type": "text", "text": message}]
        }))
    }

    pub fn restart_after_response(&self) {
        if self.restart_requested.load(Ordering::Acquire) {
            let restart_notify = self.restart_notify.clone();
            tokio::spawn(async move {
                // The MCP response is flushed. Allow the WebSocket bridge to
                // forward it before the main loop shuts down the process.
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                restart_notify.notify_one();
            });
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

// ── HF-7: coerce string tool args to native types ──────────────────

/// Extract bool from a JSON value, accepting both native bool and string "true"/"false".
pub(crate) fn value_as_bool(v: &Value) -> Option<bool> {
    v.as_bool().or_else(|| v.as_str().and_then(|s| match s {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }))
}

/// Extract u64 from a JSON value, accepting both native number and string digits.
pub(crate) fn value_as_u64(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

#[cfg(test)]
mod restart_tests {
    use super::*;
    use std::time::Duration;

    fn host(supervised: bool) -> ToolHost {
        let mut host = ToolHost::new(&PortalConfig {
            kits_enabled: false,
            ..PortalConfig::default()
        });
        host.restart_supported = supervised;
        host
    }

    #[tokio::test]
    async fn unsupervised_portal_cannot_restart() {
        let host = host(false);
        assert!(!host
            .list_builtin_tools()
            .iter()
            .any(|t| t.name == "portal_restart"));
        assert!(host.handle_restart().await.is_err());
        assert!(!host.restart_requested.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn restart_waits_for_response_and_is_not_lost_before_waiter_starts() {
        let host = host(true);
        assert!(host
            .list_builtin_tools()
            .iter()
            .any(|t| t.name == "portal_restart"));
        host.handle_restart().await.unwrap();
        assert!(host.handle_restart().await.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("already"));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), host.wait_for_restart())
                .await
                .is_err()
        );
        host.restart_after_response();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        tokio::time::timeout(Duration::from_millis(100), host.wait_for_restart())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn connection_flushes_restart_reply_before_scheduling_exit() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let host = host(true);
        let server_host = host.clone();
        // The reply exceeds this capacity: flushing must wait for our read.
        let (client, server) = tokio::io::duplex(32);
        let handler = tokio::spawn(async move {
            crate::handle_connection(server, &server_host, "test", None).await
        });
        let (reader, mut writer) = tokio::io::split(client);
        writer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"portal_restart\",\"arguments\":{}}}\n").await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(1200), host.wait_for_restart())
                .await
                .is_err()
        );
        let mut reader = BufReader::new(reader);
        let mut reply = String::new();
        tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut reply))
            .await
            .unwrap()
            .unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["id"], 1);
        assert!(reply.get("error").is_none());
        tokio::time::timeout(Duration::from_secs(3), host.wait_for_restart())
            .await
            .unwrap();
        handler.abort();
        let _ = handler.await;
    }
}
