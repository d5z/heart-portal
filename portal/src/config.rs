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

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

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

    /// Directory containing installed Portal kits.
    #[serde(default)]
    kits_dir: Option<String>,

    /// Enable kit discovery and tool proxying.
    #[serde(default)]
    kits_enabled: Option<bool>,
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
    pub kits_dir: Option<String>,
    pub kits_enabled: bool,
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
    pub screenshot: bool,
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
            kits_dir: Some(default_kits_dir()),
            kits_enabled: true,
        }
    }
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            exec: true,
            file: true,
            screenshot: true,
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
            workspace_root: default_workspace_root(),
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
        let workspace = raw
            .workspace
            .or_else(|| raw.security.as_ref().and_then(|s| s.workspace_root.clone()))
            .unwrap_or_else(default_workspace_root);
        anyhow::ensure!(
            !workspace.as_os_str().is_empty(),
            "workspace must not be empty"
        );
        // Configuration-relative paths are stable under a service manager or
        // when --config is invoked from another directory.
        let workspace = if workspace.is_absolute() {
            workspace
        } else {
            let config_path = std::path::absolute(path)?;
            config_path
                .parent()
                .context("config has no parent directory")?
                .join(workspace)
        };

        let security = SecurityConfig {
            exec_allowlist: raw
                .security
                .as_ref()
                .and_then(|s| s.exec_allowlist.clone())
                .unwrap_or_default(),
            workspace_root: workspace,
            max_file_size: raw
                .security
                .as_ref()
                .and_then(|s| s.max_file_size)
                .unwrap_or(10 * 1024 * 1024),
        };

        let cowork = CoworkConfig {
            enabled: raw.cowork.as_ref().and_then(|c| c.enabled).unwrap_or(true),
            http_port: raw
                .cowork
                .as_ref()
                .and_then(|c| c.http_port)
                .unwrap_or(port + 1),
        };

        let name = raw.name.unwrap_or_else(|| "portal".to_string());

        Ok(PortalConfig {
            name,
            bind_host: host,
            bind_port: port,
            tools: raw.tools.unwrap_or_default(),
            security,
            cowork,
            portal_mcp_token: raw.portal_mcp_token.clone().filter(|s| !s.is_empty()),
            kits_dir: raw
                .kits_dir
                .filter(|s| !s.trim().is_empty())
                .or_else(|| Some(default_kits_dir())),
            kits_enabled: raw.kits_enabled.unwrap_or(true),
        })
    }

    /// Initialize only the configured root, before exposing any tools. Tool
    /// requests must never create a different root in response to a denial.
    pub fn prepare_workspace(&mut self) -> Result<()> {
        let root = &self.security.workspace_root;
        anyhow::ensure!(!root.as_os_str().is_empty(), "workspace must not be empty");
        std::fs::create_dir_all(root).with_context(|| format!(
            "Cannot initialize configured workspace '{}'; check portal.toml and directory permissions",
            root.display()
        ))?;
        let canonical = root
            .canonicalize()
            .with_context(|| format!("Cannot resolve configured workspace '{}'", root.display()))?;
        anyhow::ensure!(
            canonical.is_dir(),
            "Configured workspace must be a directory"
        );
        self.security.workspace_root = workspace_path_from_canonical(canonical);
        Ok(())
    }
}

fn default_workspace_root() -> PathBuf {
    #[cfg(windows)]
    {
        // A per-user default, never a drive-root /workspace directory.
        return std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .map(|profile| profile.join(".heart-portal/workspace"))
            .unwrap_or_else(|| PathBuf::from("./workspace"));
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/workspace")
    }
}

fn workspace_path_from_canonical(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        // canonicalize returns a verbatim Windows path. Store a normal absolute
        // spelling so C:\\... requests compare with the root before canonical checks.
        // Preserve UNC paths and non-Unicode UTF-16 rather than using lossy text.
        let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        let prefix: Vec<u16> = "\\\\?\\".encode_utf16().collect();
        let unc: Vec<u16> = "\\\\?\\UNC\\".encode_utf16().collect();
        if wide.starts_with(&unc) {
            let mut normal: Vec<u16> = "\\\\".encode_utf16().collect();
            normal.extend_from_slice(&wide[unc.len()..]);
            return PathBuf::from(std::ffi::OsString::from_wide(&normal));
        }
        if wide.starts_with(&prefix) && wide.get(prefix.len() + 1) == Some(&(b':' as u16)) {
            return PathBuf::from(std::ffi::OsString::from_wide(&wide[prefix.len()..]));
        }
    }
    path
}

