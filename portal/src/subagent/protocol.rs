//! Pi daemon JSONL protocol, v7 subset (PRD §3.3 — normative).
//!
//! Wire format: newline-delimited JSON, UTF-8, one object per line.
//!
//! ```text
//! connect  ◄── {"type":"daemon_hello", protocol:{name,version}, schemaId, appVersion, …}
//! request  ──► {"id":"<uuid>","type":"<command>", …fields}
//! response ◄── {"id":"<uuid>","type":"response","command":"…","success":true,"data":…}
//! events   ◄── {"type":"session_event","activeSessionId":"…","event":{…}}
//!              {"type":"session_closed",…} {"type":"extension_ui_request",…}
//!              {"type":"daemon_closing",…}  anything else → ignored
//! ```
//!
//! Deserialization is deliberately tolerant: unknown message types collapse to
//! [`DaemonMessage::Unknown`] and unknown fields are ignored, so a pi upgrade
//! that adds messages or fields cannot break Portal (PRD §9 risk 1).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Protocol name Portal asserts on `daemon_hello`.
pub const DAEMON_PROTOCOL_NAME: &str = "prime-agent.daemon";
/// Protocol version Portal implements. A mismatch is a hard error.
pub const DAEMON_PROTOCOL_VERSION: u32 = 7;
/// pi version Portal is written against; a mismatch is a warning only.
pub const PINNED_PI_VERSION: &str = "0.7.2";
/// Capability required for `prompt` / `steer` / `follow_up` to be admitted.
pub const CAP_SESSION_INPUT_ADMISSION: &str = "session_input_admission";
/// Max bytes in a single protocol line (mirrors pi's own cap).
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

// ── greeting ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: u32,
}

/// First line the daemon writes after a client connects.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonHello {
    #[serde(default = "unknown_protocol")]
    pub protocol: ProtocolInfo,
    #[serde(default)]
    pub schema_id: Option<String>,
    #[serde(default)]
    pub app_version: Option<String>,
    #[serde(default)]
    pub supervisor_pid: Option<u32>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub server_capabilities: Vec<String>,
}

fn unknown_protocol() -> ProtocolInfo {
    ProtocolInfo {
        name: String::new(),
        version: 0,
    }
}

impl DaemonHello {
    /// Hard gate: the protocol Portal speaks, or refuse to use this daemon.
    pub fn check_protocol(&self) -> anyhow::Result<()> {
        if self.protocol.name != DAEMON_PROTOCOL_NAME {
            anyhow::bail!(
                "unexpected daemon protocol '{}' (want '{}')",
                self.protocol.name,
                DAEMON_PROTOCOL_NAME
            );
        }
        if self.protocol.version != DAEMON_PROTOCOL_VERSION {
            anyhow::bail!(
                "pi daemon speaks protocol v{} but Portal implements v{}; \
                 upgrade Portal or pin pi {}",
                self.protocol.version,
                DAEMON_PROTOCOL_VERSION,
                PINNED_PI_VERSION
            );
        }
        Ok(())
    }

    pub fn supports(&self, capability: &str) -> bool {
        self.server_capabilities.iter().any(|c| c == capability)
    }

    /// A stdio/degraded transport that never sent a hello.
    pub fn unverified() -> Self {
        Self {
            protocol: ProtocolInfo {
                name: DAEMON_PROTOCOL_NAME.to_string(),
                version: DAEMON_PROTOCOL_VERSION,
            },
            schema_id: None,
            app_version: None,
            supervisor_pid: None,
            client_id: None,
            server_capabilities: Vec::new(),
        }
    }
}

// ── inbound ─────────────────────────────────────────────────────────

/// Reply to a request, correlated by `id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandResponse {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub success: bool,
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub error_info: Option<Value>,
}

impl CommandResponse {
    /// `data` on success, an `anyhow` error carrying the daemon's message otherwise.
    pub fn into_data(self) -> anyhow::Result<Value> {
        if self.success {
            return Ok(self.data.unwrap_or(Value::Null));
        }
        let msg = self
            .error
            .unwrap_or_else(|| "daemon reported failure without an error message".to_string());
        anyhow::bail!("pi {} failed: {}", self.command, msg)
    }
}

/// The `event` object inside a `session_event`. Loosely typed on purpose: pi
/// emits dozens of event kinds and adds more between releases.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEventBody {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

