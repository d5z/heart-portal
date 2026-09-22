//! MCP tools `portal_subagent_*` — delegate work to the being's sub-agent.
//!
//! Five tools (PRD §6): `spawn` starts a task and returns immediately,
//! `status` shows what is happening, `log` is the pullable transcript,
//! `control` steers, follows up, or cancels, and `setup` lets the being pick
//! the sub-agent's LLM provider, model and key without touching the server.
//!
//! The being never receives a result here: completion arrives in their inbox
//! through Portal's callback. That asymmetry is the whole point — the being
//! and the sub-agent alternate instead of one blocking on the other.

use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::config::{SUBAGENT_PROVIDERS, SUBAGENT_THINKING_LEVELS};
use crate::subagent::{
    Budget, ModelConfigUpdate, SpawnRequest, SubagentManager, DEFAULT_LOG_LIMIT,
    DEFAULT_SESSION_KEY, SETUP_GUIDANCE,
};
use crate::tools::{value_as_u64, ToolInfo};

/// Hard floor/ceiling on a per-task wall clock, mirroring the tool schema.
const MIN_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 21_600;
/// A meaningful task needs at least a couple of turns and some tokens.
const MIN_MAX_TURNS: u32 = 1;
const MIN_MAX_TOKENS: u64 = 1_000;
/// `portal_process`-compatible long-poll cap.
const MAX_LOG_TIMEOUT_MS: u64 = 300_000;

pub const TOOL_NAMES: [&str; 5] = [
    "portal_subagent_spawn",
    "portal_subagent_status",
    "portal_subagent_log",
    "portal_subagent_control",
    "portal_subagent_setup",
];

pub fn is_subagent_tool(name: &str) -> bool {
    TOOL_NAMES.contains(&name)
}

/// Only `portal_subagent_setup`: advertised on its own when the sub-agent is
/// enabled but pi is not installed yet, because calling it is how pi gets
/// installed (see [`SubagentManager::ensure_pi_installed`]).
pub fn setup_tool() -> ToolInfo {
    list_tools()
        .into_iter()
        .find(|t| t.name == "portal_subagent_setup")
        .expect("portal_subagent_setup is declared in list_tools")
}

/// Tool declarations, added to `tools/list` only when the sub-agent is
/// available (enabled *and* pi resolvable).
pub fn list_tools() -> Vec<ToolInfo> {
    vec![
        ToolInfo {
            name: "portal_subagent_spawn".to_string(),
            description: "Delegate a self-contained task to your sub-agent. Returns immediately with a task_id; when the task finishes Portal notifies you through your inbox with the result — let go of it instead of polling. Tasks in the same `session` share one accumulating working memory (files, decisions, project knowledge); use one session per project or scene. Use this for work that needs judgement and many steps; use portal_exec for a single command.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "brief": {
                        "type": "string",
                        "description": "What to do. Self-contained; the sub-agent cannot ask you questions. Say what 'done' looks like."
                    },
                    "session": {
                        "type": "string",
                        "description": "Session key (e.g. scene or project id). Default \"default\". Same key ⇒ same accumulated working memory."
                    },
                    "workdir": {
                        "type": "string",
                        "description": "Working directory, inside the workspace root. Default: workspace root (or the session's existing cwd, which is fixed once set)."
                    },
                    "budget": {
                        "type": "object",
                        "properties": {
                            "max_turns": {"type": "integer", "minimum": 1},
                            "max_tokens": {"type": "integer", "minimum": 1000},
                            "timeout_secs": {"type": "integer", "minimum": 30, "maximum": 21600}
                        },
                        "description": "Caps for this task; defaults from portal.toml [subagent.budget]."
                    },
                    "model": {
                        "type": "string",
                        "description": "Override the model id for this session (provider comes from config)."
                    },
                    "thinking": {
                        "type": "string",
                        "enum": ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
                    },
                    "scene_id": {
                        "type": "string",
                        "description": "Opaque id echoed back in the callback as result.scene_id."
                    }
                },
                "required": ["brief"]
            }),
        },
        ToolInfo {
            name: "portal_subagent_status".to_string(),
            description: "What your sub-agent is doing: daemon health, sessions, running tasks (elapsed, turns, last tool) and recently finished ones. Omit task_id for the whole picture; pass one for the full row including the head of its result.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "string", "description": "One task. Omit for all."},
                    "session": {"type": "string", "description": "Filter to one session key."}
                }
            }),
        },
        ToolInfo {
            name: "portal_subagent_log".to_string(),
            description: "Transcript of a task's session: tool calls, assistant messages, compactions. Same paging as portal_process — use the returned next_offset, and timeout_ms to wait for new lines. Optional: the result comes to your inbox on its own.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "string"},
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "UTF-8 byte offset; use the returned next_offset."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 4,
                        "description": "Max bytes (default 65536)."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Wait up to this long for new lines (default 0 = return immediately, max 300000)."
                    }
                },
                "required": ["task_id"]
            }),
        },
        ToolInfo {
            name: "portal_subagent_control".to_string(),
            description: "Intervene in a running task. 'steer' injects guidance after the current turn; 'follow_up' adds work to run once the task would otherwise finish; 'cancel' ends it deliberately, returns the partial result, and suppresses the completion notification (like portal_process kill).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["steer", "follow_up", "cancel"]
                    },
                    "task_id": {"type": "string"},
                    "message": {
                        "type": "string",
                        "description": "steer: inject after the current turn; follow_up: run after the task would otherwise finish. Not used by cancel."
                    }
                },
                "required": ["action", "task_id"]
            }),
        },
        ToolInfo {
            name: "portal_subagent_setup".to_string(),
            description: "Configure your sub-agent's LLM provider, model, and API key. Run without arguments to see current config (key is masked). Required before first use if no provider is configured. Pass an empty string to clear a field. If the pi agent binary is not installed yet, this installs Portal's pinned copy first (needs npm).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "provider": {
                        "type": "string",
                        "description": "LLM provider: anthropic, openrouter, openai, gemini, groq, xai"
                    },
                    "model": {
                        "type": "string",
                        "description": "Model id, e.g. claude-sonnet-4-5, deepseek/deepseek-chat"
                    },
                    "api_key": {
                        "type": "string",
                        "description": "API key for the provider. Stored in portal.toml, never logged."
                    },
                    "thinking": {
                        "type": "string",
                        "description": "Thinking level: off, minimal, low, medium, high, xhigh, max. Default: medium"
                    }
                }
            }),
        },
    ]
}

