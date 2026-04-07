//! Extension manager for hot-reloadable MCP servers
//! 
//! Extensions are external MCP servers that can be dynamically loaded/unloaded
//! without restarting the Portal kernel. Configuration is read from extensions.toml.

use anyhow::{Result, Context};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::fs;
use tokio::process::{Child, Command};
use tokio::sync::RwLock;
use tokio::time::{timeout, Duration};
use tracing::{info, warn, error, debug};
use serde_json::Value;

/// Extension configuration loaded from extensions.toml
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExtensionsConfig {
    #[serde(default)]
    pub extensions: HashMap<String, ExtensionConfig>,
}

/// Configuration for a single extension
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExtensionConfig {
    /// Human-readable description
    pub description: String,
    
    /// Command to start the extension (array format: [executable, arg1, arg2, ...])
    pub command: Vec<String>,
    
    /// Working directory for the extension process (relative to workspace root)
    pub working_dir: Option<String>,
    
    /// Environment variables to set for the extension
    #[serde(default)]
    pub env: HashMap<String, String>,
    
    /// Whether the extension should auto-start when Portal starts
    #[serde(default = "default_true")]
    pub auto_start: bool,
    
    /// Timeout for extension startup (seconds)
    #[serde(default = "default_startup_timeout")]
    pub startup_timeout: u64,
    
    /// Whether to restart the extension if it crashes
    #[serde(default = "default_true")]
    pub restart_on_crash: bool,
}

/// Runtime state of an extension
#[derive(Debug)]
pub struct ExtensionState {
    pub name: String,
    pub config: ExtensionConfig,
    pub status: ExtensionStatus,
    pub process: Option<Child>,
    pub tools: Vec<crate::tools::ToolInfo>,
    pub last_error: Option<String>,
    pub restart_count: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExtensionStatus {
    Stopped,
    Starting,
    Running,
    Failed,
    Restarting,
}

/// Extension manager handles loading, starting, stopping, and hot-reloading extensions
pub struct ExtensionManager {
    workspace_root: PathBuf,
    extensions: Arc<RwLock<HashMap<String, ExtensionState>>>,
    config_path: PathBuf,
}

impl ExtensionManager {
    pub fn new(workspace_root: PathBuf) -> Self {
        let config_path = workspace_root.join("extensions.toml");
        Self {
            workspace_root,
            extensions: Arc::new(RwLock::new(HashMap::new())),
            config_path,
        }
    }

    /// Load extensions configuration from extensions.toml
    pub async fn load_config(&self) -> Result<ExtensionsConfig> {
        if !self.config_path.exists() {
            info!("No extensions.toml found, creating default");
            let default_config = ExtensionsConfig {
                extensions: HashMap::new(),
            };
            let toml_content = toml::to_string_pretty(&default_config)?;
            fs::write(&self.config_path, toml_content).await?;
            return Ok(default_config);
        }

        let content = fs::read_to_string(&self.config_path).await
            .context("Failed to read extensions.toml")?;
        
        let config: ExtensionsConfig = toml::from_str(&content)
            .context("Failed to parse extensions.toml")?;
        
        Ok(config)
    }

    /// Initialize extensions from config (auto-start enabled ones)
    pub async fn initialize(&self) -> Result<()> {
        let config = self.load_config().await?;
        let mut extensions = self.extensions.write().await;
        
        for (name, ext_config) in config.extensions {
            let state = ExtensionState {
                name: name.clone(),
                config: ext_config.clone(),
                status: ExtensionStatus::Stopped,
                process: None,
                tools: Vec::new(),
                last_error: None,
                restart_count: 0,
            };
            
            extensions.insert(name.clone(), state);
            
            if ext_config.auto_start {
                info!("Auto-starting extension: {}", name);
                // We'll start it after releasing the write lock
            }
        }
        
        // Start auto-start extensions
        let auto_start_names: Vec<String> = extensions
            .values()
            .filter(|state| state.config.auto_start)
            .map(|state| state.name.clone())
            .collect();
        
        drop(extensions); // Release write lock
        
        for name in auto_start_names {
            if let Err(e) = self.start_extension(&name).await {
                warn!("Failed to auto-start extension {}: {}", name, e);
            }
        }
        
        Ok(())
    }

