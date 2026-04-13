//! Portal configuration — read from portal.toml
//!
//! Supports both flat and nested formats:
//!
//! Flat (recommended):
//! ```toml
//! name = "vale"
//! bind = "0.0.0.0:9100"
//! workspace = "/workspace"
//! ```
//!
//! Nested (also works):
//! ```toml
//! bind_host = "0.0.0.0"
//! bind_port = 9100
//! [security]
//! workspace_root = "/workspace"
//! ```

use serde::Deserialize;
use std::path::PathBuf;
use anyhow::Result;

/// Raw config as parsed from TOML (supports both flat and nested fields)
#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    name: Option<String>,

    /// Flat bind string: "host:port" or just "port"
    #[serde(default)]
    bind: Option<String>,

    /// Separate host (overridden by `bind` if present)
    #[serde(default)]
    bind_host: Option<String>,

    /// Separate port (overridden by `bind` if present)
    #[serde(default)]
    bind_port: Option<u16>,

    /// Flat workspace path (convenience alias for security.workspace_root)
    #[serde(default)]
    workspace: Option<PathBuf>,

    #[serde(default)]
    tools: Option<ToolsConfig>,

    #[serde(default)]
    security: Option<RawSecurityConfig>,

    #[serde(default)]
    cowork: Option<RawCoworkConfig>,

    /// MCP TCP pre-auth token (also settable via PORTAL_MCP_TOKEN env)
    #[serde(default)]
    portal_mcp_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawCoworkConfig {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    http_port: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct RawSecurityConfig {
    #[serde(default)]
    exec_allowlist: Option<Vec<String>>,
    #[serde(default)]
    workspace_root: Option<PathBuf>,
    #[serde(default)]
    max_file_size: Option<usize>,
}

/// Resolved portal configuration
#[derive(Debug, Clone)]
pub struct PortalConfig {
    pub name: String,
    pub bind_host: String,
    pub bind_port: u16,
    pub tools: ToolsConfig,
    pub security: SecurityConfig,
    pub cowork: CoworkConfig,
    /// When set, MCP TCP clients must send `auth` as the first JSON-RPC message.
    pub portal_mcp_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CoworkConfig {
    pub enabled: bool,
    pub http_port: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolsConfig {
    #[serde(default = "default_true")]
    pub exec: bool,
    #[serde(default = "default_true")]
    pub file: bool,
    #[serde(default = "default_true")]
    pub web_fetch: bool,
    /// Recursive workspace text search (portal_search).
    #[serde(default = "default_true")]
    pub search: bool,
    /// When false, workspace/tools/mcp.toml is ignored (custom MCP tools disabled).
    #[serde(default = "default_true")]
    pub custom_tools_enabled: bool,
}

#[derive(Debug, Clone)]
pub struct SecurityConfig {
    pub exec_allowlist: Vec<String>,
    pub workspace_root: PathBuf,
    pub max_file_size: usize,
}

impl Default for CoworkConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            http_port: 9101,
        }
    }
}

impl Default for PortalConfig {
    fn default() -> Self {
        Self {
            name: "portal".to_string(),
            bind_host: "0.0.0.0".to_string(),
            bind_port: 9100,
            tools: ToolsConfig::default(),
            security: SecurityConfig::default(),
            cowork: CoworkConfig::default(),
            portal_mcp_token: None,
        }
    }
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            exec: true,
            file: true,
            web_fetch: true,
            search: true,
            custom_tools_enabled: true,
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            exec_allowlist: vec![],
            workspace_root: PathBuf::from("/workspace"),
            max_file_size: 10 * 1024 * 1024,
        }
    }
}

impl PortalConfig {
    pub fn load(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let raw: RawConfig = toml::from_str(&content)?;

        // Resolve bind address: flat `bind` takes precedence
        let (host, port) = if let Some(bind) = &raw.bind {
            parse_bind(bind)?
        } else {
            (
                raw.bind_host.unwrap_or_else(|| "0.0.0.0".to_string()),
                raw.bind_port.unwrap_or(9100),
            )
        };

        // Resolve workspace: flat `workspace` > security.workspace_root > default
        let workspace = raw.workspace
            .or_else(|| raw.security.as_ref().and_then(|s| s.workspace_root.clone()))
            .unwrap_or_else(|| PathBuf::from("/workspace"));

        let security = SecurityConfig {
            exec_allowlist: raw.security.as_ref()
                .and_then(|s| s.exec_allowlist.clone())
                .unwrap_or_default(),
            workspace_root: workspace,
            max_file_size: raw.security.as_ref()
                .and_then(|s| s.max_file_size)
                .unwrap_or(10 * 1024 * 1024),
        };

        let cowork = CoworkConfig {
            enabled: raw.cowork.as_ref().and_then(|c| c.enabled).unwrap_or(true),
            http_port: raw.cowork.as_ref().and_then(|c| c.http_port).unwrap_or(port + 1),
        };

        Ok(PortalConfig {
            name: raw.name.unwrap_or_else(|| "portal".to_string()),
            bind_host: host,
            bind_port: port,
            tools: raw.tools.unwrap_or_default(),
            security,
            cowork,
            portal_mcp_token: raw.portal_mcp_token.clone().filter(|s| !s.is_empty()),
        })
    }
}

/// Parse "host:port" or just ":port" or "port"
fn parse_bind(bind: &str) -> Result<(String, u16)> {
    if let Some((host, port_str)) = bind.rsplit_once(':') {
        let port: u16 = port_str.parse()
            .map_err(|_| anyhow::anyhow!("Invalid port in bind '{}': '{}'", bind, port_str))?;
        let host = if host.is_empty() { "0.0.0.0".to_string() } else { host.to_string() };
        Ok((host, port))
    } else {
        // Just a port number
        let port: u16 = bind.parse()
            .map_err(|_| anyhow::anyhow!("Invalid bind address: '{}'", bind))?;
        Ok(("0.0.0.0".to_string(), port))
    }
}

fn default_true() -> bool { true }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bind_host_port() {
        let (h, p) = parse_bind("0.0.0.0:9100").unwrap();
        assert_eq!(h, "0.0.0.0");
        assert_eq!(p, 9100);
    }

    #[test]
    fn test_parse_bind_port_only() {
        let (h, p) = parse_bind("9100").unwrap();
        assert_eq!(h, "0.0.0.0");
        assert_eq!(p, 9100);
    }

    #[test]
    fn test_flat_config() {
        let toml = r#"
name = "vale"
bind = "0.0.0.0:9100"
workspace = "/workspace/vale"

[tools]
exec = true
file = true
web_fetch = false
"#;
        let raw: RawConfig = toml::from_str(toml).unwrap();
        let content = std::fs::write("/tmp/test-portal.toml", toml).unwrap();
        let config = PortalConfig::load("/tmp/test-portal.toml").unwrap();
        assert_eq!(config.name, "vale");
        assert_eq!(config.bind_host, "0.0.0.0");
        assert_eq!(config.bind_port, 9100);
        assert_eq!(config.security.workspace_root, PathBuf::from("/workspace/vale"));
        assert_eq!(config.tools.web_fetch, false);
    }
}
