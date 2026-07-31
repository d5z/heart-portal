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
const WARMUP_TIMEOUT_SECS: u64 = 10;

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
    /// Calls since last heartbeat report
    unsent_calls: u64,
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
                    unsent_calls: 0,
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
        let mut kits = self.kits.lock().await;
        let mut tools = Vec::new();

        for state in kits.values_mut() {
            recover_if_cooled_down(state);
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
                let routed_name = format!("{}_{}", kit_slug(&state.kit.manifest.name), tool.name);
                let normalized_query = tool_name.replace('-', "_");
                if routed_name == normalized_query {
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
        mut arguments: Value,
    ) -> Result<Value> {
        // Phase 1: hold the manager lock only long enough to validate the
        // request, (re)establish the connection, and clone the connection
        // handle. The lock is released before the actual MCP call so a slow
        // kit cannot block calls to other kits.
        let (connection, params) = {
            let mut kits = self.kits.lock().await;
            let state = kits
                .get_mut(kit_name)
                .ok_or_else(|| anyhow::anyhow!("Unknown kit: {}", kit_name))?;

            let params = state
                .kit
                .manifest
                .tools
                .iter()
                .find(|tool| tool.name == tool_name)
                .map(|tool| tool.params.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!("Unknown tool '{}' for kit '{}'", tool_name, kit_name)
                })?;

            ensure_connection(state).await?;

            let connection = state
                .connection
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Kit '{}' process is not running", kit_name))?
                .clone();

            (connection, params)
        };

        // Heart's act DSL passes all parameter values as JSON strings; coerce
        // them to the types declared in the kit tool's JSON Schema before the
        // MCP call so kit-side validation does not reject e.g. limit="3".
        coerce_kit_arguments(&mut arguments, &params);

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
                state.unsent_calls += 1;
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

    /// Pre-spawn kits marked `eager: true` so the first tool call has no
    /// cold-start latency. Failures are logged and non-fatal.
    pub async fn warmup(&self) {
        // Phase 1: collect eager, healthy kit names (release lock afterwards).
        let eager_names: Vec<String> = {
            let kits = self.kits.lock().await;
            kits.iter()
                .filter(|(_, state)| state.kit.manifest.eager == Some(true) && !state.unhealthy)
                .map(|(name, _)| name.clone())
                .collect()
        };

        if eager_names.is_empty() {
            return;
        }

        info!("Warming up {} eager kit(s)", eager_names.len());

        // Phase 2: spawn each kit one at a time, releasing the lock between attempts
        // so other callers are not blocked for the full warmup window.
        for name in eager_names {
            let mut kits = self.kits.lock().await;
            let Some(state) = kits.get_mut(&name) else {
                continue;
            };
            // Double-check eligibility under the lock.
            if state.kit.manifest.eager != Some(true) || state.unhealthy {
                continue;
            }

            match ensure_connection_with_timeout(state, WARMUP_TIMEOUT_SECS).await {
                Ok(()) => info!("Warmed up eager kit '{}'", name),
                Err(err) => warn!("Failed to warm up eager kit '{}': {}", name, err),
            }
        }
    }

    pub async fn shutdown(&self) {
        let mut kits = self.kits.lock().await;
        for (name, state) in kits.iter_mut() {
            if let Some(connection) = state.connection.take() {
                match Arc::try_unwrap(connection) {
                    Ok(mut connection) => {
                        if let Err(err) = connection.shutdown().await {
                            warn!("Failed to shut down kit '{}': {}", name, err);
                        }
                    }
                    Err(_) => {
                        warn!(
                            "Kit '{}' connection still shared; dropping without shutdown",
                            name
                        );
                    }
                }
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

    /// Reconcile in-memory kit state with a freshly scanned kit list.
    /// Existing connections are dropped (not shut down) so the next tool call
    /// re-spawns with the updated manifest; `unsent_calls` is preserved.
    pub async fn refresh_kits(&self, fresh: Vec<LoadedKit>) {
        let mut kits = self.kits.lock().await;
        let mut seen = std::collections::HashSet::new();

        for kit in fresh {
            let name = kit.manifest.name.clone();
            seen.insert(name.clone());

            if let Some(state) = kits.get_mut(&name) {
                let old_json = serde_json::to_string(&state.kit.manifest).unwrap_or_default();
                let new_json = serde_json::to_string(&kit.manifest).unwrap_or_default();
                if old_json != new_json {
                    let old_version = state.kit.manifest.version.clone();
                    let new_version = kit.manifest.version.clone();
                    state.kit.manifest = kit.manifest;
                    state.kit.command = kit.command;
                    state.kit.kit_dir = kit.kit_dir;
                    // Drop the handle only — do not shut down the process.
                    state.connection = None;
                    state.failure_count = 0;
                    state.unhealthy = false;
                    state.last_failure_at = None;
                    info!(
                        "Kit '{}' manifest refreshed (v{} → v{})",
                        name, old_version, new_version
                    );
                }
            } else {
                let version = kit.manifest.version.clone();
                let pre_unhealthy = kit.command.is_empty()
                    || (!kit.command[0].is_empty() && !command_binary_exists(&kit.command[0]));
                if pre_unhealthy {
                    warn!(
                        "Kit '{}' pre-marked unhealthy: command binary not found: {}",
                        name,
                        kit.command.first().map(|s| s.as_str()).unwrap_or("<empty>")
                    );
                }
                kits.insert(
                    name.clone(),
                    KitState {
                        kit,
                        connection: None,
                        failure_count: 0,
                        unhealthy: pre_unhealthy,
                        last_failure_at: None,
                        unsent_calls: 0,
                    },
                );
                info!("Kit '{}' discovered (v{})", name, version);
            }
        }

        let to_remove: Vec<String> = kits
            .keys()
            .filter(|name| !seen.contains(*name))
            .cloned()
            .collect();
        for name in to_remove {
            if let Some(mut state) = kits.remove(&name) {
                // Drop the handle only — do not shut down the process.
                state.connection = None;
                info!("Kit '{}' removed", name);
            }
        }
    }

    /// Periodically report incremental kit call counts to Grove.
    pub fn start_heartbeat_task(
        &self,
        grove_url: String,
        grove_token: String,
    ) -> tokio::task::JoinHandle<()> {
        let kits = self.kits.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            let mut interval = tokio::time::interval(Duration::from_secs(300));
            loop {
                interval.tick().await;
                flush_heartbeats_inner(&kits, &client, &grove_url, &grove_token).await;
            }
        })
    }

    /// Flush remaining unsent call counts once (e.g. on graceful shutdown).
    pub async fn flush_heartbeats(&self, grove_url: &str, grove_token: &str) {
        let client = reqwest::Client::new();
        flush_heartbeats_inner(&self.kits, &client, grove_url, grove_token).await;
    }
}

async fn flush_heartbeats_inner(
    kits: &Arc<Mutex<BTreeMap<String, KitState>>>,
    client: &reqwest::Client,
    grove_url: &str,
    grove_token: &str,
) {
    let pending: Vec<(String, u64)> = {
        let mut kits = kits.lock().await;
        let mut pending = Vec::new();
        for (name, state) in kits.iter_mut() {
            if state.unsent_calls > 0 {
                let count = state.unsent_calls;
                state.unsent_calls = 0;
                pending.push((name.clone(), count));
            }
        }
        pending
    };

    for (kit_name, calls) in pending {
        let url = format!(
            "{}/api/grove/{}/heartbeat?token={}",
            grove_url, kit_name, grove_token
        );
        let result = client
            .post(&url)
            .json(&serde_json::json!({ "calls": calls }))
            .send()
            .await;

        let ok = match result {
            Ok(resp) if resp.status().is_success() => true,
            Ok(resp) => {
                warn!(
                    "Grove heartbeat for kit '{}' failed: HTTP {}",
                    kit_name,
                    resp.status()
                );
                false
            }
            Err(err) => {
                warn!("Grove heartbeat for kit '{}' failed: {}", kit_name, err);
                false
            }
        };

        if !ok {
            let mut kits = kits.lock().await;
            if let Some(state) = kits.get_mut(&kit_name) {
                state.unsent_calls = state.unsent_calls.saturating_add(calls);
            }
        }
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

/// Normalize kit name for tool routing: replace hyphens with underscores.
fn kit_slug(name: &str) -> String {
    name.replace('-', "_")
}

fn push_kit_tools(state: &KitState, tools: &mut Vec<ToolInfo>) {
    for tool in &state.kit.manifest.tools {
        tools.push(ToolInfo {
            name: format!("{}_{}", kit_slug(&state.kit.manifest.name), tool.name),
            description: tool.description.clone(),
            input_schema: tool.params.clone(),
        });
    }
}

async fn drop_connection(state: &mut KitState) {
    if let Some(connection) = state.connection.take() {
        match Arc::try_unwrap(connection) {
            Ok(mut connection) => {
                if let Err(err) = connection.shutdown().await {
                    warn!(
                        "Failed to shut down kit '{}' connection: {}",
                        state.kit.manifest.name, err
                    );
                }
            }
            Err(_) => {
                warn!(
                    "Kit '{}' connection still shared; dropping without shutdown",
                    state.kit.manifest.name
                );
            }
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

/// Best-effort coercion of string-typed act-DSL arguments to the types
/// declared in a kit tool's JSON Schema `properties`. Parse failures leave
/// the original value so the kit can report the real validation error.
fn coerce_kit_arguments(arguments: &mut Value, schema: &Value) {
    let Some(args_obj) = arguments.as_object_mut() else {
        return;
    };
    let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) else {
        return;
    };

    for (key, prop_schema) in properties {
        let Some(value) = args_obj.get_mut(key) else {
            continue;
        };
        let Some(type_str) = prop_schema.get("type").and_then(|t| t.as_str()) else {
            continue;
        };
        let Value::String(s) = value else {
            continue;
        };

        let coerced = match type_str {
            "integer" => s.parse::<i64>().ok().map(Value::from),
            "number" => s.parse::<f64>().ok().map(Value::from),
            "boolean" => match s.as_str() {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                _ => None,
            },
            "array" => serde_json::from_str::<Value>(s)
                .ok()
                .filter(|v| v.is_array()),
            _ => None,
        };

        if let Some(new_val) = coerced {
            *value = new_val;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kits::manifest::{KitManifest, KitToolDef};
    use std::path::PathBuf;

    #[test]
    fn kit_slug_normalizes_hyphens() {
        assert_eq!(kit_slug("agent-reach"), "agent_reach");
        assert_eq!(kit_slug("cua-driver"), "cua_driver");
        assert_eq!(kit_slug("cursor"), "cursor");
        assert_eq!(kit_slug("a-b-c"), "a_b_c");
    }

    #[tokio::test]
    async fn resolve_tool_normalizes_kit_hyphens() {
        let manager = KitManager::new(vec![loaded_kit("my-kit", None)]);
        // Should resolve with underscored form
        let result = manager.resolve_tool("my_kit_ping").await;
        assert!(result.is_some(), "should resolve my_kit_ping");
        let (kit_name, tool_name) = result.unwrap();
        assert_eq!(kit_name, "my-kit", "should return original kit name for internal lookup");
        assert_eq!(tool_name, "ping");
        // Should also resolve with hyphenated form (backward compat)
        let result2 = manager.resolve_tool("my-kit_ping").await;
        assert!(result2.is_some(), "should resolve my-kit_ping (hyphen form)");
        let (kit_name2, tool_name2) = result2.unwrap();
        assert_eq!(kit_name2, "my-kit");
        assert_eq!(tool_name2, "ping");
    }

    #[tokio::test]
    async fn list_tools_uses_normalized_names() {
        let manager = KitManager::new(vec![loaded_kit("my-kit", None)]);
        let tools = manager.list_tools().await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "my_kit_ping", "exposed tool name should use underscores");
    }

    #[tokio::test]
    async fn list_healthy_tools_skips_unhealthy_kits() {
        // "broken" kit has a missing binary → pre-marked unhealthy by KitManager::new()
        // "healthy" kit uses /bin/echo → not pre-marked
        let manager = KitManager::new(vec![loaded_kit("healthy", None), loaded_kit("broken", None)]);

        let all_tools = manager.list_tools().await;
        let healthy_tools = manager.list_healthy_tools().await;

        assert_eq!(all_tools.len(), 2);
        assert_eq!(healthy_tools.len(), 1);
        assert_eq!(healthy_tools[0].name, "healthy_ping");
    }

    #[tokio::test]
    async fn warmup_skips_unhealthy_and_non_eager_kits() {
        // eager=true healthy, eager=false healthy, eager=true but unhealthy (missing binary)
        let manager = KitManager::new(vec![
            loaded_kit("eager-ok", Some(true)),
            loaded_kit("not-eager", Some(false)),
            loaded_kit("broken", Some(true)),
        ]);

        manager.warmup().await;

        // Manager remains functional after warmup (failures are non-fatal).
        let all_tools = manager.list_tools().await;
        let healthy_tools = manager.list_healthy_tools().await;
        assert_eq!(all_tools.len(), 3);
        assert_eq!(healthy_tools.len(), 2);
        let statuses = manager.statuses().await;
        assert_eq!(statuses.len(), 3);
    }

    #[test]
    fn unhealthy_kit_recovers_after_cooldown() {
        let mut state = kit_state(loaded_kit("healthy", None));
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
        let mut state = kit_state(loaded_kit("healthy", None));
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
        let state = kit_state(loaded_kit("healthy", None));

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

    #[test]
    fn coerce_string_to_integer() {
        let mut args = serde_json::json!({"limit": "42"});
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer"}
            }
        });
        coerce_kit_arguments(&mut args, &schema);
        assert_eq!(args["limit"], 42);
    }

    #[test]
    fn coerce_string_to_number() {
        let mut args = serde_json::json!({"ratio": "3.14"});
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "ratio": {"type": "number"}
            }
        });
        coerce_kit_arguments(&mut args, &schema);
        assert_eq!(args["ratio"], 3.14);
    }

    #[test]
    fn coerce_string_to_boolean() {
        let mut args = serde_json::json!({"flag": "true", "other": "false"});
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "flag": {"type": "boolean"},
                "other": {"type": "boolean"}
            }
        });
        coerce_kit_arguments(&mut args, &schema);
        assert_eq!(args["flag"], true);
        assert_eq!(args["other"], false);
    }

    #[test]
    fn coerce_leaves_unparseable_string_as_is() {
        let mut args = serde_json::json!({"limit": "not-a-number", "flag": "yes"});
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer"},
                "flag": {"type": "boolean"}
            }
        });
        coerce_kit_arguments(&mut args, &schema);
        assert_eq!(args["limit"], "not-a-number");
        assert_eq!(args["flag"], "yes");
    }

    #[test]
    fn coerce_passes_native_values_through() {
        let mut args = serde_json::json!({"limit": 7, "flag": false, "ratio": 1.5});
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer"},
                "flag": {"type": "boolean"},
                "ratio": {"type": "number"}
            }
        });
        coerce_kit_arguments(&mut args, &schema);
        assert_eq!(args["limit"], 7);
        assert_eq!(args["flag"], false);
        assert_eq!(args["ratio"], 1.5);
    }

    #[test]
    fn coerce_ignores_args_missing_from_schema() {
        let mut args = serde_json::json!({"limit": "3", "extra": "keep"});
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer"}
            }
        });
        coerce_kit_arguments(&mut args, &schema);
        assert_eq!(args["limit"], 3);
        assert_eq!(args["extra"], "keep");
    }

    #[test]
    fn coerce_handles_empty_or_null_schema() {
        let mut args = serde_json::json!({"limit": "3"});
        coerce_kit_arguments(&mut args, &Value::Null);
        assert_eq!(args["limit"], "3");

        coerce_kit_arguments(&mut args, &serde_json::json!({}));
        assert_eq!(args["limit"], "3");

        coerce_kit_arguments(&mut args, &serde_json::json!({"type": "object"}));
        assert_eq!(args["limit"], "3");
    }

    #[test]
    fn coerce_string_to_array() {
        let mut args = serde_json::json!({"tags": "[\"a\",\"b\"]"});
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "tags": {"type": "array"}
            }
        });
        coerce_kit_arguments(&mut args, &schema);
        assert_eq!(args["tags"], serde_json::json!(["a", "b"]));
    }

    fn kit_state(kit: LoadedKit) -> KitState {
        KitState {
            kit,
            connection: None,
            failure_count: 0,
            unhealthy: false,
            last_failure_at: None,
            unsent_calls: 0,
        }
    }

    fn loaded_kit(name: &str, eager: Option<bool>) -> LoadedKit {
        // Use /bin/echo as a real binary so the healthy kit isn't pre-marked unhealthy
        let cmd = if name == "broken" {
            "definitely-missing-kit-binary"
        } else {
            "/bin/echo"
        };
        loaded_kit_argv(name, vec![cmd], eager)
    }

    fn loaded_kit_argv(name: &str, argv: Vec<&str>, eager: Option<bool>) -> LoadedKit {
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
                eager,
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