impl SessionEventBody {
    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.rest.get(key).and_then(|v| v.as_str())
    }

    pub fn u64_field(&self, key: &str) -> Option<u64> {
        self.rest.get(key).and_then(|v| v.as_u64())
    }

    pub fn f64_field(&self, key: &str) -> Option<f64> {
        self.rest.get(key).and_then(|v| v.as_f64())
    }
}

/// Progress notification for an attached session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEvent {
    #[serde(default)]
    pub active_session_id: String,
    pub event: SessionEventBody,
    #[serde(default)]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionClosed {
    #[serde(default)]
    pub active_session_id: String,
    #[serde(default)]
    pub reason: String,
}

/// A dialog the sub-agent wants a human to answer. Portal always declines
/// (PRD §9 risk 3) so a headless sub can never block forever.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionUiRequest {
    #[serde(default)]
    pub active_session_id: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub payload: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonClosing {
    #[serde(default)]
    pub reason: String,
}

/// Anything the daemon can write on the socket.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonMessage {
    #[serde(rename = "daemon_hello")]
    Hello(DaemonHello),
    #[serde(rename = "response")]
    Response(CommandResponse),
    #[serde(rename = "session_event")]
    SessionEvent(SessionEvent),
    #[serde(rename = "session_closed")]
    SessionClosed(SessionClosed),
    #[serde(rename = "extension_ui_request")]
    ExtensionUiRequest(ExtensionUiRequest),
    #[serde(rename = "daemon_closing")]
    DaemonClosing(DaemonClosing),
    /// Snapshot streams, telemetry, future message kinds — logged at debug.
    #[serde(other)]
    Unknown,
}

// ── outbound ────────────────────────────────────────────────────────

/// Session lifecycle. `resident` sessions survive client disconnects, which is
/// what makes a Portal restart non-destructive.
pub const LIFECYCLE_RESIDENT: &str = "resident";

/// `create.config` — a subset of pi's `AgentSessionRuntimeConfig`.
/// Only fields Portal actually sets are present; `skip_serializing_if` keeps
/// the line free of nulls so pi's own defaults apply.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRuntimeConfig {
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub append_system_prompt: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<String>,
    pub no_context_files: bool,
    pub telemetry_disabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autonomous: Option<AutonomousConfig>,
}

/// Pi's own budget primitives — Portal maps `[subagent.budget]` onto these
/// instead of re-implementing turn/token accounting.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutonomousConfig {
    pub enabled: bool,
    pub max_turns: u32,
    pub max_tokens: u64,
    pub timeout_ms: u64,
    pub max_continuations: u32,
}

/// Every command Portal sends. Internally tagged as `type`, matching the
/// daemon's bare-JSON-line command form.
///
/// The full §3.3 subset is modelled even where Portal does not call it yet, so
/// the protocol surface is reviewable in one place.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    #[serde(rename_all = "camelCase")]
    Create {
        name: String,
        lifecycle: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_path: Option<String>,
        config: SessionRuntimeConfig,
    },
    #[serde(rename_all = "camelCase")]
    Attach {
        active_session_id: String,
        supports_extension_ui: bool,
        client_id: String,
        capabilities: Vec<String>,
    },
    #[serde(rename_all = "camelCase")]
    Detach { active_session_id: String },
    #[serde(rename_all = "camelCase")]
    PromptAndWait {
        active_session_id: String,
        message: String,
        source: String,
    },
    #[serde(rename_all = "camelCase")]
    Steer {
        active_session_id: String,
        message: String,
    },
    #[serde(rename_all = "camelCase")]
    FollowUp {
        active_session_id: String,
        message: String,
    },
    #[serde(rename_all = "camelCase")]
    Abort { active_session_id: String },
    #[serde(rename_all = "camelCase")]
    WaitForIdle { active_session_id: String },
    #[serde(rename_all = "camelCase")]
    WaitForHeadlessCompletion { active_session_id: String },
    #[serde(rename_all = "camelCase")]
    GetLastAssistantText { active_session_id: String },
    #[serde(rename_all = "camelCase")]
    GetSessionStats { active_session_id: String },
    #[serde(rename_all = "camelCase")]
    GetState { active_session_id: String },
    #[serde(rename_all = "camelCase")]
    GetAvailableModels { active_session_id: String },
    #[serde(rename_all = "camelCase")]
    ExtensionUiResponse {
        active_session_id: String,
        request_id: String,
        response: Value,
    },
    #[serde(rename_all = "camelCase")]
    Kill { active_session_id: String },
    List { all: bool },
    Shutdown { force: bool },
}

