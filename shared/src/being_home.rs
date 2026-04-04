//! Standard Being home directory layout.
//!
//! Resolves all paths from a single `BEING_HOME` env var, with fallbacks
//! for legacy deployments (Docker mounts, individual env vars).
//!
//! Standard structure:
//! ```text
//! {BEING_HOME}/
//! ├── config/          features.toml, crons.toml, mcp-servers.toml
//! ├── data/            self.being, memory/
//! ├── logs/
//! ├── web/             loom.html
//! ├── workspace/
//! └── prompt/          identity.toml
//! ```
//!
//! When `BEING_HOME` is not set, falls back to legacy paths:
//! - Config: `/config/` (Docker mount)
//! - Data: `MEMORY_DIR` env or `/data/`
//! - Web/Prompt: `/app/web/`, `/app/prompt/`

use std::path::{Path, PathBuf};

/// Being home directory resolver.
///
/// Usage:
/// ```rust,ignore
/// let home = BeingHome::from_env();
/// let features = home.config_file("features.toml", "FEATURES_TOML");
/// let being = home.data_file("self.being", "BEING_FILE");
/// ```
#[derive(Debug, Clone)]
pub struct BeingHome {
    root: Option<PathBuf>,
}

impl BeingHome {
    /// Resolve from `BEING_HOME` environment variable.
    /// Returns a `BeingHome` regardless — if the env var is unset,
    /// all methods fall back to legacy paths.
    pub fn from_env() -> Self {
        let root = std::env::var("BEING_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        if let Some(ref r) = root {
            tracing::info!("BeingHome: {}", r.display());
        }
        Self { root }
    }

    /// The root directory, if set.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Whether BEING_HOME is explicitly configured.
    pub fn is_set(&self) -> bool {
        self.root.is_some()
    }

    // ── Config files ──────────────────────────────────────────────

    /// Resolve a config file path.
    /// Priority: `env_override` → `{BEING_HOME}/config/{name}` → `/config/{name}`
    pub fn config_file(&self, name: &str, env_override: &str) -> PathBuf {
        if let Some(p) = non_empty_env(env_override) {
            return PathBuf::from(p);
        }
        if let Some(ref root) = self.root {
            return root.join("config").join(name);
        }
        // Docker legacy: /config/ mount
        PathBuf::from("/config").join(name)
    }

    /// Resolve config directory.
    pub fn config_dir(&self) -> PathBuf {
        if let Some(ref root) = self.root {
            return root.join("config");
        }
        PathBuf::from("/config")
    }

    // ── Data files ────────────────────────────────────────────────

    /// Resolve a data file path.
    /// Priority: `env_override` → `{BEING_HOME}/data/{name}` → `/data/{name}`
    pub fn data_file(&self, name: &str, env_override: &str) -> PathBuf {
        if let Some(p) = non_empty_env(env_override) {
            return PathBuf::from(p);
        }
        if let Some(ref root) = self.root {
            return root.join("data").join(name);
        }
        PathBuf::from("/data").join(name)
    }

    /// Resolve data directory (replaces MEMORY_DIR / HEART_MEMORY_DIR).
    /// Priority: `HEART_MEMORY_DIR` → `MEMORY_DIR` → `{BEING_HOME}/data/` → `/data/`
    pub fn data_dir(&self) -> PathBuf {
        if let Some(p) = non_empty_env("HEART_MEMORY_DIR") {
            return PathBuf::from(p);
        }
        if let Some(p) = non_empty_env("MEMORY_DIR") {
            return PathBuf::from(p);
        }
        if let Some(ref root) = self.root {
            return root.join("data");
        }
        PathBuf::from("/data")
    }

    // ── Being file ────────────────────────────────────────────────

    /// Resolve the .being database path.
    /// Priority: `BEING_FILE` → `{BEING_HOME}/data/self.being` → `/data/self.being`
    ///
    /// Also checks for `{name}.being` patterns using BEING_HOME/BEING_NAME env vars
    /// for backward compat with named being files.
    pub fn being_file(&self) -> PathBuf {
        if let Some(p) = non_empty_env("BEING_FILE") {
            return PathBuf::from(p);
        }
        if let Some(ref root) = self.root {
            return root.join("data").join("self.being");
        }
        PathBuf::from("/data").join("self.being")
    }

    // ── Web ───────────────────────────────────────────────────────

    /// Resolve Loom HTML path.
    /// Priority: `LOOM_HTML` → `{BEING_HOME}/web/loom.html` → `/config/loom.html`
    pub fn loom_html(&self) -> PathBuf {
        if let Some(p) = non_empty_env("LOOM_HTML") {
            return PathBuf::from(p);
        }
        if let Some(ref root) = self.root {
            return root.join("web").join("loom.html");
        }
        // Docker legacy: loom.html in /config/ mount
        PathBuf::from("/config").join("loom.html")
    }

    // ── Prompt / Identity ─────────────────────────────────────────

    /// Resolve prompt/identity directory.
    /// Priority: `IDENTITY_DIR` → `{BEING_HOME}/prompt/` → `{data_dir}/../prompt/`
    pub fn prompt_dir(&self) -> PathBuf {
        if let Some(p) = non_empty_env("IDENTITY_DIR") {
            return PathBuf::from(p);
        }
        if let Some(ref root) = self.root {
            return root.join("prompt");
        }
        // Legacy: sibling of data dir
        let data = self.data_dir();
        data.parent().map(|p| p.join("prompt")).unwrap_or_else(|| PathBuf::from("/app/prompt"))
    }

