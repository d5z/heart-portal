use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::mcp::{McpConnection, McpServerConfig};
use crate::tools::ToolInfo;

use super::loader::{format_command, LoadedKit};

const MAX_FAILURES: u8 = 3;
const SPAWN_TIMEOUT_SECS: u64 = 30;
/// After a kit is marked unhealthy, wait this long before giving it another
/// chance so a transient failure does not require a Portal restart to recover.
const RECOVERY_COOLDOWN_SECS: u64 = 60;
/// Directories prepended to a kit process's PATH so kits can find common
/// toolchains (e.g. Homebrew-installed node/python) even when launched from a
/// launchd/systemd context with a minimal PATH.
const KIT_EXTRA_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";

#[derive(Clone)]
pub struct KitManager {
    kits: Arc<Mutex<BTreeMap<String, KitState>>>,
}

struct KitState {
    kit: LoadedKit,
    /// Wrapped in `Arc` so a caller can clone the handle, release the manager
    /// lock, and perform the MCP call without blocking other kits.
    connection: Option<Arc<McpConnection>>,
    failure_count: u8,
    unhealthy: bool,
    /// When the most recent failure occurred; used to gate self-healing after
    /// `RECOVERY_COOLDOWN_SECS`.
    last_failure_at: Option<Instant>,
}

#[derive(Debug, Clone)]
pub struct KitStatus {
    pub name: String,
    pub version: String,
    pub tools: usize,
    pub status: String,
}

impl KitManager {
    pub fn new(kits: Vec<LoadedKit>) -> Self {
        let mut states = BTreeMap::new();
        for kit in kits {
            let name = kit.manifest.name.clone();
            if states.contains_key(&name) {
                warn!("Duplicate kit name '{}'; keeping the last manifest loaded", name);
            }
            // Pre-mark unhealthy if command binary is missing or empty
            let pre_unhealthy = kit.command.is_empty()
                || (!kit.command[0].is_empty() && !command_binary_exists(&kit.command[0]));
            if pre_unhealthy {
                warn!(
                    "Kit '{}' pre-marked unhealthy: command binary not found: {}",
                    name,
                    kit.command.first().map(|s| s.as_str()).unwrap_or("<empty>")
                );
            }
            states.insert(
                name,
                KitState {
                    kit,
                    connection: None,
                    failure_count: 0,
                    unhealthy: pre_unhealthy,
                    last_failure_at: None,
                },
            );
        }

        Self {
            kits: Arc::new(Mutex::new(states)),
        }
    }

    pub async fn list_tools(&self) -> Vec<ToolInfo> {
        let kits = self.kits.lock().await;
        let mut tools = Vec::new();

        for state in kits.values() {
            push_kit_tools(state, &mut tools);
        }

        tools
    }

    pub async fn list_healthy_tools(&self) -> Vec<ToolInfo> {
        let kits = self.kits.lock().await;
        let mut tools = Vec::new();

        for state in kits.values() {
            if state.unhealthy {
                continue;
            }
            push_kit_tools(state, &mut tools);
        }

        tools
    }

    pub async fn resolve_tool(&self, tool_name: &str) -> Option<(String, String)> {
        let kits = self.kits.lock().await;
        for state in kits.values() {
            for tool in &state.kit.manifest.tools {
                let routed_name = format!("{}_{}", state.kit.manifest.name, tool.name);
                if routed_name == tool_name {
                    return Some((state.kit.manifest.name.clone(), tool.name.clone()));
                }
            }
        }
        None
    }