impl Command {
    /// The `command` string the daemon echoes back in its response.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Create { .. } => "create",
            Command::Attach { .. } => "attach",
            Command::Detach { .. } => "detach",
            Command::PromptAndWait { .. } => "prompt_and_wait",
            Command::Steer { .. } => "steer",
            Command::FollowUp { .. } => "follow_up",
            Command::Abort { .. } => "abort",
            Command::WaitForIdle { .. } => "wait_for_idle",
            Command::WaitForHeadlessCompletion { .. } => "wait_for_headless_completion",
            Command::GetLastAssistantText { .. } => "get_last_assistant_text",
            Command::GetSessionStats { .. } => "get_session_stats",
            Command::GetState { .. } => "get_state",
            Command::GetAvailableModels { .. } => "get_available_models",
            Command::ExtensionUiResponse { .. } => "extension_ui_response",
            Command::Kill { .. } => "kill",
            Command::List { .. } => "list",
            Command::Shutdown { .. } => "shutdown",
        }
    }

    /// Serialize as a bare protocol line: command fields + correlation id.
    /// Used for stdio transport where the process reads commands directly.
    pub fn to_line(&self, id: &str) -> anyhow::Result<String> {
        let mut value = serde_json::to_value(self)?;
        let obj = value
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("command did not serialize to an object"))?;
        obj.insert("id".to_string(), Value::String(id.to_string()));
        Ok(serde_json::to_string(&value)?)
    }

    /// Serialize as a protocol-7 daemon command envelope.
    /// The daemon rejects bare commands with "Daemon commands require protocol envelope".
    pub fn to_envelope(&self, id: &str, client_id: &str) -> anyhow::Result<String> {
        let command = serde_json::to_value(self)?;
        let envelope = serde_json::json!({
            "type": "command",
            "id": id,
            "protocol": { "name": DAEMON_PROTOCOL_NAME, "version": DAEMON_PROTOCOL_VERSION },
            "clientId": client_id,
            "command": command
        });
        Ok(serde_json::to_string(&envelope)?)
    }

    /// `attach` with the flags Portal always uses: no UI, sequenced events.
    pub fn attach(active_session_id: impl Into<String>, client_id: impl Into<String>) -> Self {
        Command::Attach {
            active_session_id: active_session_id.into(),
            supports_extension_ui: false,
            client_id: client_id.into(),
            capabilities: vec!["event_sequence".to_string()],
        }
    }

    /// Decline a dialog so the sub-agent stops waiting on a human.
    pub fn decline_ui(active_session_id: impl Into<String>, request_id: impl Into<String>) -> Self {
        Command::ExtensionUiResponse {
            active_session_id: active_session_id.into(),
            request_id: request_id.into(),
            response: serde_json::json!({ "cancelled": true }),
        }
    }
}

/// One task, rendered as a prompt into a long-lived session (PRD §7.1).
#[derive(Debug, Clone)]
pub struct PromptRequest {
    pub active_session_id: String,
    pub message: String,
}

impl PromptRequest {
    pub fn into_command(self) -> Command {
        Command::PromptAndWait {
            active_session_id: self.active_session_id,
            message: self.message,
            source: "rpc".to_string(),
        }
    }
}

// ── response payloads ───────────────────────────────────────────────

/// `create` / `list` payload. Every field optional: pi's `SessionSummary`
/// carries far more than Portal reads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionSummary {
    pub active_session_id: String,
    pub session_id: Option<String>,
    pub session_file: Option<String>,
    pub name: Option<String>,
    pub cwd: Option<String>,
    /// pi ≥0.7 returns a full model object (api/baseUrl/id/name/provider/cost);
    /// older builds returned a bare id string. Portal never reads this field —
    /// accept any shape so `create` parsing does not break on it.
    pub model: Option<serde_json::Value>,
    pub is_streaming: bool,
    pub resumed: bool,
}