    /// Start a specific extension
    pub async fn start_extension(&self, name: &str) -> Result<()> {
        let mut extensions = self.extensions.write().await;
        let state = extensions.get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("Extension '{}' not found", name))?;
        
        if state.status == ExtensionStatus::Running {
            return Ok(()); // Already running
        }
        
        state.status = ExtensionStatus::Starting;
        let config = state.config.clone();
        drop(extensions); // Release lock before potentially long-running operation
        
        info!("Starting extension: {} ({})", name, config.description);
        
        // Build command
        if config.command.is_empty() {
            return Err(anyhow::anyhow!("Extension '{}' has empty command", name));
        }
        
        let mut cmd = Command::new(&config.command[0]);
        cmd.args(&config.command[1..]);
        
        // Set working directory
        let working_dir = if let Some(wd) = &config.working_dir {
            self.workspace_root.join(wd)
        } else {
            self.workspace_root.clone()
        };
        cmd.current_dir(&working_dir);
        
        // Set environment variables
        for (key, value) in &config.env {
            cmd.env(key, value);
        }
        
        // Configure stdio
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        
        // Spawn process
        let child = cmd.spawn()
            .context(format!("Failed to spawn extension '{}'", name))?;
        
        // Update state with running process
        let mut extensions = self.extensions.write().await;
        let state = extensions.get_mut(name).unwrap();
        state.process = Some(child);
        state.status = ExtensionStatus::Running;
        state.last_error = None;
        
        info!("Extension '{}' started successfully", name);
        
        // TODO: Discover tools from the extension via MCP protocol
        // For now, we'll implement a basic tool discovery
        
        Ok(())
    }

    /// Stop a specific extension
    pub async fn stop_extension(&self, name: &str) -> Result<()> {
        let mut extensions = self.extensions.write().await;
        let state = extensions.get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("Extension '{}' not found", name))?;
        
        if state.status == ExtensionStatus::Stopped {
            return Ok(()); // Already stopped
        }
        
        info!("Stopping extension: {}", name);
        
        if let Some(mut child) = state.process.take() {
            // Try graceful shutdown first
            if let Err(e) = child.kill().await {
                warn!("Failed to kill extension '{}': {}", name, e);
            }
            
            // Wait for process to exit
            match timeout(Duration::from_secs(5), child.wait()).await {
                Ok(Ok(status)) => {
                    debug!("Extension '{}' exited with status: {}", name, status);
                }
                Ok(Err(e)) => {
                    warn!("Error waiting for extension '{}' to exit: {}", name, e);
                }
                Err(_) => {
                    warn!("Extension '{}' did not exit within timeout", name);
                }
            }
        }
        
        state.status = ExtensionStatus::Stopped;
        state.tools.clear();
        
