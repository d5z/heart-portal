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

#[path = "config_diagnostics.rs"]
mod diagnostics;
pub use diagnostics::ConfigWarning;

/// Raw config as parsed from TOML (supports both flat and nested fields)
#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    name: Option<String>,
    /// Persistent Loom link; CLI/environment still take precedence.
    connect: Option<String>,

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

    /// MCP TCP pre-auth token (also settable via PORTAL_MCP_TOKEN env)
    #[serde(default)]
    portal_mcp_token: Option<String>,

    /// Directory containing installed Portal kits.
    #[serde(default)]
    kits_dir: Option<String>,

    /// Enable kit discovery and tool proxying.
    #[serde(default)]
    kits_enabled: Option<bool>,

    /// Native sub-agent runtime (`[subagent]`).
    #[serde(default)]
    subagent: Option<SubagentConfig>,

    #[serde(flatten)]
    ignored_fields: std::collections::BTreeMap<String, serde::de::IgnoredAny>,
}

#[derive(Debug, Deserialize)]
struct RawSecurityConfig {
    #[serde(default)]
    expose_host_details: bool,
    #[serde(default)]
    exec_allowlist: Option<Vec<String>>,
    #[serde(default)]
    workspace_root: Option<PathBuf>,
    #[serde(default)]
    max_file_size: Option<usize>,
    #[serde(flatten)]
    ignored_fields: std::collections::BTreeMap<String, serde::de::IgnoredAny>,
}

/// Resolved portal configuration
#[derive(Clone)]
pub struct PortalConfig {
    pub name: String,
    pub connect_link: Option<String>,
    pub bind_host: String,
    pub bind_port: u16,
    pub tools: ToolsConfig,
    pub security: SecurityConfig,
    /// When set, MCP TCP clients must send `auth` as the first JSON-RPC message.
    pub portal_mcp_token: Option<String>,
    pub kits_dir: Option<String>,
    pub kits_enabled: bool,
    /// Startup diagnostics contain field names and fixed guidance, never values.
    pub warnings: Vec<ConfigWarning>,
    pub subagent: SubagentConfig,
    /// The file this configuration was loaded from; `None` for in-memory
    /// defaults. Runtime self-service tools write back to this same file.
    pub config_path: Option<PathBuf>,
}

/// Native sub-agent runtime (PRD §4.7).
///
/// ```toml
/// [subagent]
/// enabled = true
/// command = ["~/.heart-portal/pi/bin/pi"]   # omit to search PATH
/// auto_install = true                        # npm-install a pinned pi when none is found
/// state_dir = "~/.heart-portal/subagent"
/// max_concurrent = 3
/// idle_unload_secs = 1800
/// eager = false
///
/// [subagent.budget]
/// max_turns = 40
/// max_tokens = 400000
/// timeout_secs = 1800
///
/// [subagent.model]
/// provider = "anthropic"
/// model = "claude-sonnet-4-5"
/// thinking = "medium"
/// ```
///
/// `enabled` defaults to true, but the `portal_subagent_*` tools are only
/// advertised once the pi binary actually resolves — a being is never offered
/// a tool that cannot work.
#[derive(Debug, Clone, Deserialize)]
pub struct SubagentConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// argv to launch pi. Resolution order when unset:
    /// `~/.heart-portal/pi/bin/pi`, then `pi`, then `prime-agent` on PATH.
    #[serde(default)]
    pub command: Option<Vec<String>>,

    /// When `command` is unset and no pi resolves, install Portal's own pinned
    /// copy under `~/.heart-portal/pi` with npm (best effort; no npm ⇒ the
    /// sub-agent simply stays unavailable). Set false to never run npm.
    #[serde(default = "default_true")]
    pub auto_install: bool,

    /// Ledger, daemon socket, pi agent dir and pi sessions.
    /// Default `~/.heart-portal/subagent`. Sensitive: holds auth + transcripts.
    #[serde(default)]
    pub state_dir: Option<String>,

    /// Start the daemon at Portal startup instead of on first use.
    #[serde(default)]
    pub eager: bool,

    /// Maximum tasks running at once across all sessions.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,

    /// Unload an idle session from daemon memory after this long (0 = never).
    /// The session *file* is kept; the next spawn resumes it.
    #[serde(default = "default_idle_unload_secs")]
    pub idle_unload_secs: u64,

    /// Stop the daemon after this long with no sessions (0 = keep it while
    /// Portal runs).
    #[serde(default)]
    pub daemon_idle_exit_secs: u64,

    #[serde(default)]
    pub budget: BudgetConfig,

    #[serde(default)]
    pub model: SubagentModelConfig,

    /// Skills injected into every session.
    #[serde(default)]
    pub skills: Vec<String>,

    /// Pi extensions loaded into every session.
    #[serde(default)]
    pub extensions: Vec<String>,

    /// Variables forwarded into the otherwise-cleared child environment.
    /// Windows also preserves its runtime and installation directory variables.
    #[serde(default = "default_env_passthrough")]
    pub env_passthrough: Vec<String>,

    /// Extra contract text appended to the built-in sub-agent contract.
    #[serde(default)]
    pub append_system_prompt: Option<String>,
}