    pub async fn call_tool(
        &self,
        kit_name: &str,
        tool_name: &str,
        arguments: Value,
    ) -> Result<Value> {
        // Phase 1: hold the manager lock only long enough to validate the
        // request, (re)establish the connection, and clone the connection
        // handle. The lock is released before the actual MCP call so a slow
        // kit cannot block calls to other kits.
        let connection = {
            let mut kits = self.kits.lock().await;
            let state = kits
                .get_mut(kit_name)
                .ok_or_else(|| anyhow::anyhow!("Unknown kit: {}", kit_name))?;

            if !state
                .kit
                .manifest
                .tools
                .iter()
                .any(|tool| tool.name == tool_name)
            {
                anyhow::bail!("Unknown tool '{}' for kit '{}'", tool_name, kit_name);
            }

            ensure_connection(state).await?;

            state
                .connection
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Kit '{}' process is not running", kit_name))?
                .clone()
        };

        // Phase 2: perform the MCP call WITHOUT holding the manager lock.
        let result = connection.call_tool(tool_name, arguments).await;
        // Release our handle so `drop_connection` below can reclaim ownership
        // (via `Arc::try_unwrap`) to cleanly shut down a failed connection.
        drop(connection);

        // Phase 3: re-acquire the lock only to update health bookkeeping.
        let mut kits = self.kits.lock().await;
        let state = kits
            .get_mut(kit_name)
            .ok_or_else(|| anyhow::anyhow!("Unknown kit: {}", kit_name))?;

        match result {
            Ok(value) => {
                state.failure_count = 0;
                Ok(value)
            }
            Err(err) => {
                warn!("Kit '{}' tool '{}' failed: {}", kit_name, tool_name, err);
                drop_connection(state).await;
                record_failure(state);
                Err(err).with_context(|| {
                    format!("Failed to call kit '{}' tool '{}'", kit_name, tool_name)
                })
            }
        }
    }

    pub async fn shutdown(&self) {
        let mut kits = self.kits.lock().await;
        for (name, state) in kits.iter_mut() {
            if let Some(connection) = state.connection.take() {
                shutdown_arc(connection, name).await;
            }
        }
        info!("Kit processes shut down");
    }

    pub async fn statuses(&self) -> Vec<KitStatus> {
        let kits = self.kits.lock().await;
        kits.values()
            .map(|state| KitStatus {
                name: state.kit.manifest.name.clone(),
                version: state.kit.manifest.version.clone(),
                tools: state.kit.manifest.tools.len(),
                status: status_text(state).to_string(),
            })
            .collect()
    }
}

async fn ensure_connection(state: &mut KitState) -> Result<()> {
    ensure_connection_with_timeout(state, SPAWN_TIMEOUT_SECS).await
}

async fn ensure_connection_with_timeout(state: &mut KitState, timeout_secs: u64) -> Result<()> {
    // Self-healing: an unhealthy kit gets another chance once the cooldown has
    // elapsed, so a transient fault does not require a Portal restart.
    recover_if_cooled_down(state);

    if state.unhealthy {
        anyhow::bail!(
            "Kit '{}' is unhealthy. Command: {}. Check that the command exists and the MCP server implements the stdio protocol.",
            state.kit.manifest.name,
            format_command(&state.kit.command)
        );
    }

    if state
        .connection
        .as_ref()
        .map(|connection| connection.is_alive())
        .unwrap_or(false)
    {
        return Ok(());
    }

    drop_connection(state).await;

    if state.failure_count >= MAX_FAILURES {
        state.unhealthy = true;
        anyhow::bail!(
            "Kit '{}' is unhealthy after {} failed restart attempts. Command: {}. Check that the command exists and the MCP server implements the stdio protocol.",
            state.kit.manifest.name,
            state.failure_count,
            format_command(&state.kit.command)
        );
    }

    let kit_name = state.kit.manifest.name.clone();
    let command = state.kit.command.clone();
    let command_text = format_command(&command);

    info!(
        "Spawning kit '{}' with command: {}",
        kit_name, command_text
    );

    let config = McpServerConfig {
        name: kit_name.clone(),
        command,
        env: kit_env(state),
        cwd: Some(state.kit.kit_dir.clone()),
    };

    match timeout(
        Duration::from_secs(timeout_secs),
        McpConnection::spawn(config),
    )
    .await
    {
        Ok(Ok(connection)) => {
            debug!("Kit '{}' spawned", kit_name);
            state.connection = Some(Arc::new(connection));
            Ok(())
        }
        Ok(Err(err)) => {
            record_failure(state);
            let message = format!(
                "Kit '{}' failed to start: {}. Command: {}. Check that the command exists and the MCP server implements the stdio protocol.",
                kit_name, err, command_text
            );
            warn!("{}", message);
            Err(anyhow::anyhow!("{}", message))
        }
        Err(_) => {
            // A spawn timeout is treated like any other failure: increment the
            // failure count and let the gradual MAX_FAILURES mechanism decide
            // when to mark the kit unhealthy, rather than doing so immediately.
            record_failure(state);
            let message = format!(
                "Kit '{}' failed to start: timed out after {} seconds. Command: {}. Check that the command exists and the MCP server implements the stdio protocol.",
                kit_name,
                timeout_secs,
                command_text
            );
            warn!("{}", message);
            Err(anyhow::anyhow!("{}", message))
        }
    }
}

