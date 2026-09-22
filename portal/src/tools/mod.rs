//! Tool host — manages built-in, custom (being-defined), and kit tools.
//! Built-in: exec, file, web. Custom: loaded from workspace/tools/mcp.toml.

pub mod client;
pub mod custom;
mod exec;
mod file;
mod oauth;
#[cfg(target_os = "macos")]
mod permissions;
mod process;
mod screenshot;
mod search;
pub(crate) mod status;
mod subagent;
pub(crate) mod text;
#[cfg(test)]
mod utf8_tests;
mod web;

use crate::config::PortalConfig;
use crate::heart_callback::HeartCallback;
use crate::kits::{loader, manager::KitManager};
use crate::process_manager::ProcessManager;
use crate::subagent::SubagentManager;
use anyhow::{Context, Result};
use custom::CustomToolHost;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
    client_handler: Arc<dyn client::ClientHandler>,
    custom: CustomToolHost,
    kits: KitManager,
    pub process_manager: Arc<ProcessManager>,
    /// The being's native sub-agent. Always present; unavailable when pi is
    /// not installed, in which case its tools are not advertised.
    pub subagent: Arc<SubagentManager>,
    /// Shared Heart callback handle — every manager that can finish work
    /// asynchronously delivers through this one client/retry policy.
    callback: HeartCallback,
    /// Each connected client receives its own change cursor, including idle clients.
    tools_changed: tokio::sync::watch::Sender<u64>,
    kit_refresh_lock: Arc<tokio::sync::Mutex<()>>,
    custom_reload_lock: Arc<tokio::sync::Mutex<()>>,
    /// A controlled restart requested through the built-in portal_restart tool.
    restart_requested: Arc<AtomicBool>,
    restart_notify: Arc<tokio::sync::Notify>,
    restart_supported: bool,
    runtime: Arc<status::RuntimeStatus>,
}

impl ToolHost {
    #[cfg(test)]
    pub fn new(config: &PortalConfig) -> Self {
        Self::new_with_runtime(config, status::RuntimeStatus::for_test(config))
    }

    pub fn new_with_runtime(config: &PortalConfig, runtime: status::RuntimeStatus) -> Self {
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

        let callback = HeartCallback::new();

        Self {
            runtime: Arc::new(runtime),
            config: config.clone(),
            client_handler: Arc::new(client::NoClientHandler),
            custom: CustomToolHost::new(),
            kits: KitManager::new(loaded_kits),
            process_manager: Arc::new(ProcessManager::new(callback.clone())),
            subagent: SubagentManager::new(config, callback.clone()),
            callback,
            tools_changed: tokio::sync::watch::channel(0).0,
            kit_refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
            custom_reload_lock: Arc::new(tokio::sync::Mutex::new(())),
            restart_requested: Arc::new(AtomicBool::new(false)),
            restart_notify: Arc::new(tokio::sync::Notify::new()),
            restart_supported: std::env::var("HEART_PORTAL_SUPERVISED")
                .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
                || {
                    #[cfg(target_os = "macos")]
                    {
                        crate::macos_supervisor::attached()
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        false
                    }
                },
        }
    }

    pub fn with_client_handler(mut self, handler: Arc<dyn client::ClientHandler>) -> Self {
        self.client_handler = handler;
        self
    }

    /// Enable async callbacks for every manager at once (`--connect` mode).
    /// Replaces the old direct `process_manager.set_callback_config` call.
    ///
    /// Reconciliation of sub-agent tasks orphaned by a restart is kicked off
    /// here rather than at construction: before this point there is nowhere to
    /// deliver the `interrupted` results to.
    pub fn set_callback_config(&self, url: String, token: String, portal_name: String) {
        self.callback.set(url, token, portal_name);
        let subagent = Arc::clone(&self.subagent);
        tokio::spawn(async move { subagent.reconcile().await });
    }