    // ── Workspace ─────────────────────────────────────────────────

    /// Resolve workspace directory.
    /// Priority: `HEART_WORKSPACE_DIR` → `{BEING_HOME}/workspace/` → `/app/workspace/`
    pub fn workspace_dir(&self) -> PathBuf {
        if let Some(p) = non_empty_env("HEART_WORKSPACE_DIR") {
            return PathBuf::from(p);
        }
        if let Some(ref root) = self.root {
            return root.join("workspace");
        }
        PathBuf::from("/app/workspace")
    }
}

/// Helper: read an env var, return None if unset or empty.
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn clear_being_env() {
        for key in &[
            "BEING_HOME", "BEING_FILE", "BEING_HOME", "BEING_NAME",
            "MEMORY_DIR", "HEART_MEMORY_DIR", "FEATURES_TOML",
            "LOOM_HTML", "IDENTITY_DIR", "HEART_WORKSPACE_DIR",
            "MCP_CONFIG_PATH", "CRONS_PATH",
        ] {
            unsafe { std::env::remove_var(key); }
        }
    }

    #[test]
    #[serial]
    fn being_home_set() {
        clear_being_env();
        unsafe { std::env::set_var("BEING_HOME", "/opt/echo"); }

        let home = BeingHome::from_env();
        assert!(home.is_set());
        assert_eq!(home.config_file("features.toml", "FEATURES_TOML"),
            PathBuf::from("/opt/echo/config/features.toml"));
        assert_eq!(home.data_file("self.being", "BEING_FILE"),
            PathBuf::from("/opt/echo/data/self.being"));
        assert_eq!(home.data_dir(), PathBuf::from("/opt/echo/data"));
        assert_eq!(home.being_file(), PathBuf::from("/opt/echo/data/self.being"));
        assert_eq!(home.loom_html(), PathBuf::from("/opt/echo/web/loom.html"));
        assert_eq!(home.prompt_dir(), PathBuf::from("/opt/echo/prompt"));
        assert_eq!(home.workspace_dir(), PathBuf::from("/opt/echo/workspace"));
    }

    #[test]
    #[serial]
    fn being_home_not_set_docker_fallback() {
        clear_being_env();

        let home = BeingHome::from_env();
        assert!(!home.is_set());
        // Falls back to Docker paths
        assert_eq!(home.config_file("features.toml", "FEATURES_TOML"),
            PathBuf::from("/config/features.toml"));
        assert_eq!(home.data_dir(), PathBuf::from("/data"));
        assert_eq!(home.being_file(), PathBuf::from("/data/self.being"));
    }

    #[test]
    #[serial]
    fn env_override_takes_precedence() {
        clear_being_env();
        unsafe {
            std::env::set_var("BEING_HOME", "/opt/echo");
            std::env::set_var("FEATURES_TOML", "/custom/features.toml");
            std::env::set_var("BEING_FILE", "/custom/my.being");
            std::env::set_var("HEART_MEMORY_DIR", "/custom/data");
            std::env::set_var("LOOM_HTML", "/custom/loom.html");
            std::env::set_var("IDENTITY_DIR", "/custom/prompt");
            std::env::set_var("HEART_WORKSPACE_DIR", "/custom/workspace");
        }

        let home = BeingHome::from_env();
        assert_eq!(home.config_file("features.toml", "FEATURES_TOML"),
            PathBuf::from("/custom/features.toml"));
        assert_eq!(home.being_file(), PathBuf::from("/custom/my.being"));
        assert_eq!(home.data_dir(), PathBuf::from("/custom/data"));
        assert_eq!(home.loom_html(), PathBuf::from("/custom/loom.html"));
        assert_eq!(home.prompt_dir(), PathBuf::from("/custom/prompt"));
        assert_eq!(home.workspace_dir(), PathBuf::from("/custom/workspace"));
    }

    #[test]
    #[serial]
    fn empty_env_treated_as_unset() {
        clear_being_env();
        unsafe {
            std::env::set_var("BEING_HOME", "/opt/echo");
            std::env::set_var("FEATURES_TOML", "");  // empty = ignored
        }

        let home = BeingHome::from_env();
        assert_eq!(home.config_file("features.toml", "FEATURES_TOML"),
            PathBuf::from("/opt/echo/config/features.toml"));
    }

    #[test]
    #[serial]
    fn config_dir_resolution() {
        clear_being_env();
        unsafe { std::env::set_var("BEING_HOME", "/Users/sw/sw.home"); }

        let home = BeingHome::from_env();
        assert_eq!(home.config_dir(), PathBuf::from("/Users/sw/sw.home/config"));
        assert_eq!(home.config_file("mcp-servers.toml", "MCP_CONFIG_PATH"),
            PathBuf::from("/Users/sw/sw.home/config/mcp-servers.toml"));
        assert_eq!(home.config_file("crons.toml", "CRONS_PATH"),
            PathBuf::from("/Users/sw/sw.home/config/crons.toml"));
    }
}
