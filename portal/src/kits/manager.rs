use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::mcp::{McpConnection, McpServerConfig};
use crate::tools::ToolInfo;

use super::loader::LoadedKit;

const MAX_FAILURES: u8 = 3;

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
            states.insert(
                name,
                KitState {
                    kit,
                    connection: None,
                    failure_count: 0,
                    unhealthy: false,
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
            for tool in &state.kit.manifest.tools {
                tools.push(ToolInfo {
                    name: format!("{}_{}", state.kit.manifest.name, tool.name),
                    description: tool.description.clone(),
                    input_schema: tool.params.clone(),
                });
            }
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
        anyhow::bail!("Kit '{}' is unhealthy", state.kit.manifest.name);
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
            "Kit '{}' is unhealthy after {} failed restart attempts",
            state.kit.manifest.name,
            state.failure_count
        );
    }

    info!(
        "Spawning kit '{}' with command: {:?}",
        state.kit.manifest.name, state.kit.command
    );

    let config = McpServerConfig {
        name: state.kit.manifest.name.clone(),
        command: state.kit.command.clone(),
        env: kit_env(state),
        cwd: Some(state.kit.kit_dir.clone()),
    };

    match McpConnection::spawn(config).await {
        Ok(connection) => {
            debug!("Kit '{}' spawned", state.kit.manifest.name);
            state.connection = Some(connection);
            Ok(())
        }
        Err(err) => {
            record_failure(state);
            Err(err).with_context(|| format!("Failed to spawn kit '{}'", state.kit.manifest.name))
        }
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
        "running"
    } else {
        "not running"
    }
}
