//! Custom tools — simplified implementation without MCP dependencies.
//! This is a placeholder for custom tool functionality.

use std::path::Path;
use anyhow::Result;
use serde_json::Value;
use tracing::info;

use super::ToolInfo;

/// Simplified custom tool host without MCP dependencies.
#[derive(Clone)]
pub struct CustomToolHost {
    // Placeholder - no actual functionality in standalone mode
}

impl CustomToolHost {
    pub fn new() -> Self {
        Self {}
    }

    /// Shutdown - no-op in standalone mode
    pub async fn shutdown(&self) {
        info!("Custom tools shutdown (no-op in standalone mode)");
    }

    /// Load custom tools - simplified implementation without MCP
    pub async fn load(&self, _workspace_root: &Path) -> Result<usize> {
        // In standalone mode, no custom tools are loaded
        info!("Custom tools loading disabled in standalone mode");
        Ok(0)
    }

    /// List all custom tools - always empty in standalone mode
    pub async fn list_tools(&self) -> Vec<ToolInfo> {
        Vec::new()
    }

    /// Check if a tool name belongs to custom tools - always false in standalone mode
    pub async fn has_tool(&self, _name: &str) -> bool {
        false
    }

    /// Call a custom tool - always returns error in standalone mode
    pub async fn call(&self, tool_name: &str, _arguments: Value) -> Result<Value> {
        anyhow::bail!("Custom tool '{}' not available in standalone mode", tool_name)
    }
}

// No custom config parsing needed in standalone mode