/// Per-task caps, mapped onto pi's own `autonomous` budget primitives.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct BudgetConfig {
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u64,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_continuations")]
    pub max_continuations: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SubagentModelConfig {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// off | minimal | low | medium | high | xhigh | max
    #[serde(default)]
    pub thinking: Option<String>,
    /// Provider key for the per-task stdio fallback, where there is no
    /// daemon to have logged in. Never logged and never surfaced by
    /// `portal_status`, but it does reach pi as `--api-key`, so it is visible
    /// in `ps` on this machine: prefer `env_passthrough` of the provider's
    /// own variable (`OPENROUTER_API_KEY`, …) when that is an option.
    #[serde(default)]
    pub api_key: Option<String>,
}

/// Providers whose API key pi (and Portal's daemon env injection) knows how
/// to route. `google` is pi's alias for `gemini`.
pub const SUBAGENT_PROVIDERS: [&str; 7] = [
    "anthropic",
    "openrouter",
    "openai",
    "gemini",
    "google",
    "groq",
    "xai",
];

/// Thinking levels pi accepts.
pub const SUBAGENT_THINKING_LEVELS: [&str; 7] =
    ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

impl SubagentModelConfig {
    /// Any of provider / model / key set: the being has said *something*
    /// about which LLM the sub-agent runs on.
    pub fn is_configured(&self) -> bool {
        self.provider.is_some() || self.model.is_some() || self.api_key.is_some()
    }

    /// `sk-ant-...***` — enough to recognise which key it is, never enough
    /// to use it. Short keys are fully masked.
    pub fn masked_api_key(&self) -> Option<String> {
        self.api_key.as_deref().map(mask_secret)
    }

    /// Status payload: the key is only ever present in masked form.
    pub fn to_status_json(&self) -> serde_json::Value {
        serde_json::json!({
            "configured": self.is_configured(),
            "provider": self.provider,
            "model": self.model,
            "thinking": self.thinking,
            "api_key": self.masked_api_key(),
        })
    }
}

/// Show a recognisable prefix of a secret and hide the rest.
pub fn mask_secret(secret: &str) -> String {
    const VISIBLE: usize = 7;
    const MIN_LEN_FOR_PREFIX: usize = 16;
    if secret.chars().count() < MIN_LEN_FOR_PREFIX {
        return "***".to_string();
    }
    let prefix: String = secret.chars().take(VISIBLE).collect();
    format!("{prefix}...***")
}