/// `get_state` payload.
#[allow(dead_code)] // read by portal_subagent_session list (PRD §6.5)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionStateInfo {
    pub is_streaming: bool,
    pub session_file: Option<String>,
    /// Same shape drift as `SessionSummary::model` — pi sends an object.
    pub model: Option<serde_json::Value>,
    pub message_count: Option<u64>,
    pub error_message: Option<String>,
}

/// Token/cost accounting, as reported by `get_session_stats` or the P3
/// `prompt_and_wait` envelope. Field names vary across pi versions, so
/// [`Usage::from_value`] probes several shapes.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub total: u64,
    pub cost_usd: Option<f64>,
}

impl Usage {
    /// Pull usage out of whatever shape the daemon produced. Missing numbers
    /// stay zero rather than failing a task that actually succeeded.
    pub fn from_value(v: &Value) -> Self {
        let root = ["usage", "tokens", "totals", "stats"]
            .iter()
            .find_map(|k| v.get(k))
            .unwrap_or(v);
        let pick = |keys: &[&str]| -> u64 {
            keys.iter()
                .find_map(|k| root.get(*k).and_then(|x| x.as_u64()))
                .unwrap_or(0)
        };
        let input = pick(&["input", "inputTokens", "input_tokens", "promptTokens"]);
        let output = pick(&["output", "outputTokens", "output_tokens", "completionTokens"]);
        let mut total = pick(&["total", "totalTokens", "total_tokens"]);
        if total == 0 {
            total = input + output;
        }
        let cost_usd = ["cost", "costUsd", "cost_usd", "totalCost"]
            .iter()
            .find_map(|k| root.get(*k).and_then(|x| x.as_f64()));
        Self {
            input,
            output,
            total,
            cost_usd,
        }
    }
}

/// `wait_for_headless_completion` payload — pi's `AgentAutonomousStatus`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AutonomousStatus {
    pub turns_used: u32,
    pub tokens_used: u64,
    pub last_gate_failure: Option<String>,
    pub stop_reason: Option<String>,
    pub limit_reached: Option<String>,
}

impl AutonomousStatus {
    /// True when the task stopped because it ran out of budget rather than
    /// because it finished the work (PRD §4.3 → `BudgetExhausted`).
    pub fn budget_exhausted(&self, budget_turns: u32, budget_tokens: u64) -> bool {
        if self.limit_reached.is_some() {
            return true;
        }
        if matches!(
            self.stop_reason.as_deref(),
            Some("max_turns") | Some("max_tokens") | Some("timeout") | Some("budget")
        ) {
            return true;
        }
        (budget_turns > 0 && self.turns_used >= budget_turns)
            || (budget_tokens > 0 && self.tokens_used >= budget_tokens)
    }
}

/// Result of one task. Populated from the P3 `prompt_and_wait` envelope when
/// present, otherwise assembled from the follow-up round trips (PRD §5 P3).
#[derive(Debug, Clone, Default)]
pub struct PromptComplete {
    pub last_assistant_text: Option<String>,
    pub stop_reason: Option<String>,
    pub error_message: Option<String>,
    pub usage: Usage,
    pub turns: u32,
}

impl PromptComplete {
    /// Read the atomic envelope if this daemon has P3; `None` means Portal must
    /// fall back to `get_last_assistant_text` + `get_session_stats`.
    ///
    /// The result body is what makes an envelope P3, so the `lastAssistantText`
    /// key must be *present* — a plain response that merely happens to carry a
    /// `stopReason` is not P3, and treating it as one would silently deliver an
    /// empty result instead of falling back.
    pub fn from_value(v: &Value) -> Option<Self> {
        let obj = v.as_object()?;
        let text_key = ["lastAssistantText", "last_assistant_text", "text"]
            .into_iter()
            .find(|k| obj.contains_key(*k))?;
        // Present but null is still P3: the task simply said nothing.
        let text = obj
            .get(text_key)
            .and_then(|x| x.as_str())
            .map(str::to_string);
        let stop_reason = ["stopReason", "stop_reason"]
            .iter()
            .find_map(|k| obj.get(*k).and_then(|x| x.as_str()))
            .map(str::to_string);
        let error_message = ["errorMessage", "error_message", "error"]
            .iter()
            .find_map(|k| obj.get(*k).and_then(|x| x.as_str()))
            .map(str::to_string);
        Some(Self {
            last_assistant_text: text,
            stop_reason,
            error_message,
            usage: Usage::from_value(v),
            turns: obj
                .get("turns")
                .and_then(|x| x.as_u64())
                .unwrap_or(0) as u32,
        })
    }