/// Reset an unhealthy kit back to a retryable state once the recovery cooldown
/// has elapsed since its last recorded failure.
fn recover_if_cooled_down(state: &mut KitState) {
    if !state.unhealthy {
        return;
    }

    let cooled_down = state
        .last_failure_at
        .map(|at| at.elapsed() >= Duration::from_secs(RECOVERY_COOLDOWN_SECS))
        .unwrap_or(false);

    if cooled_down {
        info!(
            "Kit '{}' cooldown elapsed after {}s; giving it another chance",
            state.kit.manifest.name, RECOVERY_COOLDOWN_SECS
        );
        state.unhealthy = false;
        state.failure_count = 0;
    }
}

fn push_kit_tools(state: &KitState, tools: &mut Vec<ToolInfo>) {
    for tool in &state.kit.manifest.tools {
        tools.push(ToolInfo {
            name: format!("{}_{}", state.kit.manifest.name, tool.name),
            description: tool.description.clone(),
            input_schema: tool.params.clone(),
        });
    }
}

async fn drop_connection(state: &mut KitState) {
    let kit_name = state.kit.manifest.name.clone();
    if let Some(connection) = state.connection.take() {
        shutdown_arc(connection, &kit_name).await;
    }
}

/// Shut down a shared connection handle. Requires unique ownership to obtain
/// the `&mut` needed by `McpConnection::shutdown`; if a concurrent caller still
/// holds the handle, cleanup is deferred until the final reference is dropped.
async fn shutdown_arc(connection: Arc<McpConnection>, kit_name: &str) {
    match Arc::try_unwrap(connection) {
        Ok(mut connection) => {
            if let Err(err) = connection.shutdown().await {
                warn!("Failed to shut down kit '{}' connection: {}", kit_name, err);
            }
        }
        Err(_) => {
            warn!(
                "Kit '{}' connection still in use by an in-flight call; deferring shutdown",
                kit_name
            );
        }
    }
}

fn record_failure(state: &mut KitState) {
    state.failure_count = state.failure_count.saturating_add(1);
    state.last_failure_at = Some(Instant::now());
    if state.failure_count >= MAX_FAILURES {
        state.unhealthy = true;
        warn!(
            "Kit '{}' marked unhealthy after {} failures",
            state.kit.manifest.name, state.failure_count
        );
    }
}

fn kit_env(state: &KitState) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert(
        "PORTAL_KIT_NAME".to_string(),
        state.kit.manifest.name.clone(),
    );
    env.insert(
        "PORTAL_KIT_DIR".to_string(),
        state.kit.kit_dir.to_string_lossy().to_string(),
    );
    // Kit processes may be launched from a launchd/systemd context whose PATH
    // lacks common toolchain locations (e.g. /opt/homebrew/bin). Prepend the
    // well-known directories to whatever PATH we inherited.
    let path = match std::env::var("PATH") {
        Ok(existing) if !existing.is_empty() => format!("{}:{}", KIT_EXTRA_PATH, existing),
        _ => KIT_EXTRA_PATH.to_string(),
    };
    env.insert("PATH".to_string(), path);
    env
}

