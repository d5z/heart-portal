//! In-process diagnostics. Capture binary/configuration identity at startup;
//! queries only read the snapshot and shared in-memory connection/kit state.
use super::ToolHost;
use crate::{config::PortalConfig, kits::loader, paths::ConfigLocation, protocol::PORTAL_VERSION};
use anyhow::Result;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub(super) const KIT_REFRESH_INTERVAL_SECS: u64 = 5;

#[derive(Clone, Copy)]
#[repr(u8)]
pub(crate) enum ConnectionState {
    Starting,
    Connecting,
    Connected,
    Retrying,
    Invalid,
    Listening,
}

impl ConnectionState {
    fn label(value: u8) -> &'static str {
        match value {
            x if x == Self::Connecting as u8 => "connecting",
            x if x == Self::Connected as u8 => "connected",
            x if x == Self::Retrying as u8 => "retrying",
            x if x == Self::Invalid as u8 => "invalid",
            x if x == Self::Listening as u8 => "listening",
            _ => "starting",
        }
    }
}

pub(crate) struct RuntimeStatus {
    name: String,
    config_location: ConfigLocation,
    config_loaded: bool,
    relay: bool,
    started: Instant,
    started_at_unix_secs: Option<u64>,
    executable: Option<PathBuf>,
    build_id: Option<String>,
    user_directory: Option<PathBuf>,
    kits_directory: PathBuf,
    connection: AtomicU8,
}

impl RuntimeStatus {
    pub fn capture(
        config: &PortalConfig,
        location: ConfigLocation,
        loaded: bool,
        name: String,
        relay: bool,
        started: Instant,
    ) -> Self {
        let executable = std::env::current_exe().ok();
        // Hash once, before serving. Re-reading on a query could identify an
        // upgraded file on disk instead of the binary this process started with.
        // Diagnostics must remain available even when the executable is unreadable.
        let build_id = executable.as_deref().and_then(|path| binary_id(path).ok());
        Self {
            name,
            config_location: location,
            config_loaded: loaded,
            relay,
            started,
            started_at_unix_secs: SystemTime::now()
                .checked_sub(started.elapsed())
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs()),
            executable,
            build_id,
            user_directory: crate::paths::data_dir().ok(),
            kits_directory: loader::kits_dir(config),
            connection: AtomicU8::new(ConnectionState::Starting as u8),
        }
    }

    #[cfg(test)]
    pub(super) fn for_test(config: &PortalConfig) -> Self {
        Self::for_bridge(config)
    }

    pub(crate) fn for_bridge(config: &PortalConfig) -> Self {
        Self {
            name: config.name.clone(),
            config_location: ConfigLocation {
                path: PathBuf::from("test-portal.toml"),
                source: "in-memory-test",
            },
            config_loaded: false,
            relay: config.connect_link.is_some(),
            started: Instant::now(),
            started_at_unix_secs: None,
            executable: None,
            build_id: None,
            user_directory: None,
            kits_directory: loader::kits_dir(config),
            connection: AtomicU8::new(ConnectionState::Starting as u8),
        }
    }
}

fn binary_id(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("sha256:{:x}", hash.finalize()))
}

impl ToolHost {
    /// Called by the actual transport transitions on every supported platform.
    pub fn set_connection_state(&self, state: ConnectionState) {
        self.runtime
            .connection
            .store(state as u8, Ordering::Relaxed);
    }