    /// Pi signalled an error rather than a completed task.
    pub fn failed(&self) -> bool {
        self.error_message.is_some()
            || matches!(self.stop_reason.as_deref(), Some("error") | Some("aborted"))
    }
}

// ── per-task stdio transport ────────────────────────────────────────

/// Result of one `pi --print --mode json` run, folded out of its stdout.
///
/// Not every pi has a daemon mode (0.73.1 has no `--daemon-socket`), so
/// Portal falls back to one pi process per task. That process speaks a
/// simpler, one-way dialect than the daemon: a JSON object per stdout line,
/// discriminated by `type`, ending in `agent_end`.
///
/// ```text
/// {"type":"session","id":"b13…","cwd":"/work"}
/// {"type":"agent_start"}
/// {"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"…"}],
///                                  "usage":{"input":12,"output":7,"totalTokens":19,"cost":{"total":0.002}},
///                                  "stopReason":"stop"}}
/// {"type":"turn_end","message":{…}}
/// {"type":"agent_end","messages":[…]}
/// ```
///
/// Anything unparseable is skipped rather than failing the task: pi prints
/// the odd non-JSON line (update notices, warnings) and a task that actually
/// did the work must not be reported as failed over a stray line.
#[derive(Debug, Clone, Default)]
pub struct StdioResult {
    pub last_assistant_text: Option<String>,
    pub stop_reason: Option<String>,
    pub error_message: Option<String>,
    pub usage: Usage,
    pub turns: u32,
    /// pi's own session id for the run, for cross-referencing its session file.
    pub session_id: Option<String>,
}

impl StdioResult {
    /// Fold a whole stdout capture. The runtime folds line by line as pi
    /// streams; this is the same thing for a capture you already have.
    #[allow(dead_code)]
    pub fn parse(stdout: &str) -> Self {
        let mut out = Self::default();
        for line in stdout.lines() {
            out.fold_line(line);
        }
        out
    }

    /// Fold one line, so a long run can be accounted for as it streams.
    pub fn fold_line(&mut self, line: &str) {
        let line = line.trim();
        if !line.starts_with('{') {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        match v.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
            "session" => {
                self.session_id = v.get("id").and_then(|i| i.as_str()).map(str::to_string);
            }
            "message_end" => {
                if let Some(message) = v.get("message") {
                    self.fold_message(message, true);
                }
            }
            "turn_end" => self.turns = self.turns.saturating_add(1),
            "agent_end" => {
                // Belt and braces: a pi that emits no per-message events (or a
                // capture we joined late) still carries everything here.
                if self.last_assistant_text.is_none() {
                    let count_usage = self.usage.total == 0;
                    if let Some(messages) = v.get("messages").and_then(|m| m.as_array()) {
                        for message in messages {
                            self.fold_message(message, count_usage);
                        }
                    }
                }
            }
            "error" => {
                if let Some(msg) = ["message", "error", "errorMessage"]
                    .iter()
                    .find_map(|k| v.get(*k).and_then(|x| x.as_str()))
                {
                    self.error_message = Some(msg.to_string());
                }
            }
            _ => {}
        }
    }

    /// Accumulate one message. Usage is summed across assistant messages
    /// because pi reports it per request, not per run.
    fn fold_message(&mut self, message: &Value, count_usage: bool) {
        if message.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            return;
        }
        if let Some(text) = assistant_text(message) {
            self.last_assistant_text = Some(text);
        }
        if let Some(reason) = message.get("stopReason").and_then(|r| r.as_str()) {
            self.stop_reason = Some(reason.to_string());
        }
        if let Some(error) = ["errorMessage", "error_message"]
            .iter()
            .find_map(|k| message.get(*k).and_then(|x| x.as_str()))
        {
            self.error_message = Some(error.to_string());
        }
        if count_usage {
            if let Some(usage) = message.get("usage") {
                self.add_usage(usage);
            }
        }
    }

    fn add_usage(&mut self, usage: &Value) {
        let one = Usage::from_value(usage);
        self.usage.input = self.usage.input.saturating_add(one.input);
        self.usage.output = self.usage.output.saturating_add(one.output);
        self.usage.total = self.usage.total.saturating_add(one.total);
        // pi nests the price under `cost.total`; flat shapes are handled by
        // `Usage::from_value` itself.
        let cost = one.cost_usd.or_else(|| {
            usage
                .get("cost")
                .and_then(|c| c.get("total"))
                .and_then(|t| t.as_f64())
        });
        if let Some(cost) = cost {
            self.usage.cost_usd = Some(self.usage.cost_usd.unwrap_or(0.0) + cost);
        }
    }

    /// pi reported an error rather than a finished task.
    pub fn failed(&self) -> bool {
        self.error_message.is_some()
            || matches!(self.stop_reason.as_deref(), Some("error") | Some("aborted"))
    }
}