/// Write `[subagent.model]` into `path`, leaving every other line, comment
/// and blank exactly as it was. A missing file is created with just that
/// section. Keys that are `None` are removed from the section.
///
/// The file is replaced atomically (temp file + rename) and, on unix, made
/// private (0600) because it may now hold a provider key.
pub fn write_subagent_model(path: &std::path::Path, model: &SubagentModelConfig) -> Result<()> {
    use toml_edit::{DocumentMut, Entry, Item, Table};

    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(e).with_context(|| format!("reading {}", path.display()));
        }
    };
    let mut doc: DocumentMut = existing
        .trim_start_matches('\u{feff}')
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid TOML configuration in {}", path.display()))?;

    let subagent = match doc.as_table_mut().entry("subagent") {
        Entry::Vacant(slot) => {
            // Implicit: emit `[subagent.model]` without an empty `[subagent]`.
            let mut table = Table::new();
            table.set_implicit(true);
            slot.insert(Item::Table(table))
        }
        Entry::Occupied(slot) => slot.into_mut(),
    };
    let subagent = subagent
        .as_table_like_mut()
        .with_context(|| format!("[subagent] in {} is not a table", path.display()))?;
    let model_item = match subagent.entry("model") {
        Entry::Vacant(slot) => slot.insert(toml_edit::table()),
        Entry::Occupied(slot) => slot.into_mut(),
    };
    let model_table = model_item
        .as_table_like_mut()
        .with_context(|| format!("[subagent.model] in {} is not a table", path.display()))?;

    for (key, value) in [
        ("provider", &model.provider),
        ("model", &model.model),
        ("thinking", &model.thinking),
        ("api_key", &model.api_key),
    ] {
        match value {
            Some(v) => {
                model_table.insert(key, toml_edit::value(v.as_str()));
            }
            None => {
                model_table.remove(key);
            }
        }
    }

    write_private_atomic(path, doc.to_string().as_bytes())
}

/// Temp file in the same directory, then rename over `path`.
fn write_private_atomic(path: &std::path::Path, content: &[u8]) -> Result<()> {
    use std::io::Write;

    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => std::env::current_dir()?,
    };
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("creating {}", parent.display()))?;
    let stage = parent.join(format!(".portal-config-{}.tmp", uuid::Uuid::new_v4()));

    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&stage)?;
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&stage, path)
            .with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&stage);
    }
    result
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            command: None,
            auto_install: true,
            state_dir: None,
            eager: false,
            max_concurrent: default_max_concurrent(),
            idle_unload_secs: default_idle_unload_secs(),
            daemon_idle_exit_secs: 0,
            budget: BudgetConfig::default(),
            model: SubagentModelConfig::default(),
            skills: Vec::new(),
            extensions: Vec::new(),
            env_passthrough: default_env_passthrough(),
            append_system_prompt: None,
        }
    }
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            max_turns: default_max_turns(),
            max_tokens: default_max_tokens(),
            timeout_secs: default_timeout_secs(),
            max_continuations: default_max_continuations(),
        }
    }
}

impl SubagentConfig {
    /// `state_dir` with `~` expanded, or the default under Portal's home.
    pub fn resolved_state_dir(&self) -> PathBuf {
        let raw = self
            .state_dir
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("~/.heart-portal/subagent");
        crate::subagent::pi_daemon::expand_home(raw)
    }
}

impl std::fmt::Debug for PortalConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortalConfig")
            .field("name", &self.name)
            .field("connect_link", &"<redacted>")
            .field("portal_mcp_token", &"<redacted>")
            .finish_non_exhaustive()
    }
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
    #[serde(flatten)]
    ignored_fields: std::collections::BTreeMap<String, serde::de::IgnoredAny>,
}

#[derive(Debug, Clone)]
pub struct SecurityConfig {
    /// Local administrator opt-in for host paths/PID in portal_status.
    pub expose_host_details: bool,
    pub exec_allowlist: Vec<String>,
    pub workspace_root: PathBuf,
    pub max_file_size: usize,
}

impl Default for PortalConfig {
    fn default() -> Self {
        Self {
            name: "portal".to_string(),
            connect_link: None,
            bind_host: "0.0.0.0".to_string(),
            bind_port: 9100,
            tools: ToolsConfig::default(),
            security: SecurityConfig::default(),
            portal_mcp_token: None,
            kits_dir: Some(default_kits_dir()),
            kits_enabled: true,
            warnings: Vec::new(),
            subagent: SubagentConfig::default(),
            config_path: None,
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
            ignored_fields: Default::default(),
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            expose_host_details: false,
            exec_allowlist: vec![],
            workspace_root: default_workspace_root(),
            max_file_size: 10 * 1024 * 1024,
        }
    }
}

