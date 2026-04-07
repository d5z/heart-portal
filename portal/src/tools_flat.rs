//! Built-in tools for Heart Portal: exec, file, web_fetch
//! Plus support for loading custom MCP tools from workspace/tools/mcp.toml

use crate::config::PortalConfig;
use crate::extensions::ExtensionManager;
use anyhow::{Result, Context};
use serde_json::{json, Value, to_value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{info, warn, debug};
use reqwest;
use toml;

/// Tool information for MCP protocol
#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Host for built-in and custom MCP tools
#[derive(Debug, Clone)]
pub struct ToolHost {
    config: PortalConfig,
    extension_manager: Arc<ExtensionManager>,
    custom_tools: Arc<tokio::sync::RwLock<HashMap<String, CustomTool>>>,
    pub needs_reconnect: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
struct CustomTool {
    name: String,
    description: String,
    input_schema: Value,
    command: Vec<String>,
    working_dir: Option<PathBuf>,
}

impl ToolHost {
    pub fn new(config: &PortalConfig) -> Self {
        Self {
            config: config.clone(),
            extension_manager: Arc::new(ExtensionManager::new(config.security.workspace_root.clone())),
            custom_tools: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            needs_reconnect: Arc::new(AtomicBool::new(false)),
        }
    }

    /// List all available tools (built-in + custom + extensions)
    pub async fn list_tools(&self) -> Vec<ToolInfo> {
        let mut tools = Vec::new();

        // Built-in tools based on config
        if self.config.tools.exec {
            tools.push(ToolInfo {
                name: "exec".to_string(),
                description: "Execute shell commands in the workspace".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "Shell command to execute"
                        },
                        "working_dir": {
                            "type": "string",
                            "description": "Working directory (relative to workspace root)"
                        }
                    },
                    "required": ["command"]
                }),
            });
        }

        if self.config.tools.file {
            tools.push(ToolInfo {
                name: "file_read".to_string(),
                description: "Read file contents from the workspace".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path relative to workspace root"
                        }
                    },
                    "required": ["path"]
                }),
            });

            tools.push(ToolInfo {
                name: "file_write".to_string(),
                description: "Write content to a file in the workspace".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path relative to workspace root"
                        },
                        "content": {
                            "type": "string",
                            "description": "Content to write to the file"
                        }
                    },
                    "required": ["path", "content"]
                }),
            });

            tools.push(ToolInfo {
                name: "file_list".to_string(),
                description: "List files and directories in the workspace".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory path relative to workspace root (default: '.')"
                        }
                    }
                }),
            });
        }

        if self.config.tools.web_fetch {
            tools.push(ToolInfo {
                name: "web_fetch".to_string(),
                description: "Fetch content from a URL".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "URL to fetch"
                        },
                        "method": {
                            "type": "string",
                            "description": "HTTP method (default: GET)",
                            "enum": ["GET", "POST", "PUT", "DELETE"]
                        },
                        "headers": {
                            "type": "object",
                            "description": "HTTP headers to send"
                        },
                        "body": {
                            "type": "string",
                            "description": "Request body for POST/PUT"
                        }
                    },
                    "required": ["url"]
                }),
            });
        }

        // Extension management tools
        tools.push(ToolInfo {
            name: "extensions_list".to_string(),
            description: "List all available extensions and their status".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        });

        tools.push(ToolInfo {
            name: "extensions_start".to_string(),
            description: "Start an extension by name".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Extension name to start"
                    }
                },
                "required": ["name"]
            }),
        });

        tools.push(ToolInfo {
            name: "extensions_stop".to_string(),
            description: "Stop a running extension".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Extension name to stop"
                    }
                },
                "required": ["name"]
            }),
        });

        tools.push(ToolInfo {
            name: "extensions_reload".to_string(),
            description: "Reload extensions configuration and restart all extensions".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        });

        // Add custom tools
        let custom = self.custom_tools.read().await;
        for tool in custom.values() {
            tools.push(ToolInfo {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
            });
        }

        // Add extension tools
        let extension_tools = self.extension_manager.get_all_tools().await;
        tools.extend(extension_tools);

        tools
    }

    /// Call a tool by name
    pub async fn call(&self, name: &str, arguments: Value) -> Result<Value> {
        match name {
            "exec" => self.exec_tool(arguments).await,
            "file_read" => self.file_read_tool(arguments).await,
            "file_write" => self.file_write_tool(arguments).await,
            "file_list" => self.file_list_tool(arguments).await,
            "web_fetch" => self.web_fetch_tool(arguments).await,
            "extensions_list" => self.extensions_list_tool().await,
            "extensions_start" => self.extensions_start_tool(arguments).await,
            "extensions_stop" => self.extensions_stop_tool(arguments).await,
            "extensions_reload" => self.extensions_reload_tool().await,
            _ => {
                // Check custom tools
                let custom = self.custom_tools.read().await;
                if let Some(tool) = custom.get(name) {
                    self.call_custom_tool(tool, arguments).await
                } else {
                    // Check extension tools
                    self.extension_manager.call_tool(name, arguments).await
                }
            }
        }
    }

    /// Load custom tools from workspace/tools/mcp.toml
    pub async fn load_custom_tools(&self) -> Result<usize> {
        let tools_config_path = self.config.security.workspace_root.join("tools/mcp.toml");
        
        if !tools_config_path.exists() {
            return Ok(0);
        }

        let content = fs::read_to_string(&tools_config_path).await
            .context("Failed to read tools config")?;
        
        let config: toml::Value = toml::from_str(&content)
            .context("Failed to parse tools config")?;

        let mut custom_tools = self.custom_tools.write().await;
        custom_tools.clear();

        let mut count = 0;
        if let Some(tools) = config.get("tools").and_then(|t| t.as_table()) {
            for (name, tool_config) in tools {
                if let Some(tool) = self.parse_custom_tool(name, tool_config)? {
                    custom_tools.insert(name.clone(), tool);
                    count += 1;
                }
            }
        }

        // Set reconnect flag if we loaded any tools
        if count > 0 {
            self.needs_reconnect.store(true, Ordering::SeqCst);
        }

        Ok(count)
    }

    /// Load extensions from extensions.toml
    pub async fn load_extensions(&self) -> Result<usize> {
        let _config = self.extension_manager.load_config().await?;
        // Initialize extensions
        self.extension_manager.initialize().await?;
        
        // Count how many extensions we have
        let status = self.extension_manager.get_status().await;
        let count = status.len();
        
        if count > 0 {
            self.needs_reconnect.store(true, Ordering::SeqCst);
        }
        
        Ok(count)
    }

    /// Reload all extensions
    pub async fn reload_extensions(&self) -> Result<Vec<String>> {
        // Stop all running extensions
        let status = self.extension_manager.get_status().await;
        let mut changes = Vec::new();
        
        for name in status.keys() {
            let _ = self.extension_manager.stop_extension(name).await;
            changes.push(format!("Stopped extension: {}", name));
        }
        
        // Reload configuration and restart
        let _count = self.load_extensions().await?;
        changes.push("Reloaded extensions configuration".to_string());
        
        self.needs_reconnect.store(true, Ordering::SeqCst);
        Ok(changes)
    }

    /// Initialize extensions (called at startup)
    pub async fn initialize_extensions(&self) -> Result<()> {
        // Load and initialize extensions
        self.load_extensions().await?;
        Ok(())
    }

    /// Get extension status for JSON-RPC response
    pub async fn get_extension_status(&self) -> serde_json::Value {
        let status_map = self.extension_manager.get_status().await;
        let mut result = serde_json::Map::new();
        
        for (name, (status, error)) in status_map {
            let status_str = match status {
                crate::extensions::ExtensionStatus::Stopped => "stopped",
                crate::extensions::ExtensionStatus::Starting => "starting",
                crate::extensions::ExtensionStatus::Running => "running",
                crate::extensions::ExtensionStatus::Failed => "failed",
                crate::extensions::ExtensionStatus::Restarting => "restarting",
            };
            
            let mut ext_info = serde_json::Map::new();
            ext_info.insert("status".to_string(), serde_json::Value::String(status_str.to_string()));
            if let Some(err) = error {
                ext_info.insert("error".to_string(), serde_json::Value::String(err));
            }
            
            result.insert(name, serde_json::Value::Object(ext_info));
        }
        
        serde_json::Value::Object(result)
    }

    /// Start a specific extension
    pub async fn start_extension(&self, name: &str) -> Result<()> {
        self.extension_manager.start_extension(name).await?;
        self.needs_reconnect.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Stop a specific extension
    pub async fn stop_extension(&self, name: &str) -> Result<()> {
        self.extension_manager.stop_extension(name).await?;
        self.needs_reconnect.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Restart a specific extension
    pub async fn restart_extension(&self, name: &str) -> Result<()> {
        self.extension_manager.restart_extension(name).await?;
        self.needs_reconnect.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Start all configured extensions
    pub async fn start_extensions(&self) -> Result<()> {
        // Load and initialize extensions first
        self.load_extensions().await?;
        self.needs_reconnect.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn parse_custom_tool(&self, name: &str, config: &toml::Value) -> Result<Option<CustomTool>> {
        let description = config.get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("Custom tool")
            .to_string();

        let command = config.get("command")
            .and_then(|c| c.as_array())
            .ok_or_else(|| anyhow::anyhow!("Tool '{}' missing command array", name))?
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>();

        if command.is_empty() {
            return Err(anyhow::anyhow!("Tool '{}' has empty command", name));
        }

        let working_dir = config.get("working_dir")
            .and_then(|w| w.as_str())
            .map(|w| self.config.security.workspace_root.join(w));

        let input_schema = config.get("input_schema")
            .cloned()
            .unwrap_or_else(|| toml::Value::try_from(serde_json::json!({})).unwrap());

        let input_schema = to_value(input_schema)
            .context("Failed to convert input_schema to JSON")?;

        Ok(Some(CustomTool {
            name: name.to_string(),
            description,
            input_schema,
            command,
            working_dir,
        }))
    }

    async fn call_custom_tool(&self, tool: &CustomTool, arguments: Value) -> Result<Value> {
        let mut cmd = Command::new(&tool.command[0]);
        cmd.args(&tool.command[1..]);

        if let Some(working_dir) = &tool.working_dir {
            cmd.current_dir(working_dir);
        } else {
            cmd.current_dir(&self.config.security.workspace_root);
        }

        // Pass arguments as JSON via stdin
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn()
            .context("Failed to spawn custom tool process")?;

        // Write arguments to stdin
        if let Some(mut stdin) = child.stdin.take() {
            let args_json = serde_json::to_string(&arguments)?;
            stdin.write_all(args_json.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        let output = child.wait_with_output().await?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            Ok(json!({
                "content": [{"type": "text", "text": stdout}],
                "isError": false
            }))
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Ok(json!({
                "content": [{"type": "text", "text": stderr}],
                "isError": true
            }))
        }
    }

    async fn exec_tool(&self, arguments: Value) -> Result<Value> {
        if !self.config.tools.exec {
            return Err(anyhow::anyhow!("exec tool is disabled"));
        }

        let command = arguments.get("command")
            .and_then(|c| c.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'command' parameter"))?;

        let working_dir = arguments.get("working_dir")
            .and_then(|w| w.as_str())
            .map(|w| self.config.security.workspace_root.join(w))
            .unwrap_or_else(|| self.config.security.workspace_root.clone());

        // Security: check allowlist
        if !self.config.security.exec_allowlist.is_empty() {
            let allowed = self.config.security.exec_allowlist.iter()
                .any(|pattern| command.contains(pattern));
            if !allowed {
                return Ok(json!({
                    "content": [{"type": "text", "text": "Command not allowed by exec allowlist"}],
                    "isError": true
                }));
            }
        }

        debug!("Executing: {} (cwd: {:?})", command, working_dir);

        let output = Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&working_dir)
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let mut text = String::new();
        if !stdout.is_empty() {
            text.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !text.is_empty() {
                text.push_str("\n--- stderr ---\n");
            }
            text.push_str(&stderr);
        }

        Ok(json!({
            "content": [{"type": "text", "text": text}],
            "isError": !output.status.success()
        }))
    }

    async fn file_read_tool(&self, arguments: Value) -> Result<Value> {
        if !self.config.tools.file {
            return Err(anyhow::anyhow!("file tools are disabled"));
        }

        let path = arguments.get("path")
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'path' parameter"))?;

        let full_path = self.config.security.workspace_root.join(path);
        
        // Security: ensure path is within workspace
        if !full_path.starts_with(&self.config.security.workspace_root) {
            return Ok(json!({
                "content": [{"type": "text", "text": "Path outside workspace not allowed"}],
                "isError": true
            }));
        }

        match fs::read_to_string(&full_path).await {
            Ok(content) => {
                if content.len() > self.config.security.max_file_size {
                    Ok(json!({
                        "content": [{"type": "text", "text": format!("File too large ({}MB max)", self.config.security.max_file_size / 1024 / 1024)}],
                        "isError": true
                    }))
                } else {
                    Ok(json!({
                        "content": [{"type": "text", "text": content}],
                        "isError": false
                    }))
                }
            }
            Err(e) => Ok(json!({
                "content": [{"type": "text", "text": format!("Failed to read file: {}", e)}],
                "isError": true
            }))
        }
    }

    async fn file_write_tool(&self, arguments: Value) -> Result<Value> {
        if !self.config.tools.file {
            return Err(anyhow::anyhow!("file tools are disabled"));
        }

        let path = arguments.get("path")
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'path' parameter"))?;

        let content = arguments.get("content")
            .and_then(|c| c.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'content' parameter"))?;

        let full_path = self.config.security.workspace_root.join(path);
        
        // Security: ensure path is within workspace
        if !full_path.starts_with(&self.config.security.workspace_root) {
            return Ok(json!({
                "content": [{"type": "text", "text": "Path outside workspace not allowed"}],
                "isError": true
            }));
        }

        // Security: check file size
        if content.len() > self.config.security.max_file_size {
            return Ok(json!({
                "content": [{"type": "text", "text": format!("Content too large ({}MB max)", self.config.security.max_file_size / 1024 / 1024)}],
                "isError": true
            }));
        }

        // Create parent directories if needed
        if let Some(parent) = full_path.parent() {
            if let Err(e) = fs::create_dir_all(parent).await {
                return Ok(json!({
                    "content": [{"type": "text", "text": format!("Failed to create directories: {}", e)}],
                    "isError": true
                }));
            }
        }

        match fs::write(&full_path, content).await {
            Ok(_) => Ok(json!({
                "content": [{"type": "text", "text": format!("Wrote {} bytes to {}", content.len(), path)}],
                "isError": false
            })),
            Err(e) => Ok(json!({
                "content": [{"type": "text", "text": format!("Failed to write file: {}", e)}],
                "isError": true
            }))
        }
    }

    async fn file_list_tool(&self, arguments: Value) -> Result<Value> {
        if !self.config.tools.file {
            return Err(anyhow::anyhow!("file tools are disabled"));
        }

        let path = arguments.get("path")
            .and_then(|p| p.as_str())
            .unwrap_or(".");

        let full_path = self.config.security.workspace_root.join(path);
        
        // Security: ensure path is within workspace
        if !full_path.starts_with(&self.config.security.workspace_root) {
            return Ok(json!({
                "content": [{"type": "text", "text": "Path outside workspace not allowed"}],
                "isError": true
            }));
        }

        match fs::read_dir(&full_path).await {
            Ok(mut entries) => {
                let mut files = Vec::new();
                while let Some(entry) = entries.next_entry().await? {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let metadata = entry.metadata().await?;
                    let file_type = if metadata.is_dir() { "directory" } else { "file" };
                    let size = if metadata.is_file() { Some(metadata.len()) } else { None };
                    
                    files.push(json!({
                        "name": name,
                        "type": file_type,
                        "size": size
                    }));
                }

                files.sort_by(|a, b| {
                    let a_type = a["type"].as_str().unwrap_or("");
                    let b_type = b["type"].as_str().unwrap_or("");
                    let a_name = a["name"].as_str().unwrap_or("");
                    let b_name = b["name"].as_str().unwrap_or("");
                    
                    // Directories first, then files, both sorted alphabetically
                    match (a_type, b_type) {
                        ("directory", "file") => std::cmp::Ordering::Less,
                        ("file", "directory") => std::cmp::Ordering::Greater,
                        _ => a_name.cmp(b_name),
                    }
                });

                let listing = files.iter()
                    .map(|f| format!("{} ({})", f["name"].as_str().unwrap(), f["type"].as_str().unwrap()))
                    .collect::<Vec<_>>()
                    .join("\n");

                Ok(json!({
                    "content": [{"type": "text", "text": listing}],
                    "isError": false
                }))
            }
            Err(e) => Ok(json!({
                "content": [{"type": "text", "text": format!("Failed to list directory: {}", e)}],
                "isError": true
            }))
        }
    }

    async fn web_fetch_tool(&self, arguments: Value) -> Result<Value> {
        if !self.config.tools.web_fetch {
            return Err(anyhow::anyhow!("web_fetch tool is disabled"));
        }

        let url = arguments.get("url")
            .and_then(|u| u.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'url' parameter"))?;

        let method = arguments.get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("GET");

        let client = reqwest::Client::new();
        let mut request = match method.to_uppercase().as_str() {
            "GET" => client.get(url),
            "POST" => client.post(url),
            "PUT" => client.put(url),
            "DELETE" => client.delete(url),
            _ => return Ok(json!({
                "content": [{"type": "text", "text": format!("Unsupported HTTP method: {}", method)}],
                "isError": true
            }))
        };

        // Add headers if provided
        if let Some(headers) = arguments.get("headers").and_then(|h| h.as_object()) {
            for (key, value) in headers {
                if let Some(value_str) = value.as_str() {
                    request = request.header(key, value_str);
                }
            }
        }

        // Add body for POST/PUT
        if let Some(body) = arguments.get("body").and_then(|b| b.as_str()) {
            request = request.body(body.to_string());
        }

        match request.send().await {
            Ok(response) => {
                let status = response.status();
                let headers: HashMap<String, String> = response.headers()
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect();

                match response.text().await {
                    Ok(body) => {
                        let result = format!(
                            "HTTP {} {}\nHeaders: {}\n\n{}",
                            status.as_u16(),
                            status.canonical_reason().unwrap_or(""),
                            serde_json::to_string_pretty(&headers).unwrap_or_default(),
                            body
                        );

                        Ok(json!({
                            "content": [{"type": "text", "text": result}],
                            "isError": !status.is_success()
                        }))
                    }
                    Err(e) => Ok(json!({
                        "content": [{"type": "text", "text": format!("Failed to read response body: {}", e)}],
                        "isError": true
                    }))
                }
            }
            Err(e) => Ok(json!({
                "content": [{"type": "text", "text": format!("HTTP request failed: {}", e)}],
                "isError": true
            }))
        }
    }

    async fn extensions_list_tool(&self) -> Result<Value> {
        let status_map = self.extension_manager.get_status().await;
        let mut output = String::new();
        
        if status_map.is_empty() {
            output.push_str("No extensions configured.\n");
        } else {
            output.push_str("Extensions:\n");
            for (name, (status, error)) in status_map {
                let status_str = match status {
                    crate::extensions::ExtensionStatus::Stopped => "stopped",
                    crate::extensions::ExtensionStatus::Starting => "starting",
                    crate::extensions::ExtensionStatus::Running => "running",
                    crate::extensions::ExtensionStatus::Failed => "failed",
                    crate::extensions::ExtensionStatus::Restarting => "restarting",
                };
                
                if let Some(err) = error {
                    output.push_str(&format!("- {} ({}): {}\n", name, status_str, err));
                } else {
                    output.push_str(&format!("- {} ({})\n", name, status_str));
                }
            }
        }
        
        Ok(json!({
            "content": [{"type": "text", "text": output}],
            "isError": false
        }))
    }

    async fn extensions_start_tool(&self, arguments: Value) -> Result<Value> {
        let name = arguments.get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'name' parameter"))?;

        match self.extension_manager.start_extension(name).await {
            Ok(_) => {
                use std::sync::atomic::Ordering;
                self.needs_reconnect.store(true, Ordering::SeqCst);
                Ok(json!({
                    "content": [{"type": "text", "text": format!("Extension '{}' started successfully", name)}],
                    "isError": false
                }))
            }
            Err(e) => Ok(json!({
                "content": [{"type": "text", "text": format!("Failed to start extension '{}': {}", name, e)}],
                "isError": true
            }))
        }
    }

    async fn extensions_stop_tool(&self, arguments: Value) -> Result<Value> {
        let name = arguments.get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'name' parameter"))?;

        match self.extension_manager.stop_extension(name).await {
            Ok(_) => {
                use std::sync::atomic::Ordering;
                self.needs_reconnect.store(true, Ordering::SeqCst);
                Ok(json!({
                    "content": [{"type": "text", "text": format!("Extension '{}' stopped successfully", name)}],
                    "isError": false
                }))
            }
            Err(e) => Ok(json!({
                "content": [{"type": "text", "text": format!("Failed to stop extension '{}': {}", name, e)}],
                "isError": true
            }))
        }
    }

    async fn extensions_reload_tool(&self) -> Result<Value> {
        match self.reload_extensions().await {
            Ok(count) => {
                Ok(json!({
                    "content": [{"type": "text", "text": format!("Reloaded {} extensions successfully", count)}],
                    "isError": false
                }))
            }
            Err(e) => Ok(json!({
                "content": [{"type": "text", "text": format!("Failed to reload extensions: {}", e)}],
                "isError": true
            }))
        }
    }
}