/// Dispatch one `portal_subagent_*` call.
pub async fn handle(
    manager: &Arc<SubagentManager>,
    tool_name: &str,
    arguments: Value,
) -> Result<Value> {
    let result = match tool_name {
        "portal_subagent_spawn" => spawn(manager, arguments).await,
        "portal_subagent_status" => status(manager, arguments).await,
        "portal_subagent_log" => log(manager, arguments).await,
        "portal_subagent_control" => control(manager, arguments).await,
        "portal_subagent_setup" => setup(manager, arguments).await,
        other => anyhow::bail!("Unknown tool: {other}"),
    };

    // Being-facing failures are tool errors with an explanation, not transport
    // errors: the being can read them and choose differently.
    Ok(match result {
        Ok(value) => ok(&value)?,
        Err(e) => err(&format!("{e:#}")),
    })
}

async fn spawn(manager: &Arc<SubagentManager>, arguments: Value) -> Result<Value> {
    let brief = arguments
        .get("brief")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'brief'"))?
        .to_string();

    let session = arguments
        .get("session")
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_SESSION_KEY)
        .to_string();

    let workdir = arguments
        .get("workdir")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let budget = parse_budget(manager.default_budget(), arguments.get("budget"))?;

    let req = SpawnRequest {
        brief,
        session,
        workdir,
        budget,
        model: arguments
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        thinking: arguments
            .get("thinking")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        scene_id: arguments
            .get("scene_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    };

    Ok(manager.spawn(req).await?.to_json())
}

async fn status(manager: &Arc<SubagentManager>, arguments: Value) -> Result<Value> {
    let task_id = arguments.get("task_id").and_then(|v| v.as_str());
    let session = arguments.get("session").and_then(|v| v.as_str());
    manager.status(task_id, session).await
}

async fn log(manager: &Arc<SubagentManager>, arguments: Value) -> Result<Value> {
    let task_id = arguments
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'task_id'"))?;
    let offset = arguments.get("offset").and_then(value_as_u64).unwrap_or(0);
    let limit = arguments
        .get("limit")
        .and_then(value_as_u64)
        .unwrap_or(DEFAULT_LOG_LIMIT as u64)
        .max(4) as usize;
    let timeout_ms = arguments
        .get("timeout_ms")
        .and_then(value_as_u64)
        .unwrap_or(0)
        .min(MAX_LOG_TIMEOUT_MS);

    manager.log(task_id, offset, limit, timeout_ms).await
}

async fn control(manager: &Arc<SubagentManager>, arguments: Value) -> Result<Value> {
    let action = arguments
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'action'"))?;
    let task_id = arguments
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'task_id'"))?;

    match action {
        "steer" | "follow_up" => {
            let message = arguments
                .get("message")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("Missing 'message' for {action}"))?;
            manager
                .steer(task_id, message, action == "follow_up")
                .await?;
            Ok(json!({ "ok": true, "queued": action }))
        }
        "cancel" => manager.cancel(task_id).await,
        other => anyhow::bail!("Unknown action '{other}' (expected steer, follow_up or cancel)"),
    }
}