fn status_text(state: &KitState) -> &'static str {
    if state.unhealthy {
        "unhealthy"
    } else if state
        .connection
        .as_ref()
        .map(|connection| connection.is_alive())
        .unwrap_or(false)
    {
        "healthy"
    } else {
        "not-started"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kits::manifest::{KitManifest, KitToolDef};
    use std::path::PathBuf;

    #[tokio::test]
    async fn list_healthy_tools_skips_unhealthy_kits() {
        // "broken" kit has a missing binary → pre-marked unhealthy by KitManager::new()
        // "healthy" kit uses /bin/echo → not pre-marked
        let manager = KitManager::new(vec![loaded_kit("healthy"), loaded_kit("broken")]);

        let all_tools = manager.list_tools().await;
        let healthy_tools = manager.list_healthy_tools().await;

        assert_eq!(all_tools.len(), 2);
        assert_eq!(healthy_tools.len(), 1);
        assert_eq!(healthy_tools[0].name, "healthy_ping");
    }

    #[tokio::test]
    async fn spawn_timeout_increments_failure_count_without_immediate_unhealthy() {
        // `/bin/sleep 30` spawns successfully but never speaks the MCP protocol,
        // so `initialize` hangs and the spawn attempt hits the (short) timeout.
        let mut state = kit_state(loaded_kit_argv("slow", vec!["/bin/sleep", "30"]));

        let result = ensure_connection_with_timeout(&mut state, 1).await;

        assert!(result.is_err(), "spawn should time out");
        assert_eq!(
            state.failure_count, 1,
            "a spawn timeout must increment failure_count"
        );
        assert!(
            !state.unhealthy,
            "a single spawn timeout must not immediately mark the kit unhealthy"
        );
        assert!(
            state.last_failure_at.is_some(),
            "a failure must record its timestamp for the recovery cooldown"
        );
    }

    #[test]
    fn unhealthy_kit_recovers_after_cooldown() {
        let mut state = kit_state(loaded_kit("healthy"));
        state.unhealthy = true;
        state.failure_count = MAX_FAILURES;
        state.last_failure_at =
            Instant::now().checked_sub(Duration::from_secs(RECOVERY_COOLDOWN_SECS + 1));
        assert!(
            state.last_failure_at.is_some(),
            "test host must have enough uptime to construct a past Instant"
        );

        recover_if_cooled_down(&mut state);

        assert!(
            !state.unhealthy,
            "an unhealthy kit should recover once the cooldown has elapsed"
        );
        assert_eq!(state.failure_count, 0, "recovery should reset failure_count");
    }

    #[test]
    fn unhealthy_kit_stays_unhealthy_within_cooldown() {
        let mut state = kit_state(loaded_kit("healthy"));
        state.unhealthy = true;
        state.failure_count = MAX_FAILURES;
        state.last_failure_at = Some(Instant::now());

        recover_if_cooled_down(&mut state);

        assert!(
            state.unhealthy,
            "a kit within its cooldown window must stay unhealthy"
        );
    }

    #[test]
    fn kit_env_includes_path() {
        let state = kit_state(loaded_kit("healthy"));

        let env = kit_env(&state);

        let path = env.get("PATH").expect("kit_env must set PATH");
        assert!(
            path.contains("/opt/homebrew/bin"),
            "PATH should include Homebrew's bin dir, got: {}",
            path
        );
        assert!(
            path.contains("/usr/bin"),
            "PATH should include /usr/bin, got: {}",
            path
        );
        assert_eq!(
            env.get("PORTAL_KIT_NAME").map(String::as_str),
            Some("healthy"),
            "existing kit env vars must be preserved"
        );
    }

    fn kit_state(kit: LoadedKit) -> KitState {
        KitState {
            kit,
            connection: None,
            failure_count: 0,
            unhealthy: false,
            last_failure_at: None,
        }
    }

    fn loaded_kit(name: &str) -> LoadedKit {
        // Use /bin/echo as a real binary so the healthy kit isn't pre-marked unhealthy
        let cmd = if name == "broken" {
            "definitely-missing-kit-binary"
        } else {
            "/bin/echo"
        };
        loaded_kit_argv(name, vec![cmd])
    }

    fn loaded_kit_argv(name: &str, argv: Vec<&str>) -> LoadedKit {
        let command: Vec<String> = argv.into_iter().map(str::to_string).collect();
        LoadedKit {
            manifest: KitManifest {
                name: name.to_string(),
                version: "0.1.0".to_string(),
                description: None,
                author: None,
                platform: None,
                runtime: None,
                command: command.clone(),
                tools: vec![KitToolDef {
                    name: "ping".to_string(),
                    description: "Ping".to_string(),
                    params: serde_json::json!({"type": "object"}),
                }],
                permissions: None,
                workspace: None,
            },
            kit_dir: PathBuf::from("/tmp"),
            command,
        }
    }
}

/// Check if a command binary exists — supports both absolute paths and PATH lookup.
fn command_binary_exists(binary: &str) -> bool {
    let path = std::path::Path::new(binary);
    // Absolute or relative path with separator → check directly
    if binary.contains(std::path::MAIN_SEPARATOR) || binary.contains('/') {
        return path.exists();
    }
    // Bare command name → search PATH
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .any(|dir| dir.join(binary).is_file())
        })
        .unwrap_or(false)
}