    /// Wait until a tool caller has requested a controlled Portal restart.
    pub async fn wait_for_restart(&self) {
        self.restart_notify.notified().await;
    }

    pub async fn kill_all_managed_processes(&self) {
        // Shut the sub-agent down alongside the rest: its daemon is a child
        // process too, and a leaked pi daemon outlives Portal otherwise.
        let cleanup = async {
            tokio::join!(
                self.process_manager.kill_all(),
                self.kits.shutdown(),
                self.custom.shutdown(),
                self.subagent.shutdown(),
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
        self.subagent.cleanup().await;
    }

    /// Load custom tools from workspace/tools/mcp.toml
    pub async fn load_custom_tools(&self) -> Result<usize> {
        let _guard = self.custom_reload_lock.lock().await;
        if !self.config.tools.custom_tools_enabled {
            return Ok(0);
        }
        let count = self
            .custom
            .load(&self.config.security.workspace_root)
            .await?;
        if count > 0 {
            self.notify_tools_changed();
        }
        Ok(count)
    }

    /// Pre-spawn eager kits (manifest.eager == true) so the first call has
    /// no cold-start latency. Failures are logged, not fatal.
    pub async fn warmup_kits(&self) {
        self.kits.warmup().await;
        self.subagent.warmup().await;
    }

    /// Periodically re-scan the kits directory and refresh manifests in place.
    pub fn start_kit_refresh_task(&self) -> tokio::task::JoinHandle<()> {
        let host = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(
                status::KIT_REFRESH_INTERVAL_SECS,
            ));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if let Err(err) = host.refresh_kits(false).await {
                    warn!("Failed to refresh kits: {}", err);
                }
            }
        })
    }

    pub fn subscribe_tools_changed(&self) -> tokio::sync::watch::Receiver<u64> {
        self.tools_changed.subscribe()
    }

    fn notify_tools_changed(&self) {
        self.tools_changed
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    async fn refresh_kits(&self, force: bool) -> Result<crate::kits::manager::KitReloadReport> {
        self.refresh_kits_target(force, None).await
    }

    async fn refresh_kits_target(
        &self,
        force: bool,
        target: Option<&str>,
    ) -> Result<crate::kits::manager::KitReloadReport> {
        if !self.config.kits_enabled {
            return Ok(Default::default());
        }
        // Serialize scan + reconcile so an older background scan cannot undo
        // credentials/manifests just applied by an explicit reload.
        let _guard = self.kit_refresh_lock.lock().await;
        let kits_dir = loader::kits_dir(&self.config);
        let fresh =
            tokio::task::spawn_blocking(move || loader::scan_kits_from_dir(&kits_dir)).await??;
        if let Some(target) = target {
            if !fresh.kits.iter().any(|kit| kit.manifest.name == target) {
                anyhow::bail!(
                    "Kit '{}' has no loadable manifest; check its installation and manifest.json",
                    target
                );
            }
        }
        let report = self.kits.refresh_kits_target(fresh, force, target).await;
        if report.changed() {
            self.notify_tools_changed();
        }
        Ok(report)
    }

    /// Reload custom tools and notify connected clients.
    pub async fn reload_custom_tools(&self) -> Result<(usize, Vec<String>)> {
        let _guard = self.custom_reload_lock.lock().await;
        // Shutdown existing custom MCP servers
        self.custom.shutdown().await;
        if !self.config.tools.custom_tools_enabled {
            return Ok((0, vec![]));
        }
        // Reload from config
        let count = self
            .custom
            .load(&self.config.security.workspace_root)
            .await?;
        let names: Vec<String> = self
            .custom
            .list_tools()
            .await
            .iter()
            .map(|t| t.name.clone())
            .collect();
        self.notify_tools_changed();
        info!("Tools reloaded: {} custom tools.", count);
        Ok((count, names))
    }

    /// List all available tools (built-in + custom)
    pub async fn list_tools(&self) -> Vec<ToolInfo> {
        let mut tools = self.list_builtin_tools();
        let custom = self.custom.list_tools().await;
        let kit_tools = self.kits.list_healthy_tools().await;
        let external: Vec<_> = custom.into_iter().chain(kit_tools).collect();
        let mut counts = std::collections::HashMap::new();
        for tool in &external {
            *counts.entry(tool.name.replace('-', "_")).or_insert(0) += 1;
        }
        tools.extend(external.into_iter().filter(|tool| {
            let name = tool.name.replace('-', "_");
            !name.starts_with("portal_") && counts[&name] == 1
        }));
        tools
    }

    /// List built-in tools only
    fn list_builtin_tools(&self) -> Vec<ToolInfo> {
        let mut tools = vec![ToolInfo {
            name: "portal_status".into(),
            description: "Read this running Portal's version, startup binary SHA-256 build ID, effective configuration paths and switches, live connection state and loaded kit summary. Read-only: no reload, process launch or credential values. Configuration presence does not validate service permissions. Use portal_kits_setup for kit authorization requirements.".into(),
            input_schema: serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
        }];

        if self.config.tools.exec {
            tools.push(ToolInfo {
                name: "portal_exec".to_string(),
                description: "Execute a shell command, or a client command: @context [scene_id] reads recent scene conversation history (defaults to the calling scene); @scenes lists available scenes. Client commands never run in a shell. With background=true it returns a session_id immediately and, when the task finishes, Portal notifies you automatically — you will be woken with the exit code and output, so you can let go of it instead of polling. Prefer background=true for anything slow (builds, tests, long downloads). On Windows, select shell='powershell' and pass the script directly for PowerShell; do not invoke powershell.exe from the default cmd shell.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "Shell command to execute. On Windows, PowerShell scripts require shell='powershell' and must be passed directly."
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
                        },
                        "output_encoding": {
                            "type": "string",
                            "enum": text::OutputEncoding::supported_values(),
                            "description": "Source output decoding (default: auto). PowerShell and non-Windows default to UTF-8. Windows cmd auto tries UTF-8 then system OEM per line; non-ASCII lines may wait for newline/EOF (buffer capped at 64KiB). Use utf8 or oem for known encodings and immediate streaming; oem requires Windows. A mixed-encoding line is ambiguous. Responses are always UTF-8 text, including poll/log and callbacks."
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
                        "offset": { "type": "integer", "minimum": 0, "description": "UTF-8 byte offset into normalized output (poll/log, default 0); use the returned next_offset. total_output_bytes uses the same units, not source-encoding bytes." },
                        "limit": { "type": "integer", "minimum": 1, "description": "Max UTF-8 bytes for log (default 65536). Must fit the next complete character; at least 4 avoids character-size errors." },
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
                            "description": "Maximum response body bytes (default: 50000); preserves complete UTF-8 characters"
                        }
                    },
                    "required": ["url"]
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

        #[cfg(target_os = "macos")]
        tools.push(ToolInfo {
            name: "portal_permissions".to_string(),
            description: "Check this running Portal process's macOS screen recording, accessibility and input monitoring permissions without prompting or changing grants. Use before and after an upgrade; kit permissions are separate.".to_string(),
            input_schema: serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
        });

        // Always include tools_reload
        tools.push(ToolInfo {
            name: "portal_tools_reload".to_string(),
            description: "Reload custom tools from workspace/tools/mcp.toml. Kit changes use portal_kits_reload. Connected clients are notified; Portal stays running.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        });

        if self.restart_supported {
            tools.push(ToolInfo {
                name: "portal_restart".to_string(),
                description: "Gracefully exit Portal after returning a response so its OS supervisor can restart it with the same name. For kit changes use portal_kits_reload.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            });
        }

        // Only advertise the sub-agent when it can actually run: a being should
        // never be offered a tool that fails on every call. The one exception
        // is setup, which is how a missing pi gets installed.
        if self.subagent.is_available() {
            tools.extend(subagent::list_tools());
        } else if self.subagent.setup_offered() {
            tools.push(subagent::setup_tool());
        }

        if self.config.kits_enabled {
            tools.push(ToolInfo {
                name: "portal_kits_status".into(),
                description: "Read cached kit configuration requirements, missing credentials and process status without scanning or reloading. Returns variable names and configured flags, never credential values. Service authorization is checked by each kit.".into(),
                input_schema: serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
            });
            tools.push(ToolInfo {
                name: "portal_kits_setup".into(),
                description: "Read one installed kit's cached runtime, dependencies, installation instructions and available auth methods (environment credentials, credential files or kit-managed OAuth/device/CLI login). Returns setup tool names and missing requirements, never local credential values. Does not execute setup steps or grant service access.".into(),
                input_schema: serde_json::json!({"type": "object", "properties": {"kit": {"type": "string", "description": "Installed kit name"}}, "required": ["kit"], "additionalProperties": false}),
            });
            tools.push(ToolInfo {
                name: "portal_kits_reload".into(),
                description: "Reload kit code, manifests and credentials without restarting Portal. Pass kit to change only that kit, or omit to discover and reload all. Existing calls finish on their old process. Returns configuration status.".into(),
                input_schema: serde_json::json!({"type": "object", "properties": {"kit": {"type": "string", "description": "Optional installed kit name to reload"}}, "additionalProperties": false}),
            });
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
        self.call_with_scene(tool_name, arguments, None).await
    }

    pub async fn call_with_scene(
        &self,
        tool_name: &str,
        arguments: Value,
        scene_id: Option<&str>,
    ) -> Result<Value> {
        // Reserve the diagnostic endpoint: an installed server must not replace
        // a read-only status query with an arbitrary custom or kit operation.
        if tool_name == "portal_status" {
            anyhow::ensure!(
                arguments.is_null() || arguments.as_object().is_some_and(|args| args.is_empty()),
                "portal_status takes no arguments"
            );
            return self.handle_status().await;
        }
        // The Portal management namespace can never be replaced by community tools.
        if !tool_name.replace('-', "_").starts_with("portal_") {
            let kit = self.kits.resolve_tool(tool_name).await;
            let custom_matches = self.custom.tool_match_count(tool_name).await;
            anyhow::ensure!(custom_matches + usize::from(kit.is_some()) <= 1,
                "Ambiguous external tool name; rename the conflicting custom/kit tool before calling it");
            if self.custom.has_tool(tool_name).await {
                return self.custom.call(tool_name, arguments).await;
            }
            if let Some((kit_name, real_tool_name)) = kit {
                return self
                    .kits
                    .call_tool(&kit_name, &real_tool_name, arguments)
                    .await;
            }
        }

        // Built-in tools
        match tool_name {
            "portal_exec" => {
                if !self.config.tools.exec {
                    anyhow::bail!("portal_exec is disabled in configuration");
                }
                if let Some(command) = arguments
                    .get("command")
                    .and_then(Value::as_str)
                    .and_then(|s| s.trim_start().strip_prefix('@'))
                {
                    let (verb, args) = command
                        .split_once(char::is_whitespace)
                        .unwrap_or((command, ""));
                    anyhow::ensure!(!verb.is_empty(), "Missing client command after @");
                    let text = self
                        .client_handler
                        .handle_client_command(verb, args.trim(), scene_id)
                        .await?;
                    return Ok(serde_json::json!({"content": [{"type": "text", "text": text}]}));
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
            "portal_oauth_authorize" => oauth::authorize(arguments).await,
            name if subagent::is_subagent_tool(name) => {
                subagent::handle(&self.subagent, name, arguments).await
            }
            "portal_tools_reload" => self.handle_tools_reload().await,
            "portal_kits_setup" => {
                if !self.config.kits_enabled {
                    anyhow::bail!("Kits are disabled in configuration");
                }
                let kit = arguments
                    .get("kit")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow::anyhow!("Missing or invalid 'kit' argument"))?;
                let setup = self.kits.setup(kit).await?;
                Ok(
                    serde_json::json!({"content": [{"type": "text", "text": serde_json::to_string(&setup)?}]}),
                )
            }
            "portal_kits_reload" | "portal_kits_status" => {
                if !self.config.kits_enabled {
                    anyhow::bail!("Kits are disabled in configuration");
                }
                if tool_name == "portal_kits_reload" {
                    let fields = arguments
                        .as_object()
                        .context("Reload arguments must be an object")?;
                    anyhow::ensure!(fields.keys().all(|key| key == "kit"),
                        "Unknown reload argument; use 'kit' to select one kit, or an empty object to reload all");
                }
                let target = match arguments.get("kit") {
                    None => None,
                    Some(Value::String(kit)) if !kit.trim().is_empty() => Some(kit.as_str()),
                    _ => anyhow::bail!("'kit' must be a nonempty kit name"),
                };
                let report = if tool_name == "portal_kits_reload" {
                    self.refresh_kits_target(true, target).await?
                } else {
                    Default::default()
                };
                let statuses = self.kits.statuses().await;
                Ok(serde_json::json!({
                    "content": [{"type": "text", "text": serde_json::to_string(&statuses)?}],
                    "structuredContent": {
                        "schema_version": 1,
                        "kits": report,
                        "statuses": statuses,
                        "portal_restart_required": false,
                        "activation": "next-tool-call",
                    }
                }))
            }
            "portal_restart" => self.handle_restart().await,
            #[cfg(target_os = "macos")]
            "portal_permissions" => Ok(permissions::status()),
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
            "Portal restart scheduled. The supervisor will relaunch it with the same name."
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

    /// Reload custom tools and return a short summary.
    async fn handle_tools_reload(&self) -> Result<Value> {
        let (count, names) = self.reload_custom_tools().await?;
        let message = if count == 0 {
            "Reloaded. No custom tools found in workspace/tools/mcp.toml.".to_string()
        } else {
            format!("Reloaded {} custom tools: {}.", count, names.join(", "))
        };
        Ok(serde_json::json!({
            "content": [{"type": "text", "text": message}],
            "isError": false
        }))
    }
}

#[cfg(test)]
mod kit_refresh_tests {
    use super::*;
    use crate::kits::tests::TestKits;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf};

    async fn read_message(reader: &mut BufReader<ReadHalf<DuplexStream>>) -> Value {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(8), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[tokio::test]
    async fn web_search_is_not_registered_or_dispatchable() {
        let host = ToolHost::new(&PortalConfig::default());
        assert!(!host
            .list_builtin_tools()
            .iter()
            .any(|tool| tool.name == "portal_web_search"));
        let error = host
            .call("portal_web_search", serde_json::json!({"query": "test"}))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "Unknown tool: portal_web_search");
    }

    #[tokio::test]
    async fn idle_clients_receive_reload_and_partial_request_survives_notification() {
        let root = TestKits::new();
        let host = ToolHost::new(&PortalConfig {
            kits_enabled: true,
            kits_dir: Some(root.0.to_string_lossy().into_owned()),
            ..PortalConfig::default()
        });
        let mut readers = Vec::new();
        let mut writers = Vec::new();
        let mut handlers = Vec::new();
        for _ in 0..2 {
            let (client, server) = tokio::io::duplex(65536);
            let server_host = host.clone();
            handlers.push(tokio::spawn(async move {
                crate::handle_connection(server, &server_host, "test", None).await
            }));
            let (reader, mut writer) = tokio::io::split(client);
            let mut reader = BufReader::new(reader);
            writer
                .write_all(
                    b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
                )
                .await
                .unwrap();
            assert_eq!(
                read_message(&mut reader).await["result"]["capabilities"]["tools"]["listChanged"],
                true
            );
            readers.push(reader);
            writers.push(writer);
        }
        writers[0]
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,")
            .await
            .unwrap();
        let dir = root.install("jira", "PORTAL_TEST_KIT_TOKEN=private-test-value");
        host.call("portal_kits_reload", serde_json::json!({}))
            .await
            .unwrap();
        for reader in &mut readers {
            assert_eq!(
                read_message(reader).await["method"],
                "notifications/tools/list_changed"
            );
        }
        writers[0]
            .write_all(b"\"method\":\"tools/list\",\"params\":{}}\n")
            .await
            .unwrap();
        let reply = read_message(&mut readers[0]).await;
        assert_eq!(reply["id"], 2);
        assert!(reply["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "jira_ping"));

        // A real timer scan, with no MCP call to trigger discovery. Both idle
        // connections learn that a now-unconfigured kit is no longer callable.
        let refresh = host.start_kit_refresh_task();
        root.write_env(&dir, "");
        for reader in &mut readers {
            assert_eq!(
                read_message(reader).await["method"],
                "notifications/tools/list_changed"
            );
        }
        let status = host
            .call("portal_kits_status", serde_json::json!({}))
            .await
            .unwrap();
        assert!(status.to_string().contains("needs-configuration"));
        assert!(!status.to_string().contains("private-test-value"));
        assert!(!host
            .list_tools()
            .await
            .iter()
            .any(|t| t.name == "jira_ping"));
        refresh.abort();
        let _ = refresh.await;
        for handler in handlers {
            handler.abort();
            let _ = handler.await;
        }
        host.kill_all_managed_processes().await;
    }

    #[tokio::test]
    async fn disabled_kits_cannot_be_loaded_by_reload_tools_or_background_scan() {
        let root = TestKits::new();
        root.install("jira", "PORTAL_TEST_KIT_TOKEN=unused");
        let host = ToolHost::new(&PortalConfig {
            kits_enabled: false,
            kits_dir: Some(root.0.to_string_lossy().into_owned()),
            ..PortalConfig::default()
        });
        let mut changes = host.subscribe_tools_changed();
        host.refresh_kits(true).await.unwrap();
        assert!(!changes.has_changed().unwrap());
        assert!(host
            .call("portal_kits_reload", serde_json::json!({}))
            .await
            .is_err());
        assert!(!host
            .list_tools()
            .await
            .iter()
            .any(|t| t.name.starts_with("jira_")));
        // This also proves custom-tool reload uses the same broadcast path.
        host.reload_custom_tools().await.unwrap();
        changes.changed().await.unwrap();
    }
}

// ── HF-7: coerce string tool args to native types ──────────────────

/// Extract bool from a JSON value, accepting both native bool and string "true"/"false".
pub(crate) fn value_as_bool(v: &Value) -> Option<bool> {
    v.as_bool().or_else(|| {
        v.as_str().and_then(|s| match s {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        })
    })
}

/// Extract u64 from a JSON value, accepting both native number and string digits.
pub(crate) fn value_as_u64(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_workspace(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("portal-toolhost-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config_with_subagent(workspace: &PathBuf, command: Option<Vec<String>>) -> PortalConfig {
        let mut config = PortalConfig::default();
        config.security.workspace_root = workspace.clone();
        config.kits_enabled = false;
        config.subagent.state_dir = Some(workspace.join("state").display().to_string());
        config.subagent.command = command;
        config
    }

    fn tool_names(host: &ToolHost) -> Vec<String> {
        host.list_builtin_tools()
            .into_iter()
            .map(|t| t.name)
            .collect()
    }

    #[test]
    fn subagent_tools_are_advertised_only_when_pi_resolves() {
        let ws = temp_workspace("available");
        // A resolvable binary stands in for pi: availability is about the
        // command resolving, not about what it is.
        let host = ToolHost::new(&config_with_subagent(&ws, Some(vec!["/bin/sh".to_string()])));
        let names = tool_names(&host);
        assert!(host.subagent.is_available());
        for expected in subagent::TOOL_NAMES {
            assert!(names.contains(&expected.to_string()), "missing {expected} in {names:?}");
        }
        let _ = std::fs::remove_dir_all(ws);
    }

    #[test]
    fn subagent_tools_are_hidden_when_pi_is_missing() {
        let ws = temp_workspace("missing");
        let host = ToolHost::new(&config_with_subagent(
            &ws,
            Some(vec![ws.join("no-such-pi").display().to_string()]),
        ));
        assert!(!host.subagent.is_available());
        let names = tool_names(&host);
        assert!(
            !names.iter().any(|n| n.starts_with("portal_subagent_")),
            "a being must not be offered tools that cannot run: {names:?}"
        );
        // The rest of the built-ins are unaffected.
        assert!(names.contains(&"portal_exec".to_string()));
        let _ = std::fs::remove_dir_all(ws);
    }

    #[test]
    fn subagent_tools_are_hidden_when_disabled() {
        let ws = temp_workspace("disabled");
        let mut config = config_with_subagent(&ws, Some(vec!["/bin/sh".to_string()]));
        config.subagent.enabled = false;
        let host = ToolHost::new(&config);
        assert!(!host.subagent.is_available());
        assert!(!tool_names(&host)
            .iter()
            .any(|n| n.starts_with("portal_subagent_")));
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn unavailable_subagent_still_answers_its_tools_with_a_reason() {
        let ws = temp_workspace("callreason");
        let host = ToolHost::new(&config_with_subagent(
            &ws,
            Some(vec![ws.join("no-such-pi").display().to_string()]),
        ));
        // Dispatch does not depend on advertisement: a stale client may still call.
        let resp = host
            .call("portal_subagent_spawn", serde_json::json!({"brief": "do it"}))
            .await
            .unwrap();
        assert_eq!(resp["isError"], true);
        let text = resp["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("no pi binary found"), "{text}");
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn subagent_setup_is_dispatched_and_no_status_surface_leaks_the_key() {
        let ws = temp_workspace("setupdispatch");
        let mut config = config_with_subagent(&ws, Some(vec!["/bin/sh".to_string()]));
        let path = ws.join("portal.toml");
        std::fs::write(&path, "name = \"vale\"\n").unwrap();
        config.config_path = Some(path.clone());
        config.subagent.env_passthrough = vec!["PATH".to_string()];
        let host = ToolHost::new(&config);
        assert!(host.subagent.needs_setup());

        let secret = "sk-ant-api03-host-dispatch-secret";
        let resp = host
            .call(
                "portal_subagent_setup",
                serde_json::json!({"provider": "anthropic", "model": "claude-sonnet-4-5", "api_key": secret}),
            )
            .await
            .unwrap();
        assert_eq!(resp["isError"], false, "{resp}");
        assert!(!host.subagent.needs_setup());
        assert!(PortalConfig::load(path.to_str().unwrap())
            .unwrap()
            .subagent
            .model
            .api_key
            .is_some());

        for tool in ["portal_subagent_setup", "portal_subagent_status", "portal_status"] {
            let resp = host.call(tool, serde_json::json!({})).await.unwrap();
            assert_eq!(resp["isError"], false, "{tool}: {resp}");
            assert!(!resp.to_string().contains(secret), "{tool} leaked the key: {resp}");
        }
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn shutdown_and_cleanup_cover_the_subagent() {
        let ws = temp_workspace("shutdown");
        let host = ToolHost::new(&config_with_subagent(&ws, Some(vec!["/bin/sh".to_string()])));
        // Neither path may hang or panic when nothing is running.
        host.cleanup_background_sessions().await;
        host.kill_all_managed_processes().await;
        let _ = std::fs::remove_dir_all(ws);
    }
}