/// `portal_subagent_setup`: show `[subagent.model]` (key masked) when called
/// bare, otherwise merge the given fields, persist, and switch over.
async fn setup(manager: &Arc<SubagentManager>, arguments: Value) -> Result<Value> {
    // A non-string where a string belongs is a caller error, not "unset".
    fn string_field(arguments: &Value, key: &str) -> Result<Option<String>> {
        match arguments.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(other) => anyhow::bail!("'{key}' must be a string, got {other}"),
        }
    }

    let update = ModelConfigUpdate {
        provider: string_field(&arguments, "provider")?,
        model: string_field(&arguments, "model")?,
        api_key: string_field(&arguments, "api_key")?,
        thinking: string_field(&arguments, "thinking")?,
    };

    // pi first: without the binary no amount of model configuration helps.
    // The install is attempted (never silently skipped) and its failure is
    // reported alongside whatever else this call did, so the being learns
    // both facts at once instead of over two round trips.
    let install_error = match manager.ensure_pi_installed().await {
        Ok(_) => None,
        Err(e) => Some(format!("{e:#}")),
    };
    let pi_installed = manager.pi_installed();

    if update.is_empty() {
        let model = manager.model_config();
        let needs_setup = manager.needs_setup();
        return Ok(json!({
            "current": model.to_status_json(),
            "ready": !needs_setup && pi_installed,
            "pi_installed": pi_installed,
            "install_error": install_error,
            "supported_providers": SUBAGENT_PROVIDERS,
            "thinking_levels": SUBAGENT_THINKING_LEVELS,
            "hint": if needs_setup {
                SETUP_GUIDANCE
            } else if !pi_installed {
                "The pi agent binary is not installed and could not be installed \
                 automatically; see install_error."
            } else {
                "Configured. Call with provider/model/api_key/thinking to change; \
                 pass \"\" to clear a field."
            },
        }));
    }

    let outcome = manager.configure_model(update).await?;
    let mut note = if pi_installed {
        String::from("Sub-agent configured. You can now use portal_subagent_spawn.")
    } else {
        String::from(
            "Model saved, but the pi agent binary is not installed so portal_subagent_spawn \
             is not available yet; see install_error.",
        )
    };
    if outcome.daemon_restarted {
        note.push_str(" The pi daemon was restarted so the new credentials take effect; \
             existing sessions resume from their files on the next spawn.");
    }
    Ok(json!({
        "ok": true,
        "current": outcome.model.to_status_json(),
        "daemon_restarted": outcome.daemon_restarted,
        "pi_installed": pi_installed,
        "install_error": install_error,
        "note": note,
    }))
}

/// Per-task budget: config defaults with the being's overrides, clamped to the
/// schema's limits so a typo cannot pin the machine for six hours.
fn parse_budget(default: Budget, raw: Option<&Value>) -> Result<Budget> {
    let Some(obj) = raw else {
        return Ok(default);
    };
    if obj.is_null() {
        return Ok(default);
    }
    let obj = obj
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("'budget' must be an object"))?;

    let mut budget = default;
    if let Some(v) = obj.get("max_turns").and_then(value_as_u64) {
        budget.max_turns = (v as u32).max(MIN_MAX_TURNS);
    }
    if let Some(v) = obj.get("max_tokens").and_then(value_as_u64) {
        budget.max_tokens = v.max(MIN_MAX_TOKENS);
    }
    if let Some(v) = obj.get("timeout_secs").and_then(value_as_u64) {
        budget.timeout_secs = v.clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS);
    }
    Ok(budget)
}

fn ok(value: &Value) -> Result<Value> {
    Ok(json!({
        "content": [{ "type": "text", "text": serde_json::to_string_pretty(value)? }],
        "isError": false
    }))
}