impl PortalConfig {
    pub fn load(path: &str) -> Result<Self> {
        let content = crate::bounded_file::text(std::path::Path::new(path), crate::bounded_file::CONFIG_LIMIT)?;
        let raw: RawConfig = toml::from_str(content.trim_start_matches('\u{feff}'))
            .map_err(|_| anyhow::anyhow!("Invalid TOML configuration in {}", path))?;
        let mut warnings = diagnostics::collect(&raw);

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
        let workspace = crate::paths::resolve_relative(&workspace, std::path::Path::new(path))?;

        let security = SecurityConfig {
            expose_host_details: raw.security.as_ref().is_some_and(|s| s.expose_host_details),
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

        let name = raw.name.unwrap_or_else(|| "portal".to_string());
        let connect_link = raw.connect.filter(|s| !s.trim().is_empty());
        if let Some(link) = &connect_link {
            crate::relay_client::parse_loom_link(link)
                .context("Invalid configured Portal connection")?;
        }
        let kits_dir = raw
            .kits_dir
            .filter(|s| !s.trim().is_empty())
            .map(|kits| {
                let (path, legacy) = crate::paths::resolve_kits_compatible(
                    std::path::Path::new(&kits),
                    std::path::Path::new(path),
                    &std::env::current_dir()?,
                )?;
                if legacy {
                    warnings.push(ConfigWarning { code: "legacy-kits-directory", field: "kits_dir".into(),
                        message: "Preserved an existing kit directory relative to the launch working directory. Set kits_dir to the effective absolute path shown by portal_status before relocating configuration." });
                }
                Ok::<_, anyhow::Error>(path.to_string_lossy().into_owned())
            })
            .transpose()?
            .or_else(|| Some(default_kits_dir()));

        Ok(PortalConfig {
            name,
            connect_link,
            bind_host: host,
            bind_port: port,
            tools: raw.tools.unwrap_or_default(),
            security,
            portal_mcp_token: raw.portal_mcp_token.clone().filter(|s| !s.is_empty()),
            kits_dir,
            kits_enabled: raw.kits_enabled.unwrap_or(true),
            warnings,
            subagent: raw.subagent.unwrap_or_default(),
            config_path: Some(PathBuf::from(path)),
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

fn default_max_concurrent() -> usize { 3 }
fn default_idle_unload_secs() -> u64 { 1800 }
fn default_max_turns() -> u32 { 40 }
fn default_max_tokens() -> u64 { 400_000 }
fn default_timeout_secs() -> u64 { 1800 }
fn default_max_continuations() -> u32 { 3 }

/// Minimum the sub-agent needs to run tools and reach a provider. The daemon's
/// environment is otherwise cleared, so nothing else leaks into it.
fn default_env_passthrough() -> Vec<String> {
    [
        "PATH",
        "HOME",
        "LANG",
        "LC_ALL",
        "TMPDIR",
        "SHELL",
        "TERM",
        "USER",
        "SSH_AUTH_SOCK",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "GEMINI_API_KEY",
        "OPENROUTER_API_KEY",
        "GROQ_API_KEY",
        "XAI_API_KEY",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
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
    fn legacy_cowork_settings_do_not_prevent_config_loading() {
        let path = std::env::temp_dir().join(format!(
            "heart-portal-legacy-config-{}.toml",
            uuid::Uuid::new_v4()
        ));
        for enabled in [true, false] {
            std::fs::write(&path, format!(
                "name = 'legacy'\nbind = '127.0.0.1:65535'\nworkspace = './workspace'\n[cowork]\nenabled = {enabled}\nhttp_port = 9101\n"
            )).unwrap();
            let config = PortalConfig::load(path.to_str().unwrap()).unwrap();
            assert_eq!(config.name, "legacy");
            assert_eq!(config.bind_port, 65535);
            assert_eq!(
                config.security.workspace_root,
                path.parent().unwrap().join("workspace")
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn subagent_defaults_when_the_section_is_absent() {
        std::fs::write("/tmp/test-portal-nosub.toml", "name = \"vale\"\n").unwrap();
        let config = PortalConfig::load("/tmp/test-portal-nosub.toml").unwrap();
        let s = &config.subagent;
        assert!(s.enabled, "the sub-agent is on by default; availability gates the tools");
        assert!(!s.eager, "but the daemon starts lazily");
        assert_eq!(s.max_concurrent, 3);
        assert_eq!(s.idle_unload_secs, 1800);
        assert_eq!(s.daemon_idle_exit_secs, 0, "0 = keep the daemon while Portal runs");
        assert_eq!(s.budget.max_turns, 40);
        assert_eq!(s.budget.max_tokens, 400_000);
        assert_eq!(s.budget.timeout_secs, 1800);
        assert_eq!(s.budget.max_continuations, 3);
        assert!(s.command.is_none());
        assert!(s.auto_install, "a missing pi is provisioned unless opted out");
        assert!(s.env_passthrough.contains(&"PATH".to_string()));
        assert!(s.env_passthrough.contains(&"ANTHROPIC_API_KEY".to_string()));
    }

    #[test]
    fn subagent_section_is_parsed_from_toml() {
        let toml = r#"
name = "vale"

[subagent]
enabled = true
command = ["/opt/pi/bin/pi", "--quiet"]
state_dir = "/tmp/portal-sub-state"
eager = true
max_concurrent = 1
idle_unload_secs = 60
daemon_idle_exit_secs = 600
skills = ["/skills/review"]
env_passthrough = ["PATH", "ANTHROPIC_API_KEY"]
append_system_prompt = "Prefer small diffs."

[subagent.budget]
max_turns = 12
max_tokens = 80000
timeout_secs = 300

[subagent.model]
provider = "anthropic"
model = "claude-sonnet-4-5"
thinking = "medium"
api_key = "sk-or-v1-secret"
"#;
        std::fs::write("/tmp/test-portal-sub.toml", toml).unwrap();
        let config = PortalConfig::load("/tmp/test-portal-sub.toml").unwrap();
        let s = &config.subagent;
        assert_eq!(
            s.command.as_deref(),
            Some(&["/opt/pi/bin/pi".to_string(), "--quiet".to_string()][..])
        );
        assert_eq!(s.resolved_state_dir(), PathBuf::from("/tmp/portal-sub-state"));
        assert!(s.eager);
        assert_eq!(s.max_concurrent, 1);
        assert_eq!(s.idle_unload_secs, 60);
        assert_eq!(s.daemon_idle_exit_secs, 600);
        assert_eq!(s.budget.max_turns, 12);
        assert_eq!(s.budget.timeout_secs, 300);
        // Unset nested field keeps its default.
        assert_eq!(s.budget.max_continuations, 3);
        assert_eq!(s.model.provider.as_deref(), Some("anthropic"));
        assert_eq!(s.model.thinking.as_deref(), Some("medium"));
        // Only the stdio fallback needs it, but it is read either way.
        assert_eq!(s.model.api_key.as_deref(), Some("sk-or-v1-secret"));
        assert_eq!(s.skills, vec!["/skills/review".to_string()]);
        assert_eq!(s.append_system_prompt.as_deref(), Some("Prefer small diffs."));
    }

    #[test]
    fn subagent_state_dir_expands_a_leading_tilde() {
        let cfg = SubagentConfig::default();
        let resolved = cfg.resolved_state_dir();
        assert!(resolved.ends_with(".heart-portal/subagent"), "{resolved:?}");
        assert!(resolved.is_absolute(), "{resolved:?}");
    }

    #[test]
    fn subagent_can_be_disabled() {
        let toml = "name = \"vale\"\n\n[subagent]\nenabled = false\n";
        std::fs::write("/tmp/test-portal-suboff.toml", toml).unwrap();
        let config = PortalConfig::load("/tmp/test-portal-suboff.toml").unwrap();
        assert!(!config.subagent.enabled);
    }

    #[test]
    fn load_remembers_the_config_path_and_defaults_do_not() {
        let path = std::env::temp_dir()
            .join(format!("heart-portal-path-{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(&path, "name = \"vale\"\n").unwrap();
        let config = PortalConfig::load(path.to_str().unwrap()).unwrap();
        assert_eq!(config.config_path.as_deref(), Some(path.as_path()));
        assert!(PortalConfig::default().config_path.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn api_key_is_masked_to_a_short_prefix() {
        let mut model = SubagentModelConfig::default();
        assert_eq!(model.masked_api_key(), None);
        assert!(!model.is_configured());

        model.api_key = Some("sk-ant-api03-0123456789abcdefghijklmnop".to_string());
        assert_eq!(model.masked_api_key().as_deref(), Some("sk-ant-...***"));
        assert!(model.is_configured());

        // Too short to show any of it.
        model.api_key = Some("short".to_string());
        assert_eq!(model.masked_api_key().as_deref(), Some("***"));

        let status = model.to_status_json();
        assert_eq!(status["api_key"], "***");
        assert!(!status.to_string().contains("short"));
    }

    #[test]
    fn write_subagent_model_preserves_the_rest_of_the_file() {
        let dir = std::env::temp_dir().join(format!("heart-portal-edit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("portal.toml");
        std::fs::write(
            &path,
            "# My portal\nname = \"vale\"   # keep this comment\n\n[subagent]\nmax_concurrent = 2\n\n[subagent.model]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\n",
        )
        .unwrap();

        let model = SubagentModelConfig {
            provider: Some("anthropic".to_string()),
            model: Some("claude-sonnet-4-5".to_string()),
            thinking: None,
            api_key: Some("sk-ant-secret-value-1234567890".to_string()),
        };
        write_subagent_model(&path, &model).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("# My portal\n"), "{written}");
        assert!(written.contains("name = \"vale\"   # keep this comment"), "{written}");
        assert!(written.contains("max_concurrent = 2"), "{written}");
        assert!(!written.contains("gpt-4o"), "old model replaced: {written}");

        let reloaded = PortalConfig::load(path.to_str().unwrap()).unwrap();
        assert_eq!(reloaded.subagent.max_concurrent, 2);
        assert_eq!(reloaded.subagent.model.provider.as_deref(), Some("anthropic"));
        assert_eq!(reloaded.subagent.model.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(reloaded.subagent.model.thinking, None);
        assert_eq!(
            reloaded.subagent.model.api_key.as_deref(),
            Some("sk-ant-secret-value-1234567890")
        );

        // Clearing a key removes it rather than writing an empty string.
        let cleared = SubagentModelConfig { api_key: None, ..model };
        write_subagent_model(&path, &cleared).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("api_key"), "{written}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a file that may hold a key is private");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_subagent_model_creates_a_minimal_file_when_missing() {
        let dir = std::env::temp_dir().join(format!("heart-portal-new-{}", uuid::Uuid::new_v4()));
        let path = dir.join("nested").join("portal.toml");
        let model = SubagentModelConfig {
            provider: Some("openrouter".to_string()),
            model: Some("deepseek/deepseek-chat".to_string()),
            thinking: Some("low".to_string()),
            api_key: None,
        };
        write_subagent_model(&path, &model).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("[subagent.model]"), "{written}");
        assert!(!written.contains("[subagent]\n"), "no empty parent table: {written}");
        let reloaded = PortalConfig::load(path.to_str().unwrap()).unwrap();
        assert_eq!(reloaded.subagent.model.provider.as_deref(), Some("openrouter"));
        assert_eq!(reloaded.subagent.model.thinking.as_deref(), Some("low"));
        assert_eq!(reloaded.name, "portal", "everything else stays default");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_subagent_model_handles_an_inline_model_table() {
        let dir = std::env::temp_dir().join(format!("heart-portal-inline-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("portal.toml");
        std::fs::write(&path, "[subagent]\nmodel = { provider = \"groq\" }\n").unwrap();
        let model = SubagentModelConfig {
            provider: Some("xai".to_string()),
            ..SubagentModelConfig::default()
        };
        write_subagent_model(&path, &model).unwrap();
        let reloaded = PortalConfig::load(path.to_str().unwrap()).unwrap();
        assert_eq!(reloaded.subagent.model.provider.as_deref(), Some("xai"));
        let _ = std::fs::remove_dir_all(&dir);
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
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            PathBuf::from(config.kits_dir.unwrap()),
            path.parent().unwrap().join("/tmp/portal-kits")
        );
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
        let list = host
            .call("portal_file_list", serde_json::json!({"path": "."}))
            .await
            .unwrap();
        assert!(list["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("中文.txt"));
        let search = host
            .call(
                "portal_search",
                serde_json::json!({"path": ".", "pattern": "中文正常"}),
            )
            .await
            .unwrap();
        assert_eq!(search["match_count"], 1);
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