/// Parse "host:port" or just ":port" or "port"
fn parse_bind(bind: &str) -> Result<(String, u16)> {
    if let Some((host, port_str)) = bind.rsplit_once(':') {
        let port: u16 = port_str
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid port in bind '{}': '{}'", bind, port_str))?;
        let host = if host.is_empty() {
            "0.0.0.0".to_string()
        } else {
            host.to_string()
        };
        Ok((host, port))
    } else {
        // Just a port number
        let port: u16 = bind
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid bind address: '{}'", bind))?;
        Ok(("0.0.0.0".to_string(), port))
    }
}

fn default_true() -> bool {
    true
}

fn default_kits_dir() -> String {
    "~/.heart-portal/kits/".to_string()
}

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
        let workspace = std::env::temp_dir().join("portal-config-workspace");
        let toml = toml.replace(
            "\"/workspace/vale\"",
            &serde_json::to_string(&workspace).unwrap(),
        );
        let path =
            std::env::temp_dir().join(format!("heart-portal-flat-{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(&path, toml).unwrap();
        let config = PortalConfig::load(path.to_str().unwrap()).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(config.name, "vale");
        assert_eq!(config.bind_host, "0.0.0.0");
        assert_eq!(config.bind_port, 9100);
        assert_eq!(config.security.workspace_root, workspace);
        assert_eq!(config.tools.web_fetch, false);
        assert_eq!(config.kits_dir.as_deref(), Some("~/.heart-portal/kits/"));
        assert!(config.kits_enabled);
    }

    #[test]
    fn test_kits_config() {
        let toml = r#"
name = "vale"
kits_dir = "/tmp/portal-kits"
kits_enabled = false
"#;
        let path =
            std::env::temp_dir().join(format!("heart-portal-kits-{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(&path, toml).unwrap();
        let config = PortalConfig::load(path.to_str().unwrap()).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(config.kits_dir.as_deref(), Some("/tmp/portal-kits"));
        assert!(!config.kits_enabled);
    }

    #[tokio::test]
    async fn relative_workspace_initializes_file_tools_without_expanding_boundary() {
        let temp = std::env::temp_dir().join(format!("portal-workspace-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp).unwrap();
        let path = temp.join("portal.toml");
        std::fs::write(&path, "workspace = './workspace'\n").unwrap();
        let mut config = PortalConfig::load(path.to_str().unwrap()).unwrap();
        assert!(
            !config.security.workspace_root.exists(),
            "loading config must not create directories"
        );
        config.prepare_workspace().unwrap();
        assert!(config.security.workspace_root.is_absolute());
        config.kits_enabled = false;
        let host = crate::tools::ToolHost::new(&config);
        let absolute = config.security.workspace_root.join("中文.txt");
        host.call(
            "portal_file_write",
            serde_json::json!({"path": absolute, "content": "中文正常"}),
        )
        .await
        .unwrap();
        let read = host
            .call("portal_file_read", serde_json::json!({"path": "中文.txt"}))
            .await
            .unwrap();
        assert!(read["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("中文正常"));
        let outside = temp.join("outside.txt");
        let error = host
            .call(
                "portal_file_write",
                serde_json::json!({"path": outside, "content": "denied"}),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Path outside workspace"));
        assert!(!outside.exists());
        assert!(host
            .call(
                "portal_file_write",
                serde_json::json!({"path": "../outside.txt", "content": "denied"})
            )
            .await
            .is_err());
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn workspace_misconfiguration_fails_instead_of_falling_back() {
        let temp =
            std::env::temp_dir().join(format!("portal-bad-workspace-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp).unwrap();
        let path = temp.join("portal.toml");
        std::fs::write(&path, "workspace = ''\n").unwrap();
        assert!(PortalConfig::load(path.to_str().unwrap()).is_err());
        let file = temp.join("is-a-file");
        std::fs::write(&file, "existing content").unwrap();
        let mut config = PortalConfig::default();
        config.security.workspace_root = file.clone();
        assert!(config.prepare_workspace().is_err());
        assert_eq!(std::fs::read_to_string(file).unwrap(), "existing content");
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_default_and_canonical_paths_are_usable() {
        let profile = PathBuf::from(std::env::var_os("USERPROFILE").unwrap());
        assert_eq!(
            default_workspace_root(),
            profile.join(".heart-portal/workspace")
        );
        assert_eq!(
            workspace_path_from_canonical(PathBuf::from(r"\\?\C:\Users\中文\workspace")),
            PathBuf::from(r"C:\Users\中文\workspace")
        );
        assert_eq!(
            workspace_path_from_canonical(PathBuf::from(r"\\?\UNC\server\share\workspace")),
            PathBuf::from(r"\\server\share\workspace")
        );
    }
}