fn err(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PortalConfig;
    use crate::heart_callback::HeartCallback;
    use std::path::PathBuf;

    fn temp_workspace(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("portal-subtool-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn base_config(workspace: &PathBuf) -> PortalConfig {
        let mut config = PortalConfig::default();
        config.security.workspace_root = workspace.clone();
        config.subagent.state_dir = Some(workspace.join("state").display().to_string());
        // A real binary that is not pi: enough to exercise validation paths
        // without spawning a daemon.
        config.subagent.command = Some(vec!["/bin/sh".to_string()]);
        // Independent of whatever `*_API_KEY` this machine exports.
        config.subagent.env_passthrough = vec!["PATH".to_string(), "HOME".to_string()];
        config
    }

    /// A configured manager: these tests are about what happens after setup.
    fn manager(workspace: &PathBuf) -> Arc<SubagentManager> {
        let mut config = base_config(workspace);
        config.subagent.model.provider = Some("anthropic".to_string());
        SubagentManager::new(&config, HeartCallback::new())
    }

    /// Fresh install: a portal.toml on disk, nothing in `[subagent.model]`.
    fn unconfigured_manager(workspace: &PathBuf) -> (Arc<SubagentManager>, PathBuf) {
        let path = workspace.join("portal.toml");
        std::fs::write(&path, "name = \"vale\"\n").unwrap();
        let mut config = base_config(workspace);
        config.config_path = Some(path.clone());
        (SubagentManager::new(&config, HeartCallback::new()), path)
    }

    fn text_of(response: &Value) -> String {
        response["content"][0]["text"].as_str().unwrap().to_string()
    }

    fn parsed(response: &Value) -> Value {
        serde_json::from_str(&text_of(response)).unwrap()
    }

    #[test]
    fn five_tools_are_declared_with_required_fields() {
        let tools = list_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, TOOL_NAMES.to_vec());
        for tool in &tools {
            assert!(tool.name.starts_with("portal_subagent_"));
            assert!(!tool.description.is_empty());
            assert_eq!(tool.input_schema["type"], "object");
        }
        let spawn = &tools[0];
        assert_eq!(spawn.input_schema["required"][0], "brief");
        assert!(spawn.description.contains("inbox"), "the being must be told where the result goes");
        let control = &tools[3];
        let actions = control.input_schema["properties"]["action"]["enum"]
            .as_array()
            .unwrap();
        assert_eq!(actions.len(), 3);
        let setup = &tools[4];
        assert_eq!(setup.name, "portal_subagent_setup");
        assert!(setup.input_schema.get("required").is_none(), "bare call shows status");
        for field in ["provider", "model", "api_key", "thinking"] {
            assert_eq!(setup.input_schema["properties"][field]["type"], "string", "{field}");
        }
        assert!(setup.description.contains("masked"));
    }

    #[tokio::test]
    async fn spawn_before_setup_returns_guidance_not_a_provider_error() {
        let ws = temp_workspace("firstuse");
        let (m, _) = unconfigured_manager(&ws);
        let resp = handle(&m, "portal_subagent_spawn", json!({"brief": "do it"}))
            .await
            .unwrap();
        assert_eq!(resp["isError"], true);
        let text = text_of(&resp);
        assert!(text.starts_with("Sub-agent is not configured yet."), "{text}");
        assert!(text.contains("portal_subagent_setup(provider=\"anthropic\""), "{text}");
        assert!(text.contains("Supported providers: anthropic, openrouter, openai, gemini, groq, xai"), "{text}");
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn setup_without_arguments_shows_status_with_the_key_masked() {
        let ws = temp_workspace("setupstatus");
        let (m, _) = unconfigured_manager(&ws);

        let resp = handle(&m, "portal_subagent_setup", json!({})).await.unwrap();
        assert_eq!(resp["isError"], false);
        let status = parsed(&resp);
        assert_eq!(status["ready"], false);
        assert_eq!(status["current"]["configured"], false);
        assert!(status["current"]["provider"].is_null());
        assert!(status["current"]["api_key"].is_null());
        assert!(status["hint"].as_str().unwrap().contains("portal_subagent_setup"));
        assert_eq!(status["supported_providers"][0], "anthropic");

        // Configure, then look again: the key is recognisable but unusable.
        let secret = "sk-ant-api03-status-test-secret-value";
        handle(
            &m,
            "portal_subagent_setup",
            json!({"provider": "anthropic", "model": "claude-sonnet-4-5", "api_key": secret}),
        )
        .await
        .unwrap();
        let resp = handle(&m, "portal_subagent_setup", json!({})).await.unwrap();
        let status = parsed(&resp);
        assert_eq!(status["ready"], true);
        assert_eq!(status["current"]["provider"], "anthropic");
        assert_eq!(status["current"]["model"], "claude-sonnet-4-5");
        assert_eq!(status["current"]["api_key"], "sk-ant-...***");
        assert!(!text_of(&resp).contains(secret), "raw key must never come back");

        // Nor through portal_subagent_status.
        let resp = handle(&m, "portal_subagent_status", json!({})).await.unwrap();
        assert!(!text_of(&resp).contains(secret), "{resp}");
        assert_eq!(parsed(&resp)["auth"]["api_key"], "sk-ant-...***");
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn setup_with_arguments_persists_and_unlocks_spawn() {
        let ws = temp_workspace("setupargs");
        let (m, path) = unconfigured_manager(&ws);
        let secret = "sk-or-v1-setup-args-secret-value";

        let resp = handle(
            &m,
            "portal_subagent_setup",
            json!({
                "provider": "openrouter",
                "model": "deepseek/deepseek-chat",
                "api_key": secret,
                "thinking": "low"
            }),
        )
        .await
        .unwrap();
        assert_eq!(resp["isError"], false, "{resp}");
        let body = parsed(&resp);
        assert_eq!(body["ok"], true);
        assert!(body["note"].as_str().unwrap().contains("portal_subagent_spawn"));
        assert_eq!(body["current"]["provider"], "openrouter");
        assert_eq!(body["current"]["thinking"], "low");
        assert_eq!(body["current"]["api_key"], "sk-or-v...***");
        assert!(!text_of(&resp).contains(secret));

        // Persisted where Portal loaded its config from, around existing keys.
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("name = \"vale\"\n"), "{written}");
        let reloaded = PortalConfig::load(path.to_str().unwrap()).unwrap();
        assert_eq!(reloaded.subagent.model.provider.as_deref(), Some("openrouter"));
        assert_eq!(reloaded.subagent.model.model.as_deref(), Some("deepseek/deepseek-chat"));
        assert_eq!(reloaded.subagent.model.thinking.as_deref(), Some("low"));
        assert_eq!(reloaded.subagent.model.api_key.as_deref(), Some(secret));

        // Spawn now gets past the first-use gate to ordinary validation.
        assert!(!m.needs_setup());
        let resp = handle(
            &m,
            "portal_subagent_spawn",
            json!({"brief": "do it", "workdir": "/etc"}),
        )
        .await
        .unwrap();
        assert_eq!(resp["isError"], true);
        assert!(text_of(&resp).contains("outside the workspace root"), "{resp}");
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn setup_rejects_bad_input_without_writing() {
        let ws = temp_workspace("setupbad");
        let (m, path) = unconfigured_manager(&ws);
        let before = std::fs::read_to_string(&path).unwrap();

        let resp = handle(&m, "portal_subagent_setup", json!({"api_key": "sk-lonely-key"}))
            .await
            .unwrap();
        assert_eq!(resp["isError"], true);
        assert!(text_of(&resp).contains("needs a provider"), "{resp}");

        let resp = handle(&m, "portal_subagent_setup", json!({"provider": "hal9000"}))
            .await
            .unwrap();
        assert_eq!(resp["isError"], true);
        assert!(text_of(&resp).contains("unknown provider 'hal9000'"), "{resp}");

        let resp = handle(&m, "portal_subagent_setup", json!({"provider": 42}))
            .await
            .unwrap();
        assert_eq!(resp["isError"], true);
        assert!(text_of(&resp).contains("'provider' must be a string"), "{resp}");

        let resp = handle(&m, "portal_subagent_setup", json!({"provider": "groq", "thinking": "hard"}))
            .await
            .unwrap();
        assert_eq!(resp["isError"], true);
        assert!(text_of(&resp).contains("unknown thinking level"), "{resp}");

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "nothing written");
        assert!(m.needs_setup());
        let _ = std::fs::remove_dir_all(ws);
    }

    #[test]
    fn tool_name_recognition_is_exact() {
        assert!(is_subagent_tool("portal_subagent_spawn"));
        assert!(is_subagent_tool("portal_subagent_control"));
        assert!(!is_subagent_tool("portal_subagent_session"));
        assert!(!is_subagent_tool("portal_exec"));
    }

    #[test]
    fn budget_overrides_are_clamped_to_the_schema() {
        let default = Budget {
            max_turns: 40,
            max_tokens: 400_000,
            timeout_secs: 1800,
            max_continuations: 3,
        };
        assert_eq!(parse_budget(default, None).unwrap(), default);
        assert_eq!(parse_budget(default, Some(&Value::Null)).unwrap(), default);

        let b = parse_budget(
            default,
            Some(&json!({"max_turns": 5, "max_tokens": 50_000, "timeout_secs": 120})),
        )
        .unwrap();
        assert_eq!(b.max_turns, 5);
        assert_eq!(b.max_tokens, 50_000);
        assert_eq!(b.timeout_secs, 120);
        // Unspecified fields keep the configured default.
        assert_eq!(b.max_continuations, 3);

        let clamped = parse_budget(
            default,
            Some(&json!({"max_turns": 0, "max_tokens": 1, "timeout_secs": 999_999})),
        )
        .unwrap();
        assert_eq!(clamped.max_turns, MIN_MAX_TURNS);
        assert_eq!(clamped.max_tokens, MIN_MAX_TOKENS);
        assert_eq!(clamped.timeout_secs, MAX_TIMEOUT_SECS);

        // Strings coerce, per the HF-7 argument policy.
        let coerced = parse_budget(default, Some(&json!({"timeout_secs": "300"}))).unwrap();
        assert_eq!(coerced.timeout_secs, 300);

        assert!(parse_budget(default, Some(&json!("nope"))).is_err());
    }

    #[tokio::test]
    async fn spawn_requires_a_brief() {
        let ws = temp_workspace("nobrief");
        let m = manager(&ws);
        let resp = handle(&m, "portal_subagent_spawn", json!({})).await.unwrap();
        assert_eq!(resp["isError"], true);
        assert!(text_of(&resp).contains("Missing 'brief'"), "{resp}");
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn being_facing_failures_come_back_as_tool_errors() {
        let ws = temp_workspace("toolerr");
        let m = manager(&ws);

        // Outside the workspace: refused, but readable.
        let resp = handle(
            &m,
            "portal_subagent_spawn",
            json!({"brief": "do it", "workdir": "/etc"}),
        )
        .await
        .unwrap();
        assert_eq!(resp["isError"], true);
        assert!(text_of(&resp).contains("outside the workspace root"), "{resp}");

        // Unknown task: an error the being can act on, not a transport failure.
        let resp = handle(&m, "portal_subagent_log", json!({"task_id": "sub_nope"}))
            .await
            .unwrap();
        assert_eq!(resp["isError"], true);
        assert!(text_of(&resp).contains("unknown task"), "{resp}");
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn status_returns_json_text_content() {
        let ws = temp_workspace("statusjson");
        let m = manager(&ws);
        let resp = handle(&m, "portal_subagent_status", json!({}))
            .await
            .unwrap();
        assert_eq!(resp["isError"], false);
        let parsed: Value = serde_json::from_str(&text_of(&resp)).unwrap();
        assert_eq!(parsed["available"], true);
        assert_eq!(parsed["daemon"]["running"], false);
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn control_validates_action_and_message() {
        let ws = temp_workspace("control");
        let m = manager(&ws);

        let resp = handle(
            &m,
            "portal_subagent_control",
            json!({"action": "nope", "task_id": "sub_1"}),
        )
        .await
        .unwrap();
        assert_eq!(resp["isError"], true);
        assert!(text_of(&resp).contains("Unknown action"), "{resp}");

        let resp = handle(
            &m,
            "portal_subagent_control",
            json!({"action": "steer", "task_id": "sub_1"}),
        )
        .await
        .unwrap();
        assert_eq!(resp["isError"], true);
        assert!(text_of(&resp).contains("Missing 'message'"), "{resp}");

        let resp = handle(&m, "portal_subagent_control", json!({"task_id": "sub_1"}))
            .await
            .unwrap();
        assert_eq!(resp["isError"], true);
        assert!(text_of(&resp).contains("Missing 'action'"), "{resp}");
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn unknown_tool_names_are_rejected() {
        let ws = temp_workspace("unknowntool");
        let m = manager(&ws);
        assert!(handle(&m, "portal_subagent_session", json!({}))
            .await
            .is_err());
        let _ = std::fs::remove_dir_all(ws);
    }
}