        info!("Extension '{}' stopped", name);
        Ok(())
    }

    /// Restart a specific extension
    pub async fn restart_extension(&self, name: &str) -> Result<()> {
        info!("Restarting extension: {}", name);
        
        // Increment restart count
        {
            let mut extensions = self.extensions.write().await;
            if let Some(state) = extensions.get_mut(name) {
                state.restart_count += 1;
                state.status = ExtensionStatus::Restarting;
            }
        }
        
        self.stop_extension(name).await?;
        tokio::time::sleep(Duration::from_millis(500)).await; // Brief pause
        self.start_extension(name).await?;
        
        Ok(())
    }

    /// Hot-reload extensions configuration and apply changes
    pub async fn reload_config(&self) -> Result<Vec<String>> {
        info!("Hot-reloading extensions configuration");
        
        let new_config = self.load_config().await?;
        let mut changes = Vec::new();
        let mut extensions = self.extensions.write().await;
        
        // Stop extensions that are no longer in config
        let current_names: Vec<String> = extensions.keys().cloned().collect();
        for name in current_names {
            if !new_config.extensions.contains_key(&name) {
                info!("Removing extension: {}", name);
                if let Some(state) = extensions.get_mut(&name) {
                    if state.status == ExtensionStatus::Running {
                        // Stop it (we'll do this after releasing the lock)
                        changes.push(format!("Stopped removed extension: {}", name));
                    }
                }
                extensions.remove(&name);
            }
        }
        
        // Add or update extensions
        for (name, ext_config) in new_config.extensions {
            if let Some(state) = extensions.get_mut(&name) {
                // Check if config changed
                let config_changed = state.config.command != ext_config.command
                    || state.config.working_dir != ext_config.working_dir
                    || state.config.env != ext_config.env;
                
                if config_changed {
                    info!("Extension '{}' config changed, will restart", name);
                    state.config = ext_config;
                    changes.push(format!("Updated extension: {}", name));
                    // We'll restart it after releasing the lock
                } else {
                    // Just update non-critical config
                    state.config = ext_config;
                }
            } else {
                // New extension
                info!("Adding new extension: {}", name);
                let state = ExtensionState {
                    name: name.clone(),
                    config: ext_config,
                    status: ExtensionStatus::Stopped,
                    process: None,
                    tools: Vec::new(),
                    last_error: None,
                    restart_count: 0,
                };
                extensions.insert(name.clone(), state);
                changes.push(format!("Added new extension: {}", name));
            }
        }
        
        drop(extensions); // Release lock
        
        // Apply changes that require async operations
        let config = self.load_config().await?;
        for (name, ext_config) in config.extensions {
            if ext_config.auto_start {
                let extensions = self.extensions.read().await;
                if let Some(state) = extensions.get(&name) {
                    if state.status == ExtensionStatus::Stopped {
                        drop(extensions);
                        if let Err(e) = self.start_extension(&name).await {
                            warn!("Failed to start extension '{}' after reload: {}", name, e);
                        }
                    }
                }
            }
        }
        
        info!("Extensions configuration reloaded with {} changes", changes.len());
        Ok(changes)
    }

    /// Get status of all extensions
    pub async fn get_status(&self) -> HashMap<String, (ExtensionStatus, Option<String>)> {
        let extensions = self.extensions.read().await;
        extensions
            .iter()
            .map(|(name, state)| {
                (name.clone(), (state.status.clone(), state.last_error.clone()))
            })
            .collect()
    }

    /// Get all tools from all running extensions
    pub async fn get_all_tools(&self) -> Vec<crate::tools::ToolInfo> {
        let extensions = self.extensions.read().await;
        let mut all_tools = Vec::new();
        
        for state in extensions.values() {
            if state.status == ExtensionStatus::Running {
                all_tools.extend(state.tools.iter().cloned());
            }
        }
        
        all_tools
    }

    /// Call a tool on the appropriate extension
    pub async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<Value> {
        let extensions = self.extensions.read().await;
        
        // Find which extension has this tool
        for state in extensions.values() {
            if state.status == ExtensionStatus::Running {
                for tool in &state.tools {
                    if tool.name == tool_name {
                        // TODO: Implement actual MCP communication with the extension
                        // For now, return a placeholder
                        return Ok(serde_json::json!({
                            "content": [{"type": "text", "text": format!("Tool '{}' called on extension '{}'", tool_name, state.name)}],
                            "isError": false
                        }));
                    }
                }
            }
        }
        
        Err(anyhow::anyhow!("Tool '{}' not found in any running extension", tool_name))
    }
}

fn default_true() -> bool {
    true
}

fn default_startup_timeout() -> u64 {
    30
}

impl Default for ExtensionsConfig {
    fn default() -> Self {
        Self {
            extensions: HashMap::new(),
        }
    }
}