/// The text blocks of an assistant message, joined. `None` for anyone else's
/// message (pi echoes the user's own prompt back as an event) and `None` when
/// the assistant said nothing, so a tool-only turn cannot overwrite a real
/// answer.
pub(super) fn assistant_text(message: &Value) -> Option<String> {
    if message.get("role").and_then(|r| r.as_str()) != Some("assistant") {
        return None;
    }
    let content = message.get("content")?;
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_accepts_v7_and_rejects_others() {
        let line = r#"{"type":"daemon_hello","protocol":{"name":"prime-agent.daemon","version":7},
            "schemaId":"rev16","appVersion":"0.7.2","supervisorPid":48213,"clientId":"c1",
            "serverCapabilities":["session_input_admission"],"futureField":123}"#;
        let msg: DaemonMessage = serde_json::from_str(line).unwrap();
        let DaemonMessage::Hello(hello) = msg else {
            panic!("expected hello");
        };
        assert!(hello.check_protocol().is_ok());
        assert!(hello.supports(CAP_SESSION_INPUT_ADMISSION));
        assert_eq!(hello.supervisor_pid, Some(48213));

        let old = r#"{"type":"daemon_hello","protocol":{"name":"prime-agent.daemon","version":6}}"#;
        let DaemonMessage::Hello(hello) = serde_json::from_str::<DaemonMessage>(old).unwrap() else {
            panic!("expected hello");
        };
        assert!(hello.check_protocol().is_err());
    }

    #[test]
    fn unknown_message_types_are_ignored_not_errors() {
        let msg: DaemonMessage =
            serde_json::from_str(r#"{"type":"snapshot_chunk","seq":4,"data":"…"}"#).unwrap();
        assert!(matches!(msg, DaemonMessage::Unknown));
    }

    #[test]
    fn session_event_keeps_unmodelled_fields() {
        let line = r#"{"type":"session_event","activeSessionId":"as_1",
            "event":{"type":"tool_execution_start","toolName":"bash","args":{"command":"ls"}},
            "meta":{"sequence":12}}"#;
        let DaemonMessage::SessionEvent(ev) = serde_json::from_str::<DaemonMessage>(line).unwrap()
        else {
            panic!("expected session_event");
        };
        assert_eq!(ev.active_session_id, "as_1");
        assert_eq!(ev.event.kind, "tool_execution_start");
        assert_eq!(ev.event.str_field("toolName"), Some("bash"));
    }

    #[test]
    fn response_failure_becomes_an_error() {
        let line = r#"{"id":"r1","type":"response","command":"prompt_and_wait","success":false,
            "error":"session is busy"}"#;
        let DaemonMessage::Response(resp) = serde_json::from_str::<DaemonMessage>(line).unwrap()
        else {
            panic!("expected response");
        };
        let err = resp.into_data().unwrap_err().to_string();
        assert!(err.contains("prompt_and_wait"), "{err}");
        assert!(err.contains("session is busy"), "{err}");
    }

    #[test]
    fn command_lines_carry_type_id_and_camel_case_fields() {
        let cmd = Command::PromptAndWait {
            active_session_id: "as_1".to_string(),
            message: "do the thing".to_string(),
            source: "rpc".to_string(),
        };
        assert_eq!(cmd.name(), "prompt_and_wait");
        let v: Value = serde_json::from_str(&cmd.to_line("req-1").unwrap()).unwrap();
        assert_eq!(v["type"], "prompt_and_wait");
        assert_eq!(v["id"], "req-1");
        assert_eq!(v["activeSessionId"], "as_1");
        assert_eq!(v["message"], "do the thing");
        assert!(!cmd.to_line("x").unwrap().contains('\n'), "lines must be single-line");
    }

    #[test]
    fn create_omits_unset_optional_config() {
        let cmd = Command::Create {
            name: "portal:scene-42".to_string(),
            lifecycle: LIFECYCLE_RESIDENT.to_string(),
            session_path: None,
            config: SessionRuntimeConfig {
                cwd: "/ws/proj".to_string(),
                session_dir: Some("/state/pi/sessions".to_string()),
                append_system_prompt: vec!["contract".to_string()],
                telemetry_disabled: true,
                execution_mode: Some("rpc".to_string()),
                autonomous: Some(AutonomousConfig {
                    enabled: true,
                    max_turns: 40,
                    max_tokens: 400_000,
                    timeout_ms: 1_800_000,
                    max_continuations: 3,
                }),
                ..Default::default()
            },
        };
        let v: Value = serde_json::from_str(&cmd.to_line("c1").unwrap()).unwrap();
        assert_eq!(v["type"], "create");
        assert_eq!(v["lifecycle"], "resident");
        assert!(v.get("sessionPath").is_none(), "unset options must be omitted");
        assert!(v["config"].get("provider").is_none());
        assert_eq!(v["config"]["cwd"], "/ws/proj");
        assert_eq!(v["config"]["autonomous"]["maxTurns"], 40);
        assert_eq!(v["config"]["autonomous"]["timeoutMs"], 1_800_000);
    }

    #[test]
    fn attach_and_decline_ui_use_headless_flags() {
        let v: Value = serde_json::from_str(&Command::attach("as_1", "portal").to_line("a").unwrap())
            .unwrap();
        assert_eq!(v["supportsExtensionUi"], false);
        assert_eq!(v["capabilities"][0], "event_sequence");

        let v: Value =
            serde_json::from_str(&Command::decline_ui("as_1", "req_7").to_line("b").unwrap())
                .unwrap();
        assert_eq!(v["type"], "extension_ui_response");
        assert_eq!(v["requestId"], "req_7");
        assert_eq!(v["response"]["cancelled"], true);
    }

    #[test]
    fn usage_reads_nested_and_flat_shapes() {
        let nested = serde_json::json!({"usage":{"input":30120,"output":8210,"cost":0.42}});
        let u = Usage::from_value(&nested);
        assert_eq!((u.input, u.output, u.total), (30120, 8210, 38330));
        assert_eq!(u.cost_usd, Some(0.42));

        let camel = serde_json::json!({"inputTokens":10,"outputTokens":5,"totalTokens":15});
        let u = Usage::from_value(&camel);
        assert_eq!((u.input, u.output, u.total), (10, 5, 15));
        assert_eq!(Usage::from_value(&serde_json::json!({})).total, 0);
    }

    #[test]
    fn prompt_complete_detects_p3_envelope() {
        let v = serde_json::json!({
            "lastAssistantText":"## Result\ndone","stopReason":"end_turn","turns":14,
            "usage":{"input":1,"output":2}
        });
        let pc = PromptComplete::from_value(&v).unwrap();
        assert_eq!(pc.turns, 14);
        assert_eq!(pc.usage.total, 3);
        assert!(!pc.failed());

        // Present but null still counts: the envelope is there, the text isn't.
        let empty = PromptComplete::from_value(&serde_json::json!({
            "lastAssistantText": Value::Null, "stopReason": "error", "errorMessage": "boom"
        }))
        .unwrap();
        assert!(empty.last_assistant_text.is_none());
        assert!(empty.failed());

        // Pre-P3 daemons return something unusable → fall back.
        assert!(PromptComplete::from_value(&serde_json::json!({"ok": true})).is_none());
        assert!(PromptComplete::from_value(&Value::Null).is_none());
        assert!(
            PromptComplete::from_value(&serde_json::json!({"stopReason": "end_turn"})).is_none(),
            "a stopReason alone is not the P3 envelope; without the result body \
             Portal must fall back or it would report an empty result"
        );
        assert!(
            PromptComplete::from_value(&serde_json::json!(true)).is_none(),
            "pi v0.7.2 answers prompt_and_wait with a bare true"
        );
    }

    #[test]
    fn budget_exhaustion_is_detected_from_status() {
        let hit = AutonomousStatus {
            turns_used: 40,
            tokens_used: 1000,
            ..Default::default()
        };
        assert!(hit.budget_exhausted(40, 400_000));

        let fine = AutonomousStatus {
            turns_used: 14,
            tokens_used: 38330,
            ..Default::default()
        };
        assert!(!fine.budget_exhausted(40, 400_000));

        let reason = AutonomousStatus {
            stop_reason: Some("max_turns".to_string()),
            ..Default::default()
        };
        assert!(reason.budget_exhausted(0, 0));
    }

    /// Verbatim shape of `pi --print --mode json`, trimmed of the bulk.
    const STDIO_STREAM: &str = r###"
{"type":"session","version":3,"id":"b13175af","timestamp":"2026-09-17T06:20:33.970Z","cwd":"/work"}
{"type":"agent_start"}
{"type":"turn_start"}
{"type":"message_start","message":{"role":"user","content":[{"type":"text","text":"do the thing"}]}}
{"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"do the thing"}]}}
{"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"## Result\nDid the thing."}],"provider":"openrouter","usage":{"input":120,"output":40,"totalTokens":160,"cost":{"input":0.001,"output":0.002,"total":0.003}},"stopReason":"stop"}}
{"type":"turn_end","message":{"role":"assistant","content":[]},"toolResults":[]}
{"type":"agent_end","messages":[{"role":"assistant","content":[{"type":"text","text":"## Result\nDid the thing."}]}]}
"###;

    #[test]
    fn stdio_json_lines_fold_into_one_result() {
        let r = StdioResult::parse(STDIO_STREAM);
        assert_eq!(
            r.last_assistant_text.as_deref(),
            Some("## Result\nDid the thing.")
        );
        assert_eq!(r.session_id.as_deref(), Some("b13175af"));
        assert_eq!(r.turns, 1);
        assert_eq!((r.usage.input, r.usage.output, r.usage.total), (120, 40, 160));
        assert_eq!(r.usage.cost_usd, Some(0.003), "cost is nested under cost.total");
        assert_eq!(r.stop_reason.as_deref(), Some("stop"));
        assert!(!r.failed());
    }

    #[test]
    fn stdio_usage_sums_across_turns_and_ignores_noise() {
        let stream = "\
pi: a new version is available\n\
{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"one\"}],\"usage\":{\"input\":10,\"output\":5}}}\n\
{ not json at all\n\
{\"type\":\"turn_end\"}\n\
{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[],\"usage\":{\"input\":20,\"output\":7}}}\n\
{\"type\":\"turn_end\"}\n";
        let r = StdioResult::parse(stream);
        assert_eq!(r.turns, 2);
        assert_eq!(r.usage.total, 42, "usage is per request, so it must be summed");
        assert_eq!(
            r.last_assistant_text.as_deref(),
            Some("one"),
            "a silent tool-only turn must not erase the last real answer"
        );
    }

    #[test]
    fn a_stdio_run_that_errored_is_reported_as_failed() {
        let stream = r#"{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"401 User not found."}}"#;
        let r = StdioResult::parse(stream);
        assert!(r.failed());
        assert_eq!(r.error_message.as_deref(), Some("401 User not found."));
        assert!(r.last_assistant_text.is_none());

        // A bare error line (no message envelope) counts too.
        let bare = StdioResult::parse(r#"{"type":"error","message":"no provider configured"}"#);
        assert!(bare.failed());
        assert_eq!(bare.error_message.as_deref(), Some("no provider configured"));

        assert!(!StdioResult::parse("").failed());
    }

    #[test]
    fn agent_end_alone_still_yields_the_result() {
        // Some builds only emit the final envelope; Portal must not lose the
        // answer just because it saw no per-message events.
        let stream = r#"{"type":"agent_end","messages":[{"role":"user","content":[{"type":"text","text":"go"}]},{"role":"assistant","content":[{"type":"text","text":"done"}],"usage":{"input":3,"output":4}}]}"#;
        let r = StdioResult::parse(stream);
        assert_eq!(r.last_assistant_text.as_deref(), Some("done"));
        assert_eq!(r.usage.total, 7);
    }
}
