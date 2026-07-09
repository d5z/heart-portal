use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::mcp::{McpConnection, McpServerConfig};
use crate::tools::ToolInfo;

use super::loader::{format_command, LoadedKit};

const MAX_FAILURES: u8 = 3;
const SPAWN_TIMEOUT_SECS: u64 = 10;

#[derive(Clone)]
pub struct KitManager {
    kits: Arc<Mutex<BTreeMap<String, KitState>>>,
}

struct KitState {
    kit: LoadedKit,
    connection: Option<McpConnection>,
    failure_count: u8,
    unhealthy: bool,
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

        let result = state
            .connection
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Kit '{}' process is not running", kit_name))?
            .call_tool(tool_name, arguments)
            .await;

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
            if let Some(mut connection) = state.connection.take() {
                if let Err(err) = connection.shutdown().await {
                    warn!("Failed to shut down kit '{}': {}", name, err);
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
}

async fn ensure_connection(state: &mut KitState) -> Result<()> {
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
        Duration::from_secs(SPAWN_TIMEOUT_SECS),
        McpConnection::spawn(config),
    )
    .await
    {
        Ok(Ok(connection)) => {
            debug!("Kit '{}' spawned", kit_name);
            state.connection = Some(connection);
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
            record_failure(state);
            state.unhealthy = true;
            let message = format!(
                "Kit '{}' failed to start: timed out after {} seconds. Command: {}. Check that the command exists and the MCP server implements the stdio protocol.",
                kit_name,
                SPAWN_TIMEOUT_SECS,
                command_text
            );
            warn!("{}", message);
            Err(anyhow::anyhow!("{}", message))
        }
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
    if let Some(mut connection) = state.connection.take() {
        if let Err(err) = connection.shutdown().await {
            warn!(
                "Failed to shut down kit '{}' connection: {}",
                state.kit.manifest.name, err
            );
        }
    }
}

fn record_failure(state: &mut KitState) {
    state.failure_count = state.failure_count.saturating_add(1);
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

    fn loaded_kit(name: &str) -> LoadedKit {
        // Use /bin/echo as a real binary so the healthy kit isn't pre-marked unhealthy
        let cmd = if name == "broken" {
            "definitely-missing-kit-binary".to_string()
        } else {
            "/bin/echo".to_string()
        };
        LoadedKit {
            manifest: KitManifest {
                name: name.to_string(),
                version: "0.1.0".to_string(),
                description: None,
                author: None,
                platform: None,
                runtime: None,
                command: vec![cmd.clone()],
                tools: vec![KitToolDef {
                    name: "ping".to_string(),
                    description: "Ping".to_string(),
                    params: serde_json::json!({"type": "object"}),
                }],
                permissions: None,
                workspace: None,
            },
            kit_dir: PathBuf::from("/tmp"),
            command: vec![cmd],
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