    pub(super) async fn handle_status(&self) -> Result<Value> {
        let runtime = &self.runtime;
        let config = &self.config;
        let details = config.security.expose_host_details;
        // Do not use refresh_kits/list_healthy_tools here: both can change state.
        // Never include raw config, manifests, defaults, commands or error text.
        let statuses = self.kits.statuses().await;
        let mut by_status = BTreeMap::<&str, usize>::new();
        let kits: Vec<Value> = statuses.iter().map(|kit| {
            *by_status.entry(&kit.status).or_default() += 1;
            json!({"name": kit.name, "version": kit.version, "status": kit.status, "declared_tools": kit.tools,
                "process_id": kit.process_id, "diagnostics": kit.diagnostics,
                "next_action": kit.next_action, "service_authorization": kit.service_authorization})
        }).collect();
        let custom_count = self
            .custom
            .list_tools()
            .await
            .into_iter()
            .filter(|tool| !tool.name.replace('-', "_").starts_with("portal_"))
            .count();
        let status = json!({
            "schema_version": 1,
            "portal": {
                "name": runtime.name,
                "version": PORTAL_VERSION,
                "build_id": runtime.build_id,
                "executable": if details { json!(runtime.executable) } else { Value::Null },
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "pid": details.then(std::process::id),
                "started_at_unix_secs": runtime.started_at_unix_secs,
                "uptime_seconds": runtime.started.elapsed().as_secs(),
            },
            "connection": {
                "mode": if runtime.relay { "relay" } else { "tcp" },
                "state": ConnectionState::label(runtime.connection.load(Ordering::Relaxed)),
                "relay_configured": runtime.relay,
                "listener": if runtime.relay { Value::Null } else {
                    json!({"host": config.bind_host, "port": config.bind_port})
                },
            },
            "config": {
                "path": if details { json!(runtime.config_location.path) } else { Value::Null },
                "source": runtime.config_location.source,
                "loaded_from_file": runtime.config_loaded,
                "snapshot": "loaded-at-startup",
                "reload_requires_restart": true,
                "user_directory": if details { json!(runtime.user_directory) } else { Value::Null },
                "workspace": if details { json!(config.security.workspace_root) } else { Value::Null },
                "kits_directory": if details { json!(runtime.kits_directory) } else { Value::Null },
                "custom_tools_config": if details { json!(config.security.workspace_root.join("tools/mcp.toml")) } else { Value::Null },
                "warnings": config.warnings,
            },
            "capabilities": {
                "host_details_visible": details,
                "schema_version": 1,
                "read_only_status": true,
                "tools_list_changed_notifications": true,
                "kit_reload": {
                    "available": config.kits_enabled,
                    "tool": "portal_kits_reload",
                    "supports_target": true,
                    "automatic_changes": ["installation", "removal", "manifest", "environment", "credential-files"],
                    "explicit_reload_changes": ["code", "dependencies"],
                    "restarts_portal": false,
                    "activation": "next-tool-call",
                    "eager_prewarm": "portal-startup",
                },
                "tools_reload": {
                    "tool": "portal_tools_reload",
                    "custom_tools": config.tools.custom_tools_enabled,
                    "kits": false,
                },
                "portal_config_hot_reload": false,
                "custom_tools_config_path_override": false,
                "controlled_restart": self.restart_supported,
                "restart_may_disconnect": true,
                "service_authorization_verification": "kit-managed",
                "kit_isolation": {
                    "reserved_management_namespace": "portal_",
                    "targeted_reload_scope": "target-only",
                    "status_queries_are_read_only": true,
                    "concurrent_requests_on_shared_connection": true,
                    "standard_request_cancellation": true,
                    "shared_kit_and_custom_mcp_runtime": true,
                    "max_calls_per_connection": crate::mcp::limits::WORK_REQUESTS,
                    "reserved_management_requests_per_connection": crate::mcp::limits::MANAGEMENT_REQUESTS,
                    "process_tree_ownership": true,
                    "process_cleanup": if cfg!(windows) { "job-object" } else { "best-effort-process-group" },
                    "max_process_generations": crate::mcp::limits::PROCESS_GENERATIONS,
                    "max_generations_per_kit": crate::mcp::limits::KIT_GENERATIONS,
                    "same_os_user": true,
                    "exec_can_manage_host": config.tools.exec,
                    "max_mcp_message_bytes": crate::mcp::limits::MESSAGE_BYTES,
                    "environment": "runtime-allowlist-plus-explicit-kit-credentials",
                    "malicious_code_sandbox": false,
                },
            },
            "authorization": {
                "permission_mode": format!("{:?}", config.security.permission_mode),
                "os_sandbox": cfg!(windows) && config.security.permission_mode != crate::config::PermissionMode::FullAccess,
                "command_backend": if config.security.permission_mode == crate::config::PermissionMode::FullAccess { "host" }
                    else if config.security.permission_mode == crate::config::PermissionMode::ReadOnly { "disabled" }
                    else if cfg!(windows) { "windows-lpac" } else { "unavailable" },
                "command_network": if config.security.permission_mode == crate::config::PermissionMode::FullAccess { "host-policy" } else { "denied" },
                "subagent_tools": "portal-broker-in-restricted-mode",
                "approval_scope": "single-call",
            },
            "tools": {
                "exec": config.tools.exec,
                "file": config.tools.file,
                "screenshot": config.tools.screenshot,
                "web_fetch": config.tools.web_fetch,
                "search": config.tools.search,
                "custom_tools_enabled": config.tools.custom_tools_enabled,
                "custom_tools_loaded": custom_count,
            },
            "security": {
                "mcp_token_configured": config.portal_mcp_token.as_ref().is_some_and(|token| !token.is_empty()),
                "exec_allowlist_entries": config.security.exec_allowlist.len(),
                "max_file_size_bytes": config.security.max_file_size,
            },
            "kits": {
                "enabled": config.kits_enabled,
                "hot_reload": config.kits_enabled,
                "refresh_interval_seconds": config.kits_enabled.then_some(KIT_REFRESH_INTERVAL_SECS),
                "inventory": "loaded-manifests",
                "loaded": kits.len(),
                "by_status": by_status,
                "items": kits,
            },
            "supervision": {
                "restart_supported": self.restart_supported,
                "restart_pending": self.restart_requested.load(Ordering::Acquire),
            },
        });
        Ok(
            json!({"content": [{"type": "text", "text": serde_json::to_string(&status)?}], "isError": false}),
        )
    }
}

#[cfg(test)]
#[path = "status_tests.rs"]
mod tests;
