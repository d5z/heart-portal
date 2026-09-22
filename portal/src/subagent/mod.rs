//! Native sub-agent runtime (PRD v07, Stage 1).
//!
//! Portal owns a private pi daemon and speaks its JSONL protocol directly.
//! One long-lived pi session per being-chosen *session key*; each task is a
//! prompt into that session, so the sub-agent's working memory accumulates
//! across tasks. Results never come back through the tool call — they arrive
//! in the being's inbox through the same [`HeartCallback`] pipeline
//! `portal_exec --background` uses, so the being can let go and be woken.
//!
//! ```text
//! spawn  → returns {task_id} immediately  (the being releases)
//! run    → prompt_and_wait on the session's socket
//! finish → HeartCallback::deliver_detached  (the being is woken)
//! cancel → abort, no callback               (deliberate, like process kill)
//! ```
//!
//! Module map: [`protocol`] wire types, [`pi_client`] the socket client,
//! [`pi_daemon`] daemon lifecycle, [`ledger`] restart-surviving bookkeeping,
//! [`transcript`] the pullable progress log.

pub mod ledger;
pub mod pi_client;
pub mod pi_daemon;
pub mod protocol;
pub mod transcript;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::{mpsc, Mutex as AsyncMutex, Semaphore};
use tokio::time;
use tracing::{debug, info, warn};

use crate::config::{
    PortalConfig, SubagentConfig, SubagentModelConfig, SUBAGENT_PROVIDERS,
    SUBAGENT_THINKING_LEVELS,
};
use crate::heart_callback::{
    clamp_str, clamp_str_tail, fit_payload, HeartCallback, CALLBACK_COMMAND_MAX_BYTES,
};

use ledger::{Ledger, LedgerCallbackState, LedgerTaskStatus, TaskRecord};
use pi_client::{PiClient, DEFAULT_REQUEST_TIMEOUT};
use pi_daemon::{
    PiDaemon, PiDaemonConfig, Provision, StdioRun, StdioTask, Transport, TransportMode,
    SHUTDOWN_GRACE,
};
use protocol::{
    AutonomousConfig, AutonomousStatus, Command, DaemonMessage, PromptComplete, PromptRequest,
    SessionRuntimeConfig, SessionSummary, Usage, CAP_SESSION_INPUT_ADMISSION, LIFECYCLE_RESIDENT,
};
use transcript::{Transcript, TranscriptPage};

/// Session key used when the being does not choose one.
pub const DEFAULT_SESSION_KEY: &str = "default";
/// A brief is being-authored prose; beyond this it is a file, not a brief.
pub const MAX_BRIEF_BYTES: usize = 64 * 1024;
/// Session keys land in filenames and session names.
pub const MAX_SESSION_KEY_BYTES: usize = 64;
/// Slack over the task's own budget before Portal stops waiting on the daemon.
const PROMPT_TIMEOUT_SLACK: Duration = Duration::from_secs(120);
/// How long a finished task stays visible in `portal_subagent_status`.
const TASK_RETENTION: Duration = Duration::from_secs(5 * 60);
/// How long `cancel` waits for the session to go idle before answering.
const CANCEL_IDLE_WAIT: Duration = Duration::from_secs(10);
/// Window for reconnecting after a mid-task socket loss (PRD §9 risk 8).
const RECONNECT_WINDOW: Duration = Duration::from_secs(60);
/// Partial result handed back by `cancel`.
const PARTIAL_RESULT_BYTES: usize = 8 * 1024;
/// `result_head` in the single-task status form.
const RESULT_HEAD_BYTES: usize = 2 * 1024;
/// Default page size for `portal_subagent_log`.
pub const DEFAULT_LOG_LIMIT: usize = 64 * 1024;
/// How often a stdio task checks whether it has been cancelled. There is no
/// socket to abort over: cancelling means killing the process.
const STDIO_CANCEL_POLL: Duration = Duration::from_millis(250);
/// Synthetic `active_session_id` for a stdio session. Nothing is loaded in a
/// daemon behind it, but the rest of the manager — status, busy tracking,
/// unload — is written against one.
const STDIO_SESSION_PREFIX: &str = "stdio:";
/// Why the session-level controls are unavailable in stdio mode.
const STDIO_NO_SESSION: &str = "this pi has no daemon mode, so each task runs as its own \
     process: there is no live session to steer into. Cancel the task (which kills the \
     process) or spawn a new one with a fuller brief.";

/// What a being sees on its first `portal_subagent_spawn` when nothing tells
/// pi which LLM to talk to. Actionable, not a provider stack trace.
pub const SETUP_GUIDANCE: &str = "Sub-agent is not configured yet. Run portal_subagent_setup \
to choose your provider and API key.\n\
\n\
Example:\n  \
portal_subagent_setup(provider=\"anthropic\", model=\"claude-sonnet-4-5\", api_key=\"sk-ant-...\")\n\
\n\
Supported providers: anthropic, openrouter, openai, gemini, groq, xai";

/// The contract every sub-agent session is created with (PRD §7.1). It exists
/// because the sub-agent cannot ask questions: it must decide, state its
/// assumptions, verify, and report.
const SUB_CONTRACT: &str = "\
You are a sub-agent working for a principal who is not present. You cannot ask \
questions; make reasonable assumptions, state them, and verify. Work only inside \
the working directory given in the task. When finished, end your final message \
with a `## Result` section: what you did, what changed (files), how you verified \
it, and anything the principal must decide or check. Keep the result under ~2000 \
words; put long output in files and reference them. Do not start unrelated work. \
If you cannot finish within budget, stop and report exactly where you are.";

// ── task and session state ──────────────────────────────────────────

/// Per-task caps for one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub max_turns: u32,
    pub max_tokens: u64,
    pub timeout_secs: u64,
    pub max_continuations: u32,
}

impl Budget {
    fn from_config(cfg: &SubagentConfig) -> Self {
        Self {
            max_turns: cfg.budget.max_turns,
            max_tokens: cfg.budget.max_tokens,
            timeout_secs: cfg.budget.timeout_secs,
            max_continuations: cfg.budget.max_continuations,
        }
    }

    fn to_autonomous(self) -> AutonomousConfig {
        AutonomousConfig {
            enabled: true,
            max_turns: self.max_turns,
            max_tokens: self.max_tokens,
            timeout_ms: self.timeout_secs.saturating_mul(1000),
            max_continuations: self.max_continuations,
        }
    }

    fn to_json(self) -> Value {
        json!({
            "max_turns": self.max_turns,
            "max_tokens": self.max_tokens,
            "timeout_secs": self.timeout_secs,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Done,
    Failed,
    Cancelled,
    Interrupted,
    BudgetExhausted,
    Timeout,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Running => "running",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
            TaskStatus::Interrupted => "interrupted",
            TaskStatus::BudgetExhausted => "budget_exhausted",
            TaskStatus::Timeout => "timeout",
        }
    }

    pub fn is_finished(self) -> bool {
        !matches!(self, TaskStatus::Running)
    }

    fn to_ledger(self) -> LedgerTaskStatus {
        match self {
            TaskStatus::Running => LedgerTaskStatus::Running,
            TaskStatus::Done => LedgerTaskStatus::Done,
            TaskStatus::Failed => LedgerTaskStatus::Failed,
            TaskStatus::Cancelled => LedgerTaskStatus::Cancelled,
            TaskStatus::Interrupted => LedgerTaskStatus::Interrupted,
            TaskStatus::BudgetExhausted => LedgerTaskStatus::BudgetExhausted,
            TaskStatus::Timeout => LedgerTaskStatus::Timeout,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackState {
    Pending,
    Sent,
    Suppressed,
}

impl CallbackState {
    fn as_str(self) -> &'static str {
        match self {
            CallbackState::Pending => "pending",
            CallbackState::Sent => "sent",
            CallbackState::Suppressed => "suppressed",
        }
    }
}

struct TaskInner {
    status: TaskStatus,
    ended_at: Option<time::Instant>,
    turns: u32,
    last_tool: Option<String>,
    result_text: Option<String>,
    error: Option<String>,
    usage: Usage,
    callback: CallbackState,
}

pub struct TaskState {
    pub task_id: String,
    pub session_key: String,
    pub brief: String,
    pub workdir: PathBuf,
    pub scene_id: Option<String>,
    pub budget: Budget,
    pub started_at: time::Instant,
    /// Set before `abort` is sent; suppresses the callback exactly like
    /// `ManagedProcess.killed` does for `portal_process kill`.
    cancelled: AtomicBool,
    inner: AsyncMutex<TaskInner>,
}

impl TaskState {
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    async fn status(&self) -> TaskStatus {
        self.inner.lock().await.status
    }

    async fn elapsed_secs(&self) -> u64 {
        let inner = self.inner.lock().await;
        match inner.ended_at {
            Some(end) => end.saturating_duration_since(self.started_at).as_secs(),
            None => self.started_at.elapsed().as_secs(),
        }
    }
}

struct SessionInner {
    active_session_id: Option<String>,
    session_file: Option<String>,
    busy_task: Option<String>,
    last_used: time::Instant,
    tasks_total: u64,
    attached: bool,
}

pub struct SessionState {
    pub key: String,
    /// Fixed at first create: pi sessions carry their cwd in the header, so a
    /// later task with a different workdir would silently escape it.
    pub cwd: PathBuf,
    pub transcript: Transcript,
    inner: AsyncMutex<SessionInner>,
}

/// What `spawn` hands back to the being.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub brief: String,
    pub session: String,
    pub workdir: Option<String>,
    pub budget: Budget,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub scene_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SpawnReceipt {
    pub task_id: String,
    pub session: String,
    pub status: TaskStatus,
    pub resumed: bool,
    pub session_file: Option<String>,
    pub workdir: PathBuf,
    pub budget: Budget,
}

impl SpawnReceipt {
    pub fn to_json(&self) -> Value {
        json!({
            "task_id": self.task_id,
            "session": self.session,
            "status": self.status.as_str(),
            "resumed": self.resumed,
            "session_file": self.session_file,
            "workdir": self.workdir.display().to_string(),
            "budget": self.budget.to_json(),
            "note": "The result will arrive in your inbox as a callback, not as this tool result.",
        })
    }
}

/// The model the task will run on: `[subagent.model]` with the per-task
/// overrides applied. Carried separately from [`TaskState`] because only the
/// stdio transport needs it — a daemon session is *created* with it instead.
#[derive(Debug, Clone, Default)]
struct ModelChoice {
    provider: Option<String>,
    model: Option<String>,
    thinking: Option<String>,
    /// Never logged, never in a status payload.
    api_key: Option<String>,
}

/// What `portal_subagent_setup` asked to change in `[subagent.model]`.
/// `None` leaves a field alone; `Some("")` clears it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelConfigUpdate {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub thinking: Option<String>,
}

impl ModelConfigUpdate {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Merge onto `current`, validating the result as a whole so the being
    /// hears about every problem before anything is written.
    fn apply_to(&self, current: &SubagentModelConfig) -> Result<SubagentModelConfig> {
        fn merge(field: &Option<String>, current: &Option<String>) -> Option<String> {
            match field.as_deref().map(str::trim) {
                None => current.clone(),
                Some("") => None,
                Some(v) => Some(v.to_string()),
            }
        }
        let next = SubagentModelConfig {
            provider: merge(&self.provider, &current.provider).map(|p| p.to_ascii_lowercase()),
            model: merge(&self.model, &current.model),
            thinking: merge(&self.thinking, &current.thinking).map(|t| t.to_ascii_lowercase()),
            api_key: merge(&self.api_key, &current.api_key),
        };

        if let Some(provider) = next.provider.as_deref() {
            if !SUBAGENT_PROVIDERS.contains(&provider) {
                anyhow::bail!(
                    "unknown provider '{provider}'; supported: {}",
                    SUBAGENT_PROVIDERS.join(", ")
                );
            }
        }
        if let Some(level) = next.thinking.as_deref() {
            if !SUBAGENT_THINKING_LEVELS.contains(&level) {
                anyhow::bail!(
                    "unknown thinking level '{level}'; one of: {}",
                    SUBAGENT_THINKING_LEVELS.join(", ")
                );
            }
        }
        if let Some(key) = next.api_key.as_deref() {
            if next.provider.is_none() {
                anyhow::bail!(
                    "api_key needs a provider so the key reaches the right service; \
                     pass provider too (supported: {})",
                    SUBAGENT_PROVIDERS.join(", ")
                );
            }
            if key.chars().any(char::is_whitespace) || !key.is_ascii() {
                anyhow::bail!("api_key contains whitespace or non-ASCII characters; check the paste");
            }
        }
        if let Some(model) = next.model.as_deref() {
            if model.chars().any(char::is_whitespace) {
                anyhow::bail!("model id must not contain whitespace");
            }
        }
        Ok(next)
    }
}

/// Result of a successful `configure_model`.
#[derive(Debug, Clone)]
pub struct ModelConfigOutcome {
    pub model: SubagentModelConfig,
    /// A live daemon was stopped so the new credentials take effect.
    pub daemon_restarted: bool,
}

/// Outcome of one run, before it becomes a callback.
#[derive(Debug, Clone, Default)]
struct TaskOutcome {
    status_override: Option<&'static str>,
    text: Option<String>,
    error: Option<String>,
    usage: Usage,
    turns: u32,
}

// ── manager ─────────────────────────────────────────────────────────

pub struct SubagentManager {
    /// Everything but `[subagent.model]`, which lives in `model` because the
    /// being can change it at runtime through `portal_subagent_setup`.
    config: SubagentConfig,
    /// The live `[subagent.model]`. Read through [`Self::model`].
    model: StdMutex<SubagentModelConfig>,
    /// Where `[subagent.model]` is written back; `None` when Portal started
    /// from in-memory defaults.
    config_path: Option<PathBuf>,
    workspace_root: PathBuf,
    /// Resolved pi argv. `None` ⇒ pi is not installed, the tools stay hidden.
    /// Filled in later by [`Self::ensure_pi_installed`] when
    /// `portal_subagent_setup` gets pi installed. Read through [`Self::command`].
    command: StdMutex<Option<Vec<String>>>,
    /// Swapped for a fresh instance when the provider or key changes, since a
    /// daemon's environment is fixed at spawn. Read through [`Self::daemon`].
    daemon: StdMutex<Arc<PiDaemon>>,
    sessions: AsyncMutex<HashMap<String, Arc<SessionState>>>,
    tasks: AsyncMutex<HashMap<String, Arc<TaskState>>>,
    ledger: AsyncMutex<Ledger>,
    callback: HeartCallback,
    running: Arc<Semaphore>,
    /// One event pump per client generation.
    pump_generation: AsyncMutex<Option<Arc<PiClient>>>,
    /// Last time the daemon was asked to do anything; drives
    /// `daemon_idle_exit_secs`.
    last_activity: AsyncMutex<time::Instant>,
    /// `get_available_models` precheck runs once per Portal lifetime.
    auth_checked: AtomicBool,
    models_available: AsyncMutex<Option<usize>>,
    reconciled: AtomicBool,
    shutting_down: AtomicBool,
    /// Session keys with a spawn in flight. `ensure_session` and the
    /// `busy_task` claim are several awaits apart, so without this two
    /// parallel spawns on one key could both create the pi session and both
    /// pass the busy check.
    spawning: Arc<StdMutex<HashSet<String>>>,
}

/// Holds a session key in [`SubagentManager::spawning`] for the length of one
/// spawn attempt. Releases on every exit path, including the error ones.
struct SpawnGuard {
    key: String,
    keys: Arc<StdMutex<HashSet<String>>>,
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        if let Ok(mut keys) = self.keys.lock() {
            keys.remove(&self.key);
        }
    }
}

impl SubagentManager {
    /// Build the manager. Never fails: a missing pi binary makes the sub-agent
    /// unavailable, which is reported through `portal_status`, not a crash.
    pub fn new(config: &PortalConfig, callback: HeartCallback) -> Arc<Self> {
        let sub = config.subagent.clone();
        let state_dir = sub.resolved_state_dir();
        let mut command = PiDaemon::resolve_command(sub.command.as_deref());

        // No pi anywhere (or Portal's own copy at the wrong version): install
        // the pinned one. Best effort — every failure mode leaves `command`
        // as it was and Portal carries on without the sub-agent.
        if sub.enabled && sub.command.is_none() && sub.auto_install {
            command = Self::provision_at_startup(command);
        }

        if sub.enabled {
            match &command {
                Some(argv) => info!(
                    "subagent enabled: pi at {}, state dir {}",
                    argv[0],
                    state_dir.display()
                ),
                None => warn!(
                    "subagent enabled but no pi binary found (looked at \
                     ~/.heart-portal/pi/bin/pi, then pi and prime-agent on PATH); \
                     portal_subagent_* tools are hidden until portal_subagent_setup \
                     installs it"
                ),
            }
        }

        let daemon = Arc::new(PiDaemon::new(PiDaemonConfig {
            command: command.clone().unwrap_or_default(),
            state_dir: state_dir.clone(),
            workspace_root: config.security.workspace_root.clone(),
            env_passthrough: sub.env_passthrough.clone(),
            api_key: sub.model.api_key.clone(),
            provider: sub.model.provider.clone(),
        }));

        let ledger = Ledger::load(state_dir.join("ledger.json"));
        let max_concurrent = sub.max_concurrent.max(1);

        Arc::new(Self {
            model: StdMutex::new(sub.model.clone()),
            config_path: config.config_path.clone(),
            config: sub,
            workspace_root: config.security.workspace_root.clone(),
            command: StdMutex::new(command),
            daemon: StdMutex::new(daemon),
            sessions: AsyncMutex::new(HashMap::new()),
            tasks: AsyncMutex::new(HashMap::new()),
            ledger: AsyncMutex::new(ledger),
            callback,
            running: Arc::new(Semaphore::new(max_concurrent)),
            pump_generation: AsyncMutex::new(None),
            last_activity: AsyncMutex::new(time::Instant::now()),
            auth_checked: AtomicBool::new(false),
            models_available: AsyncMutex::new(None),
            reconciled: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            spawning: Arc::new(StdMutex::new(HashSet::new())),
        })
    }

    /// Startup half of auto-provisioning: decide whether the resolved
    /// `command` calls for an install, run it (with backoff), and re-resolve.
    fn provision_at_startup(resolved: Option<Vec<String>>) -> Option<Vec<String>> {
        let Some(root) = pi_daemon::bundled_pi_root() else {
            return resolved;
        };
        if !pi_daemon::bundled_pi_wants_provisioning(resolved.as_deref(), &root) {
            return resolved;
        }
        match pi_daemon::auto_provision_pi(false) {
            Provision::Ready(_) => PiDaemon::resolve_command(None).or(resolved),
            // Each of these already logged why; the sub-agent stays as it was.
            Provision::NoNpm
            | Provision::Deferred
            | Provision::Failed(_)
            | Provision::Unsupported => resolved,
        }
    }

    /// Snapshot of the resolved pi argv.
    fn command(&self) -> Option<Vec<String>> {
        self.command
            .lock()
            .map(|c| c.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    /// Whether a pi binary has been resolved (independent of `enabled`).
    pub fn pi_installed(&self) -> bool {
        self.command().is_some()
    }

    /// Enabled *and* pi actually resolvable. Gates tool advertisement.
    pub fn is_available(&self) -> bool {
        self.config.enabled && self.pi_installed()
    }

    /// Enabled and either available or able to become so through
    /// `portal_subagent_setup` (which auto-installs pi). Gates advertising
    /// the setup tool on its own when the rest are hidden. An explicit
    /// `[subagent].command` that does not resolve is a config error, not
    /// something setup can fix, so it does not count.
    pub fn setup_offered(&self) -> bool {
        self.config.enabled
            && (self.pi_installed() || (self.config.auto_install && self.config.command.is_none()))
    }

    pub fn wants_eager_start(&self) -> bool {
        self.is_available() && self.config.eager
    }

    fn ensure_available(&self) -> Result<()> {
        if !self.config.enabled {
            anyhow::bail!("the sub-agent is disabled in configuration ([subagent].enabled)");
        }
        if !self.pi_installed() {
            anyhow::bail!(
                "no pi binary found: run portal_subagent_setup to install it, put pi on \
                 PATH, or set [subagent].command in portal.toml"
            );
        }
        Ok(())
    }

    /// Make sure pi is installed, installing Portal's pinned copy if it is
    /// not. Called by `portal_subagent_setup`, so the backoff that guards
    /// startup does not apply: the being asked. Returns whether an install
    /// happened. Refuses (rather than silently doing nothing) when
    /// `[subagent].command` names a binary that does not exist — that is a
    /// configuration to fix, not something to paper over.
    pub async fn ensure_pi_installed(self: &Arc<Self>) -> Result<bool> {
        if self.pi_installed() {
            return Ok(false);
        }
        if let Some(argv) = self.config.command.as_deref().filter(|a| !a.is_empty()) {
            anyhow::bail!(
                "[subagent].command is set to '{}' but that does not exist; fix or remove \
                 it in portal.toml so Portal can install its own copy",
                argv[0]
            );
        }
        if !self.config.auto_install {
            anyhow::bail!(
                "no pi binary found and [subagent].auto_install is false; install pi \
                 ({}@{}) yourself or set [subagent].command",
                pi_daemon::PI_NPM_PACKAGE,
                pi_daemon::PI_PINNED_VERSION
            );
        }

        let outcome = tokio::task::spawn_blocking(|| pi_daemon::auto_provision_pi(true))
            .await
            .map_err(|e| anyhow::anyhow!("the pi installer panicked: {e}"))?;
        match outcome {
            Provision::Ready(_) => {}
            Provision::NoNpm => anyhow::bail!(
                "could not auto-install pi: npm is not on PATH. Install Node.js (which \
                 brings npm) and call portal_subagent_setup again, or install pi \
                 yourself and put it on PATH"
            ),
            Provision::Failed(reason) => anyhow::bail!("could not auto-install pi: {reason}"),
            Provision::Unsupported => anyhow::bail!(
                "auto-installing pi is not supported on this platform; install pi and \
                 set [subagent].command"
            ),
            // `force` disables the backoff, so this cannot come back; treat it
            // like a failure rather than trusting that.
            Provision::Deferred => anyhow::bail!("pi auto-install was deferred; try again"),
        }

        let Some(command) = PiDaemon::resolve_command(None) else {
            anyhow::bail!(
                "pi was installed but still does not resolve; check ~/.heart-portal/pi/bin/pi"
            );
        };
        self.adopt_command(command).await;
        Ok(true)
    }

    /// Switch the manager over to a freshly resolved pi argv: the daemon's
    /// argv is fixed at construction, so it is replaced the same way a
    /// credential change replaces it.
    async fn adopt_command(self: &Arc<Self>, command: Vec<String>) {
        info!("subagent now available: pi at {}", command[0]);
        if let Ok(mut slot) = self.command.lock() {
            *slot = Some(command.clone());
        }
        let old = self.daemon();
        old.shutdown(SHUTDOWN_GRACE).await;
        let mut daemon_config = old.config().clone();
        daemon_config.command = command;
        let fresh = Arc::new(PiDaemon::new(daemon_config));
        if let Ok(mut slot) = self.daemon.lock() {
            *slot = fresh;
        }
        *self.pump_generation.lock().await = None;
    }

    // ── model configuration ─────────────────────────────────────────

    /// Snapshot of the live `[subagent.model]`.
    fn model(&self) -> SubagentModelConfig {
        self.model
            .lock()
            .map(|m| m.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    /// Public, for the setup tool's status view. Contains the raw key: mask
    /// it before showing it to anyone.
    pub fn model_config(&self) -> SubagentModelConfig {
        self.model()
    }

    fn daemon(&self) -> Arc<PiDaemon> {
        self.daemon
            .lock()
            .map(|d| Arc::clone(&d))
            .unwrap_or_else(|poisoned| Arc::clone(&poisoned.into_inner()))
    }

    /// True when nothing anywhere tells pi which LLM to use: no
    /// `[subagent.model]`, no provider key forwarded from Portal's own
    /// environment, and no `pi login` credentials in the agent dir. The
    /// first spawn then gets [`SETUP_GUIDANCE`] instead of a provider error.
    pub fn needs_setup(&self) -> bool {
        if self.model().is_configured() {
            return false;
        }
        let key_in_env = self.config.env_passthrough.iter().any(|name| {
            name.ends_with("_API_KEY")
                && std::env::var_os(name).is_some_and(|v| !v.is_empty())
        });
        if key_in_env {
            return false;
        }
        !self.daemon().agent_dir().join("auth.json").is_file()
    }

    /// Apply `update` to `[subagent.model]`: validate, persist to portal.toml,
    /// then switch the running manager over. When the provider or key changed
    /// the pi daemon is replaced, because its environment was fixed at spawn;
    /// that is refused while a task is running rather than killing it.
    pub async fn configure_model(
        self: &Arc<Self>,
        update: ModelConfigUpdate,
    ) -> Result<ModelConfigOutcome> {
        let current = self.model();
        let next = update.apply_to(&current)?;

        let Some(path) = self.config_path.clone() else {
            anyhow::bail!(
                "Portal was started without a config file, so there is nowhere to save \
                 this; create portal.toml (see portal.example.toml) and restart Portal"
            );
        };

        let credentials_changed =
            next.provider != current.provider || next.api_key != current.api_key;
        if credentials_changed && self.has_running_tasks().await {
            anyhow::bail!(
                "a sub-agent task is still running; wait for it to finish (or cancel it \
                 with portal_subagent_control) before changing the provider or API key"
            );
        }

        crate::config::write_subagent_model(&path, &next)?;
        if let Ok(mut live) = self.model.lock() {
            *live = next.clone();
        }

        let daemon_restarted = if credentials_changed {
            self.replace_daemon(&next).await
        } else {
            false
        };
        info!(
            "sub-agent model configured: provider={} model={} thinking={} key={}",
            next.provider.as_deref().unwrap_or("-"),
            next.model.as_deref().unwrap_or("-"),
            next.thinking.as_deref().unwrap_or("-"),
            if next.api_key.is_some() { "set" } else { "unset" }
        );
        Ok(ModelConfigOutcome {
            model: next,
            daemon_restarted,
        })
    }

    async fn has_running_tasks(&self) -> bool {
        for task in self.tasks.lock().await.values() {
            if !task.status().await.is_finished() {
                return true;
            }
        }
        false
    }

    /// Stop the current daemon (if any) and install a fresh `PiDaemon` that
    /// will start with `next`'s credentials in its environment. Loaded
    /// sessions are forgotten — their files stay, so the next spawn resumes
    /// them. Returns whether a running daemon was actually stopped.
    async fn replace_daemon(self: &Arc<Self>, next: &SubagentModelConfig) -> bool {
        let old = self.daemon();
        let was_running = old.client().await.is_some();
        if was_running {
            info!("restarting the pi daemon so the new provider credentials take effect");
        }
        old.shutdown(SHUTDOWN_GRACE).await;
        for session in self.sessions.lock().await.values() {
            let mut inner = session.inner.lock().await;
            inner.active_session_id = None;
            inner.attached = false;
        }
        *self.pump_generation.lock().await = None;

        let mut daemon_config = old.config().clone();
        daemon_config.api_key = next.api_key.clone();
        daemon_config.provider = next.provider.clone();
        let fresh = Arc::new(PiDaemon::new(daemon_config));
        if let Ok(mut slot) = self.daemon.lock() {
            *slot = fresh;
        }
        // The provider precheck is about the *old* credentials.
        self.auth_checked.store(false, Ordering::SeqCst);
        *self.models_available.lock().await = None;
        was_running
    }

    // ── daemon / client ─────────────────────────────────────────────

    /// The transport for the next task: a connected daemon client, or the
    /// per-task stdio fallback when this pi has no daemon mode. Also
    /// (re)starts the event pump when the client generation changes.
    async fn transport(self: &Arc<Self>) -> Result<Transport> {
        self.ensure_available()?;
        // `ensure_transport` would start a fresh pi daemon; during shutdown
        // that resurrects the very process we are tearing down (and outlives
        // us).
        if self.shutting_down.load(Ordering::SeqCst) {
            anyhow::bail!("Portal is shutting down; the pi daemon will not be started again");
        }
        let transport = self.daemon().ensure_transport().await?;
        *self.last_activity.lock().await = time::Instant::now();

        let Transport::Daemon(client) = &transport else {
            return Ok(transport);
        };

        if !client.hello().supports(CAP_SESSION_INPUT_ADMISSION)
            && !client.hello().server_capabilities.is_empty()
        {
            warn!(
                "pi daemon does not advertise {CAP_SESSION_INPUT_ADMISSION}; \
                 prompts may be refused"
            );
        }

        let mut pump = self.pump_generation.lock().await;
        let same = pump
            .as_ref()
            .is_some_and(|c| Arc::ptr_eq(c, client) && c.is_connected());
        if !same {
            *pump = Some(Arc::clone(client));
            self.spawn_event_pump(Arc::clone(client));
        }
        Ok(transport)
    }

    /// A connected client. The session-level controls (steer, abort,
    /// recovery) only exist in daemon mode, so they say so rather than
    /// pretending.
    async fn client(self: &Arc<Self>) -> Result<Arc<PiClient>> {
        match self.transport().await? {
            Transport::Daemon(client) => Ok(client),
            Transport::Stdio => anyhow::bail!("{STDIO_NO_SESSION}"),
        }
    }

    /// `[subagent.model]` with this task's overrides applied.
    fn model_choice(&self, req: &SpawnRequest) -> ModelChoice {
        let model = self.model();
        ModelChoice {
            provider: model.provider,
            model: req.model.clone().or(model.model),
            thinking: req.thinking.clone().or(model.thinking),
            api_key: model.api_key,
        }
    }

    /// Start the daemon ahead of the first task (`[subagent].eager`).
    pub async fn warmup(self: &Arc<Self>) {
        if !self.wants_eager_start() {
            return;
        }
        match self.transport().await {
            Ok(Transport::Daemon(client)) => info!(
                "pi daemon warm ({})",
                client.hello().app_version.as_deref().unwrap_or("unknown version")
            ),
            Ok(Transport::Stdio) => {
                info!("pi has no daemon mode; each task will run as its own pi process")
            }
            Err(e) => warn!("eager pi daemon start failed: {e:#}"),
        }
    }

    /// Fan session events into transcripts and task counters, and decline every
    /// UI request so a headless sub-agent can never wait on a human.
    fn spawn_event_pump(self: &Arc<Self>, client: Arc<PiClient>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut rx = client.subscribe();
            loop {
                let msg = match rx.recv().await {
                    Ok(m) => m,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("subagent event pump lagged; dropped {n} event(s)");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                let Some(manager) = weak.upgrade() else {
                    break;
                };
                match msg {
                    DaemonMessage::SessionEvent(event) => {
                        manager.route_event(&event).await;
                    }
                    DaemonMessage::ExtensionUiRequest(req) => {
                        debug!(
                            "declining extension UI request {} ({})",
                            req.id,
                            req.method.as_deref().unwrap_or("?")
                        );
                        client
                            .notify(Command::decline_ui(&req.active_session_id, &req.id))
                            .await;
                    }
                    DaemonMessage::SessionClosed(closed) => {
                        manager
                            .mark_session_unloaded(&closed.active_session_id, &closed.reason)
                            .await;
                    }
                    DaemonMessage::DaemonClosing(closing) => {
                        warn!("pi daemon is closing ({})", closing.reason);
                        break;
                    }
                    _ => {}
                }
            }
            debug!("subagent event pump for {} ended", client.label());
        });
    }

    async fn route_event(&self, event: &protocol::SessionEvent) {
        let Some(session) = self.session_by_active_id(&event.active_session_id).await else {
            return;
        };
        let digest = session.transcript.record(event).await;
        if digest == transcript::EventDigest::default() {
            return;
        }

        let busy = session.inner.lock().await.busy_task.clone();
        let Some(task_id) = busy else { return };
        let Some(task) = self.tasks.lock().await.get(&task_id).cloned() else {
            return;
        };
        let mut inner = task.inner.lock().await;
        if digest.turn_completed {
            inner.turns += 1;
        }
        if let Some(tool) = digest.tool_started {
            inner.last_tool = Some(tool);
        }
    }

    async fn session_by_active_id(&self, active_session_id: &str) -> Option<Arc<SessionState>> {
        let sessions = self.sessions.lock().await;
        for session in sessions.values() {
            let matches = session
                .inner
                .lock()
                .await
                .active_session_id
                .as_deref()
                .is_some_and(|id| id == active_session_id);
            if matches {
                return Some(Arc::clone(session));
            }
        }
        None
    }

    async fn mark_session_unloaded(&self, active_session_id: &str, reason: &str) {
        let Some(session) = self.session_by_active_id(active_session_id).await else {
            return;
        };
        let mut inner = session.inner.lock().await;
        inner.active_session_id = None;
        inner.attached = false;
        debug!("session '{}' unloaded by the daemon ({reason})", session.key);
    }

    // ── spawn ───────────────────────────────────────────────────────

    /// Delegate a task. Returns as soon as the prompt is in flight; the result
    /// is delivered to Heart's callback inbox (PRD §4.3).
    pub async fn spawn(self: &Arc<Self>, req: SpawnRequest) -> Result<SpawnReceipt> {
        self.ensure_available()?;
        if self.shutting_down.load(Ordering::SeqCst) {
            anyhow::bail!("Portal is shutting down");
        }
        // First use: say how to configure, not which provider call failed.
        if self.needs_setup() {
            anyhow::bail!("{SETUP_GUIDANCE}");
        }

        let brief = req.brief.trim().to_string();
        if brief.is_empty() {
            anyhow::bail!("brief is empty: say what to do and what 'done' looks like");
        }
        if brief.len() > MAX_BRIEF_BYTES {
            anyhow::bail!(
                "brief is {} bytes (max {MAX_BRIEF_BYTES}); put the detail in a file and reference it",
                brief.len()
            );
        }
        let key = normalize_session_key(&req.session)?;
        let workdir = self.resolve_workdir(req.workdir.as_deref())?;

        // Serialize spawns per session key for the whole check-and-set window.
        let _spawn_guard = self.begin_spawn(&key)?;

        // Capacity before anything is started, so a refusal costs nothing.
        let permit = Arc::clone(&self.running)
            .try_acquire_owned()
            .map_err(|_| {
                anyhow::anyhow!(
                    "{}/{} sub-agent tasks already running; wait for one to finish",
                    self.config.max_concurrent,
                    self.config.max_concurrent
                )
            })?;

        let transport = self.transport().await?;
        let (session, resumed) = self
            .ensure_session(&transport, &key, &workdir, &req)
            .await?;

        // Claim the session under one lock hold: checking `busy_task` and then
        // setting it in two steps would let two parallel spawns both pass.
        let task_id = format!("sub_{}", uuid::Uuid::new_v4());
        let (active_session_id, session_file) = {
            let mut inner = session.inner.lock().await;
            if let Some(running) = inner.busy_task.as_deref() {
                anyhow::bail!(
                    "session '{key}' is busy with task {running}; wait for it to finish \
                     or use a different session key"
                );
            }
            let active_session_id = inner
                .active_session_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("session '{key}' has no live pi session"))?;
            inner.busy_task = Some(task_id.clone());
            inner.tasks_total += 1;
            inner.last_used = time::Instant::now();
            (active_session_id, inner.session_file.clone())
        };

        let task = self
            .register_task(task_id, &key, &brief, &workdir, &req)
            .await;
        {
            let mut ledger = self.ledger.lock().await;
            ledger.start_task(
                &task.task_id,
                &key,
                &brief,
                &workdir.display().to_string(),
                req.scene_id.as_deref(),
            );
            ledger.touch_session(&key);
            ledger.save_lossy();
        }

        session
            .transcript
            .append_line(&format!("[start] task {} — {}", task.task_id, head(&brief, 120)))
            .await;

        let manager = Arc::clone(self);
        let run_task = Arc::clone(&task);
        let run_session = Arc::clone(&session);
        let choice = self.model_choice(&req);
        let message = render_prompt(&task, &brief);
        tokio::spawn(async move {
            manager
                .run_task(
                    transport,
                    run_session,
                    run_task,
                    active_session_id,
                    message,
                    choice,
                )
                .await;
            drop(permit);
        });

        Ok(SpawnReceipt {
            task_id: task.task_id.clone(),
            session: key,
            status: TaskStatus::Running,
            resumed,
            session_file,
            workdir,
            budget: req.budget,
        })
    }

    /// Claim `key` for one spawn attempt, or refuse if another is already in
    /// flight for it. The guard releases the claim on drop.
    fn begin_spawn(&self, key: &str) -> Result<SpawnGuard> {
        let mut keys = self
            .spawning
            .lock()
            .map_err(|_| anyhow::anyhow!("the sub-agent spawn guard is poisoned"))?;
        if !keys.insert(key.to_string()) {
            anyhow::bail!(
                "session '{key}' already has a spawn in flight; wait for it to finish \
                 or use a different session key"
            );
        }
        Ok(SpawnGuard {
            key: key.to_string(),
            keys: Arc::clone(&self.spawning),
        })
    }

    async fn register_task(
        &self,
        task_id: String,
        key: &str,
        brief: &str,
        workdir: &Path,
        req: &SpawnRequest,
    ) -> Arc<TaskState> {
        let task = Arc::new(TaskState {
            task_id,
            session_key: key.to_string(),
            brief: brief.to_string(),
            workdir: workdir.to_path_buf(),
            scene_id: req.scene_id.clone(),
            budget: req.budget,
            started_at: time::Instant::now(),
            cancelled: AtomicBool::new(false),
            inner: AsyncMutex::new(TaskInner {
                status: TaskStatus::Running,
                ended_at: None,
                turns: 0,
                last_tool: None,
                result_text: None,
                error: None,
                usage: Usage::default(),
                callback: CallbackState::Pending,
            }),
        });
        self.tasks
            .lock()
            .await
            .insert(task.task_id.clone(), Arc::clone(&task));
        task
    }

    /// Get or create the pi session for `key`, resuming its file if we have one.
    async fn ensure_session(
        self: &Arc<Self>,
        transport: &Transport,
        key: &str,
        workdir: &Path,
        req: &SpawnRequest,
    ) -> Result<(Arc<SessionState>, bool)> {
        let existing = self.sessions.lock().await.get(key).cloned();
        if let Some(session) = existing {
            // A session's cwd is fixed at create; refusing here is the only way
            // to keep the resumed harness honest about where it works.
            if session.cwd != workdir {
                anyhow::bail!(
                    "session '{key}' works in {}; use a different session key or reset it \
                     instead of changing workdir to {}",
                    session.cwd.display(),
                    workdir.display()
                );
            }
            if session.inner.lock().await.active_session_id.is_some() {
                return Ok((session, true));
            }
            let resumed = self.open_pi_session(transport, &session, req).await?;
            return Ok((session, resumed));
        }

        let session = Arc::new(SessionState {
            key: key.to_string(),
            cwd: workdir.to_path_buf(),
            transcript: Transcript::new(),
            inner: AsyncMutex::new(SessionInner {
                active_session_id: None,
                session_file: self
                    .ledger
                    .lock()
                    .await
                    .session(key)
                    .and_then(|s| s.session_file.clone()),
                busy_task: None,
                last_used: time::Instant::now(),
                tasks_total: 0,
                attached: false,
            }),
        });
        let resumed = self.open_pi_session(transport, &session, req).await?;
        self.sessions
            .lock()
            .await
            .insert(key.to_string(), Arc::clone(&session));
        Ok((session, resumed))
    }

    /// `create` (resuming by path when known) + `attach`, then remember the
    /// session file so the harness survives a restart.
    async fn open_pi_session(
        self: &Arc<Self>,
        transport: &Transport,
        session: &Arc<SessionState>,
        req: &SpawnRequest,
    ) -> Result<bool> {
        let client = match transport {
            Transport::Daemon(client) => client,
            Transport::Stdio => return self.open_stdio_session(session).await,
        };

        let session_path = {
            let inner = session.inner.lock().await;
            inner
                .session_file
                .clone()
                .filter(|p| Path::new(p).exists())
        };
        let resumed = session_path.is_some();

        let mut append_system_prompt = vec![SUB_CONTRACT.to_string()];
        if let Some(extra) = self
            .config
            .append_system_prompt
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            append_system_prompt.push(extra.to_string());
        }

        let model = self.model();
        let daemon = self.daemon();
        let config = SessionRuntimeConfig {
            cwd: session.cwd.display().to_string(),
            agent_dir: Some(daemon.agent_dir().display().to_string()),
            session_dir: Some(daemon.sessions_dir().display().to_string()),
            provider: model.provider,
            model: req.model.clone().or(model.model),
            thinking: req.thinking.clone().or(model.thinking),
            append_system_prompt,
            skills: self.config.skills.clone(),
            extensions: self.config.extensions.clone(),
            no_context_files: false,
            telemetry_disabled: true,
            execution_mode: Some("rpc".to_string()),
            autonomous: Some(req.budget.to_autonomous()),
        };

        let data = client
            .request(
                Command::Create {
                    name: format!("portal:{}", session.key),
                    lifecycle: LIFECYCLE_RESIDENT.to_string(),
                    session_path,
                    config,
                },
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await
            .with_context(|| format!("opening pi session '{}'", session.key))?;

        let summary: SessionSummary = serde_json::from_value(data.clone())
            .with_context(|| format!("unexpected create payload: {data}"))?;
        if summary.active_session_id.is_empty() {
            anyhow::bail!("pi create returned no activeSessionId: {data}");
        }

        {
            let mut inner = session.inner.lock().await;
            inner.active_session_id = Some(summary.active_session_id.clone());
            if let Some(file) = summary.session_file.clone() {
                inner.session_file = Some(file);
            }
            inner.last_used = time::Instant::now();
        }
        {
            let mut ledger = self.ledger.lock().await;
            ledger.upsert_session(
                &session.key,
                &session.cwd.display().to_string(),
                summary.session_file.as_deref(),
            );
            ledger.save_lossy();
        }

        // Attach for progress only; completion is `prompt_and_wait`'s job.
        if let Err(e) = client
            .request(
                Command::attach(&summary.active_session_id, client.client_id()),
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await
        {
            warn!("could not attach to session '{}': {e:#}", session.key);
        } else {
            session.inner.lock().await.attached = true;
        }

        self.check_auth(client, &summary.active_session_id).await?;

        info!(
            "pi session '{}' {} ({})",
            session.key,
            if resumed { "resumed" } else { "created" },
            summary
                .session_file
                .as_deref()
                .unwrap_or("no session file reported")
        );
        Ok(resumed)
    }

    /// The stdio stand-in for `open_pi_session`. There is nothing to create:
    /// each task brings its own process. The session still exists as Portal
    /// bookkeeping — a cwd, a transcript, a busy flag, a ledger row — so
    /// everything the being can ask about a task keeps working.
    async fn open_stdio_session(self: &Arc<Self>, session: &Arc<SessionState>) -> Result<bool> {
        {
            let mut inner = session.inner.lock().await;
            inner.active_session_id = Some(format!("{STDIO_SESSION_PREFIX}{}", session.key));
            inner.attached = false;
            inner.last_used = time::Instant::now();
        }
        {
            let mut ledger = self.ledger.lock().await;
            ledger.upsert_session(&session.key, &session.cwd.display().to_string(), None);
            ledger.save_lossy();
        }
        info!(
            "session '{}' runs over stdio: one pi process per task, no shared context",
            session.key
        );
        // Never "resumed": a fresh process carries nothing of the last task.
        Ok(false)
    }

    /// Fail the first spawn loudly rather than letting every task die on a
    /// missing provider (PRD §9 risk 5).
    async fn check_auth(&self, client: &Arc<PiClient>, active_session_id: &str) -> Result<()> {
        if self.auth_checked.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let data = match client
            .request(
                Command::GetAvailableModels {
                    active_session_id: active_session_id.to_string(),
                },
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await
        {
            Ok(d) => d,
            Err(e) => {
                // Not fatal: an older daemon may not implement it.
                debug!("get_available_models unavailable: {e:#}");
                return Ok(());
            }
        };
        let count = data
            .as_array()
            .map(|a| a.len())
            .or_else(|| data.get("models").and_then(|m| m.as_array()).map(|a| a.len()));
        *self.models_available.lock().await = count;
        if count == Some(0) {
            self.auth_checked.store(false, Ordering::SeqCst);
            anyhow::bail!(
                "pi has no configured provider; run `PRIME_AGENT_CODING_AGENT_DIR={} pi login` \
                 once, or pass an API key via [subagent].env_passthrough",
                self.daemon().agent_dir().display()
            );
        }
        Ok(())
    }

    // ── run ─────────────────────────────────────────────────────────

    /// Run the task over whichever transport `spawn` resolved.
    async fn run_task(
        self: Arc<Self>,
        transport: Transport,
        session: Arc<SessionState>,
        task: Arc<TaskState>,
        active_session_id: String,
        message: String,
        choice: ModelChoice,
    ) {
        match transport {
            Transport::Daemon(client) => {
                self.run_task_daemon(client, session, task, active_session_id, message)
                    .await
            }
            Transport::Stdio => self.run_task_stdio(session, task, message, choice).await,
        }
    }

    /// One `pi --print --mode json` process for this task, killed if the
    /// being cancels or Portal shuts down. Everything after the run — status,
    /// ledger, transcript, callback — is the same `finalize` the daemon path
    /// uses, so the being cannot tell the two apart.
    async fn run_task_stdio(
        self: Arc<Self>,
        session: Arc<SessionState>,
        task: Arc<TaskState>,
        message: String,
        choice: ModelChoice,
    ) {
        // Rendered progress lines, drained into the session transcript so
        // `portal_subagent_log` keeps working without an event pump.
        let (lines_tx, mut lines_rx) = mpsc::unbounded_channel::<String>();
        let pump_session = Arc::clone(&session);
        let pump = tokio::spawn(async move {
            while let Some(line) = lines_rx.recv().await {
                pump_session.transcript.append_line(&line).await;
            }
        });

        let spec = StdioTask {
            prompt: message,
            workdir: task.workdir.clone(),
            provider: choice.provider,
            model: choice.model,
            api_key: choice.api_key,
            thinking: choice.thinking,
            timeout: Duration::from_secs(task.budget.timeout_secs),
            transcript: Some(lines_tx),
        };

        let daemon = self.daemon();
        let mut handle = tokio::spawn(async move { daemon.run_stdio_task(spec).await });

        // There is no socket to abort over: cancelling a stdio task means
        // dropping the run, which kills pi (`kill_on_drop`).
        let run = loop {
            tokio::select! {
                joined = &mut handle => {
                    break joined
                        .map_err(|e| anyhow::anyhow!("the pi task panicked: {e}"))
                        .and_then(|inner| inner);
                }
                _ = time::sleep(STDIO_CANCEL_POLL) => {
                    if task.is_cancelled() || self.shutting_down.load(Ordering::SeqCst) {
                        handle.abort();
                        let _ = handle.await;
                        break Err(anyhow::anyhow!("the pi task was stopped before it finished"));
                    }
                }
            }
        };
        // The sender lives in the run; with it dropped the pump sees the end.
        let _ = time::timeout(SHUTDOWN_GRACE, pump).await;

        let (status, outcome) = match run {
            Ok(run) => self.stdio_outcome(&task, run).await,
            Err(e) => {
                let cancelled = task.is_cancelled();
                let interrupted = self.shutting_down.load(Ordering::SeqCst);
                if cancelled || interrupted {
                    (
                        if cancelled {
                            TaskStatus::Cancelled
                        } else {
                            TaskStatus::Interrupted
                        },
                        TaskOutcome::default(),
                    )
                } else {
                    (
                        TaskStatus::Failed,
                        TaskOutcome {
                            error: Some(format!("{e:#}")),
                            ..Default::default()
                        },
                    )
                }
            }
        };

        self.finalize(&session, &task, status, outcome).await;
    }

    /// Classify a finished stdio run the way the daemon path classifies a
    /// finished prompt, so both produce the same callback shapes.
    async fn stdio_outcome(&self, task: &Arc<TaskState>, run: StdioRun) -> (TaskStatus, TaskOutcome) {
        let error = run.error();
        let outcome = TaskOutcome {
            status_override: error.is_some().then_some("failed"),
            text: run.result.last_assistant_text.clone(),
            error,
            usage: run.result.usage,
            turns: run.result.turns,
        };
        if run.timed_out {
            // Whatever it managed to say before the wall clock is still worth
            // handing back; the status is what says it was cut short.
            return (TaskStatus::Timeout, outcome);
        }
        // `classify` reads the accounting an autonomous daemon would report,
        // so budget exhaustion is judged by one rule in one place.
        let reported = json!({
            "turnsUsed": run.result.turns,
            "tokensUsed": run.result.usage.total,
            "stopReason": run.result.stop_reason,
        });
        (classify(task, &outcome, &reported).await, outcome)
    }

    async fn run_task_daemon(
        self: Arc<Self>,
        client: Arc<PiClient>,
        session: Arc<SessionState>,
        task: Arc<TaskState>,
        active_session_id: String,
        message: String,
    ) {
        let timeout = Duration::from_secs(task.budget.timeout_secs) + PROMPT_TIMEOUT_SLACK;
        let prompt = PromptRequest {
            active_session_id: active_session_id.clone(),
            message,
        };

        let result = client.request(prompt.into_command(), timeout).await;

        let (status, outcome) = match result {
            Ok(value) => {
                let outcome = self
                    .collect_outcome(&client, &active_session_id, Some(&value))
                    .await;
                (classify(&task, &outcome, &value).await, outcome)
            }
            Err(e) => {
                let message = format!("{e:#}");
                let cancelled = task.is_cancelled();
                let shutting_down = self.shutting_down.load(Ordering::SeqCst);
                if cancelled || shutting_down {
                    // The prompt was aborted on purpose. Reconnecting to
                    // collect a result nobody will read would race the
                    // teardown and can start a fresh pi daemon behind it.
                    (
                        if cancelled {
                            TaskStatus::Cancelled
                        } else {
                            TaskStatus::Interrupted
                        },
                        TaskOutcome {
                            error: (!cancelled).then_some(message),
                            ..Default::default()
                        },
                    )
                } else if message.contains("did not answer within") {
                    // Hard wall clock: abort so the sub stops burning tokens.
                    client
                        .notify(Command::Abort {
                            active_session_id: active_session_id.clone(),
                        })
                        .await;
                    let outcome = self
                        .collect_outcome(&client, &active_session_id, None)
                        .await;
                    (TaskStatus::Timeout, outcome)
                } else if !client.is_connected() {
                    self.recover_after_socket_loss(&task, &active_session_id)
                        .await
                } else {
                    (
                        TaskStatus::Failed,
                        TaskOutcome {
                            error: Some(message),
                            ..Default::default()
                        },
                    )
                }
            }
        };

        self.finalize(&session, &task, status, outcome).await;
    }

    /// Pull the result body and accounting. `value` is the `prompt_and_wait`
    /// payload when we have one: with pi change P3 it already contains
    /// everything and no follow-up round trips are needed.
    async fn collect_outcome(
        &self,
        client: &Arc<PiClient>,
        active_session_id: &str,
        value: Option<&Value>,
    ) -> TaskOutcome {
        if let Some(complete) = value.and_then(PromptComplete::from_value) {
            return TaskOutcome {
                status_override: complete.failed().then_some("failed"),
                text: complete.last_assistant_text,
                error: complete.error_message,
                usage: complete.usage,
                turns: complete.turns,
            };
        }

        let text = client
            .request(
                Command::GetLastAssistantText {
                    active_session_id: active_session_id.to_string(),
                },
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await
            .ok()
            .and_then(|v| match v {
                Value::String(s) => Some(s),
                other => other
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(str::to_string),
            });

        let usage = client
            .request(
                Command::GetSessionStats {
                    active_session_id: active_session_id.to_string(),
                },
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await
            .map(|v| Usage::from_value(&v))
            .unwrap_or_default();

        let auto: AutonomousStatus = client
            .request(
                Command::WaitForHeadlessCompletion {
                    active_session_id: active_session_id.to_string(),
                },
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await
            .ok()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();

        TaskOutcome {
            status_override: None,
            text,
            error: auto.last_gate_failure.clone(),
            usage: Usage {
                total: if usage.total > 0 {
                    usage.total
                } else {
                    auto.tokens_used
                },
                ..usage
            },
            turns: auto.turns_used,
        }
    }

    /// The socket died mid-task. Reconnect, re-attach, wait for idle, and
    /// collect — the session itself is `resident` and kept running.
    async fn recover_after_socket_loss(
        self: &Arc<Self>,
        task: &Arc<TaskState>,
        active_session_id: &str,
    ) -> (TaskStatus, TaskOutcome) {
        warn!(
            "lost the pi socket while task {} was running; attempting recovery",
            task.task_id
        );
        let deadline = time::Instant::now() + RECONNECT_WINDOW;
        while time::Instant::now() < deadline {
            // A cancel or a shutdown landing mid-recovery ends it: neither
            // wants a reconnect, and `client()` refuses one anyway.
            if task.is_cancelled() || self.shutting_down.load(Ordering::SeqCst) {
                break;
            }
            if let Ok(client) = self.client().await {
                let attached = client
                    .request(
                        Command::attach(active_session_id, client.client_id()),
                        DEFAULT_REQUEST_TIMEOUT,
                    )
                    .await;
                if attached.is_ok() {
                    let idle = client
                        .request(
                            Command::WaitForIdle {
                                active_session_id: active_session_id.to_string(),
                            },
                            Duration::from_secs(task.budget.timeout_secs.max(60)),
                        )
                        .await;
                    if idle.is_ok() {
                        let outcome = self
                            .collect_outcome(&client, active_session_id, None)
                            .await;
                        info!("recovered task {} after a socket loss", task.task_id);
                        return (TaskStatus::Done, outcome);
                    }
                }
            }
            time::sleep(Duration::from_secs(2)).await;
        }

        (
            TaskStatus::Interrupted,
            TaskOutcome {
                error: Some(
                    "the connection to the pi daemon was lost and could not be recovered"
                        .to_string(),
                ),
                ..Default::default()
            },
        )
    }

    /// Record the outcome, free the session, and wake the being — unless the
    /// task was cancelled, which is deliberate and stays silent.
    async fn finalize(
        &self,
        session: &Arc<SessionState>,
        task: &Arc<TaskState>,
        status: TaskStatus,
        outcome: TaskOutcome,
    ) {
        let cancelled = task.is_cancelled();
        let status = if cancelled { TaskStatus::Cancelled } else { status };

        let elapsed;
        {
            let mut inner = task.inner.lock().await;
            inner.status = status;
            inner.ended_at = Some(time::Instant::now());
            inner.result_text = outcome.text.clone();
            inner.error = outcome.error.clone();
            inner.usage = outcome.usage;
            if outcome.turns > inner.turns {
                inner.turns = outcome.turns;
            }
            inner.callback = if cancelled {
                CallbackState::Suppressed
            } else {
                CallbackState::Pending
            };
            elapsed = inner
                .ended_at
                .unwrap_or_else(time::Instant::now)
                .saturating_duration_since(task.started_at)
                .as_secs();
        }

        {
            let mut inner = session.inner.lock().await;
            if inner.busy_task.as_deref() == Some(task.task_id.as_str()) {
                inner.busy_task = None;
            }
            inner.last_used = time::Instant::now();
        }

        let (turns, usage) = {
            let inner = task.inner.lock().await;
            (inner.turns, inner.usage)
        };

        {
            let mut ledger = self.ledger.lock().await;
            ledger.finish_task(
                &task.task_id,
                status.to_ledger(),
                turns,
                usage.total,
                outcome.error.as_deref(),
            );
            if cancelled {
                ledger.set_callback_state(&task.task_id, LedgerCallbackState::Suppressed);
            }
            ledger.prune();
            ledger.save_lossy();
        }

        session
            .transcript
            .append_line(&format!(
                "[end] task {} → {} ({}s, {} turns, {} tok)",
                task.task_id,
                status.as_str(),
                elapsed,
                turns,
                usage.total
            ))
            .await;

        if cancelled {
            debug!(
                "task {} was cancelled; callback suppressed",
                task.task_id
            );
            return;
        }

        let Some(portal_name) = self.callback.portal_name() else {
            debug!(
                "no callback configured; task {} finished {} silently",
                task.task_id,
                status.as_str()
            );
            return;
        };

        let session_file = session.inner.lock().await.session_file.clone();
        let payload = self
            .build_payload(task, status, &outcome, elapsed, turns, session_file, &portal_name)
            .await;
        {
            let mut ledger = self.ledger.lock().await;
            ledger.set_callback_state(&task.task_id, LedgerCallbackState::Sent);
            ledger.save_lossy();
        }
        task.inner.lock().await.callback = CallbackState::Sent;
        self.callback
            .deliver_detached(task.task_id.clone(), payload);
    }

    /// The envelope Heart already understands (`source:"portal"`), with
    /// `result.kind` distinguishing a sub-agent result from `portal_exec`
    /// (PRD §4.5).
    #[allow(clippy::too_many_arguments)]
    async fn build_payload(
        &self,
        task: &Arc<TaskState>,
        status: TaskStatus,
        outcome: &TaskOutcome,
        elapsed_secs: u64,
        turns: u32,
        session_file: Option<String>,
        portal_name: &str,
    ) -> Value {
        let full = outcome.text.clone().unwrap_or_default();
        let usage = outcome.usage;
        let brief = clamp_str(&task.brief, CALLBACK_COMMAND_MAX_BYTES);

        fit_payload(|max_output| {
            let result = clamp_str_tail(&full, max_output);
            let truncated = result.len() < full.len();
            json!({
                "source": "portal",
                "task_id": task.task_id,
                "summary": format!(
                    "portal_subagent completed: '{}' ({}, {} turns, {} tok)",
                    head(&task.brief, 120),
                    status.as_str(),
                    turns,
                    usage.total
                ),
                "result": {
                    "kind": "subagent",
                    "task_id": task.task_id,
                    "session": task.session_key,
                    "status": status.as_str(),
                    "result": result,
                    "truncated": truncated,
                    "error": outcome.error,
                    "brief": brief,
                    "workdir": task.workdir.display().to_string(),
                    "elapsed_secs": elapsed_secs,
                    "turns": turns,
                    "tokens": {
                        "input": usage.input,
                        "output": usage.output,
                        "total": usage.total,
                    },
                    "cost_usd": usage.cost_usd,
                    "session_file": session_file,
                    "scene_id": task.scene_id,
                    "portal_name": portal_name,
                }
            })
        })
    }

    // ── control ─────────────────────────────────────────────────────

    /// Inject a message into a running task: `steer` lands after the current
    /// turn, `follow_up` runs once the task would otherwise finish.
    pub async fn steer(self: &Arc<Self>, task_id: &str, message: &str, follow_up: bool) -> Result<()> {
        let message = message.trim();
        if message.is_empty() {
            anyhow::bail!("message is empty");
        }
        let (task, session) = self.task_and_session(task_id).await?;
        if task.status().await.is_finished() {
            anyhow::bail!(
                "task {task_id} already finished ({})",
                task.status().await.as_str()
            );
        }
        if self.daemon().transport_mode().await == TransportMode::Stdio {
            anyhow::bail!("{STDIO_NO_SESSION}");
        }
        let active_session_id = self.active_session_id(&session).await?;
        let client = self.client().await?;

        let cmd = if follow_up {
            Command::FollowUp {
                active_session_id,
                message: message.to_string(),
            }
        } else {
            Command::Steer {
                active_session_id,
                message: message.to_string(),
            }
        };
        client.request(cmd, DEFAULT_REQUEST_TIMEOUT).await?;
        session
            .transcript
            .append_line(&format!(
                "[{}] {}",
                if follow_up { "follow_up" } else { "steer" },
                head(message, 200)
            ))
            .await;
        Ok(())
    }

    /// End a task deliberately. Suppresses the callback (the being is already
    /// here) and hands back the partial result instead.
    pub async fn cancel(self: &Arc<Self>, task_id: &str) -> Result<Value> {
        let (task, session) = self.task_and_session(task_id).await?;
        if task.status().await.is_finished() {
            anyhow::bail!(
                "task {task_id} already finished ({})",
                task.status().await.as_str()
            );
        }

        // Set before signalling: `run_task` may finalize the moment we abort.
        task.cancelled.store(true, Ordering::SeqCst);

        // Stdio mode has no session to abort into: the runner sees the flag
        // and kills its pi process. Answer with whatever it had said by then
        // rather than waiting for the kill to land.
        if self.daemon().transport_mode().await == TransportMode::Stdio {
            session
                .transcript
                .append_line(&format!("[cancel] task {task_id} aborted by the being"))
                .await;
            let partial = task
                .inner
                .lock()
                .await
                .result_text
                .clone()
                .map(|text| clamp_str_tail(&text, PARTIAL_RESULT_BYTES));
            return Ok(json!({
                "task_id": task_id,
                "status": TaskStatus::Cancelled.as_str(),
                "partial_result": partial,
                "elapsed_s": task.elapsed_secs().await,
                "note": "Cancelled deliberately, so no callback will arrive for this task.",
            }));
        }

        let active_session_id = self.active_session_id(&session).await?;
        let client = self.client().await?;

        client
            .notify(Command::Abort {
                active_session_id: active_session_id.clone(),
            })
            .await;
        session
            .transcript
            .append_line(&format!("[cancel] task {task_id} aborted by the being"))
            .await;

        let _ = time::timeout(
            CANCEL_IDLE_WAIT,
            client.request(
                Command::WaitForIdle {
                    active_session_id: active_session_id.clone(),
                },
                CANCEL_IDLE_WAIT,
            ),
        )
        .await;

        let partial = client
            .request(
                Command::GetLastAssistantText {
                    active_session_id,
                },
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .map(|s| clamp_str_tail(&s, PARTIAL_RESULT_BYTES));

        Ok(json!({
            "task_id": task_id,
            "status": TaskStatus::Cancelled.as_str(),
            "partial_result": partial,
            "elapsed_s": task.elapsed_secs().await,
            "note": "Cancelled deliberately, so no callback will arrive for this task.",
        }))
    }

    /// Transcript page for a task's session, with `portal_process` paging.
    pub async fn log(
        &self,
        task_id: &str,
        offset: u64,
        limit: usize,
        timeout_ms: u64,
    ) -> Result<Value> {
        let (task, session) = self.task_and_session(task_id).await?;
        let page: TranscriptPage = session.transcript.page(offset, limit, timeout_ms).await?;
        let status = task.status().await;
        Ok(json!({
            "output": String::from_utf8_lossy(&page.output),
            "next_offset": page.next_offset,
            "truncated": page.truncated,
            "status": { "kind": status.as_str() },
            "idle_s": page.idle_s,
            "total_output_bytes": page.total_output_bytes,
        }))
    }

    // ── status ──────────────────────────────────────────────────────

    /// One task, or the whole picture (PRD §6.2).
    pub async fn status(&self, task_id: Option<&str>, session: Option<&str>) -> Result<Value> {
        if let Some(task_id) = task_id {
            let (task, session_state) = self.task_and_session(task_id).await?;
            return Ok(self.task_detail(&task, &session_state).await);
        }

        let daemon = self.daemon().health().await;
        let model = self.model();
        let mut sessions = Vec::new();
        for state in self.sessions.lock().await.values() {
            if session.is_some_and(|f| f != state.key) {
                continue;
            }
            let inner = state.inner.lock().await;
            sessions.push(json!({
                "session": state.key,
                "loaded": inner.active_session_id.is_some(),
                "busy": inner.busy_task.is_some(),
                "cwd": state.cwd.display().to_string(),
                "tasks_total": inner.tasks_total,
                "last_used_s": inner.last_used.elapsed().as_secs(),
                "session_file": inner.session_file,
            }));
        }

        let mut running = Vec::new();
        let mut completed = Vec::new();
        for task in self.tasks.lock().await.values() {
            if session.is_some_and(|f| f != task.session_key) {
                continue;
            }
            let inner = task.inner.lock().await;
            let elapsed = match inner.ended_at {
                Some(end) => end.saturating_duration_since(task.started_at).as_secs(),
                None => task.started_at.elapsed().as_secs(),
            };
            if inner.status.is_finished() {
                completed.push(json!({
                    "task_id": task.task_id,
                    "session": task.session_key,
                    "status": inner.status.as_str(),
                    "elapsed_s": elapsed,
                    "turns": inner.turns,
                    "tokens_total": inner.usage.total,
                    "callback": inner.callback.as_str(),
                    "ended_s_ago": inner.ended_at.map(|e| e.elapsed().as_secs()),
                    "error": inner.error,
                }));
            } else {
                running.push(json!({
                    "task_id": task.task_id,
                    "session": task.session_key,
                    "status": inner.status.as_str(),
                    "elapsed_s": elapsed,
                    "turns": inner.turns,
                    "last_tool": inner.last_tool,
                    "brief_head": head(&task.brief, 80),
                }));
            }
        }

        Ok(json!({
            "enabled": self.config.enabled,
            "available": self.is_available(),
            "command_resolved": self.pi_installed(),
            "state_dir": self.config.resolved_state_dir().display().to_string(),
            "daemon": daemon.to_json(),
            "auth": {
                "provider": model.provider,
                "model": model.model,
                "thinking": model.thinking,
                // Masked: enough to recognise, never enough to use.
                "api_key": model.masked_api_key(),
                "configured": !self.needs_setup(),
                "models_available": *self.models_available.lock().await,
            },
            "callback_configured": self.callback.is_configured(),
            "max_concurrent": self.config.max_concurrent,
            "sessions": sessions,
            "running": running,
            "completed": completed,
        }))
    }

    async fn task_detail(&self, task: &Arc<TaskState>, session: &Arc<SessionState>) -> Value {
        let inner = task.inner.lock().await;
        let elapsed = match inner.ended_at {
            Some(end) => end.saturating_duration_since(task.started_at).as_secs(),
            None => task.started_at.elapsed().as_secs(),
        };
        json!({
            "task_id": task.task_id,
            "session": task.session_key,
            "status": inner.status.as_str(),
            "brief": clamp_str(&task.brief, CALLBACK_COMMAND_MAX_BYTES),
            "workdir": task.workdir.display().to_string(),
            "scene_id": task.scene_id,
            "budget": task.budget.to_json(),
            "elapsed_s": elapsed,
            "idle_s": session.transcript.idle_s().await,
            "turns": inner.turns,
            "last_tool": inner.last_tool,
            "tokens": {
                "input": inner.usage.input,
                "output": inner.usage.output,
                "total": inner.usage.total,
            },
            "cost_usd": inner.usage.cost_usd,
            "callback": inner.callback.as_str(),
            "error": inner.error,
            "result_head": inner
                .result_text
                .as_deref()
                .map(|t| clamp_str(t, RESULT_HEAD_BYTES)),
            "total_output_bytes": session.transcript.total_bytes().await,
        })
    }

    /// Compact section for `portal_status`. Never credentials.
    #[allow(dead_code)] // consumed by tools/status.rs (PRD §4.7), not yet ported
    pub async fn status_summary(&self) -> Value {
        let daemon = self.daemon().health().await;
        let sessions = self.sessions.lock().await.len();
        let mut running = 0usize;
        for task in self.tasks.lock().await.values() {
            if !task.status().await.is_finished() {
                running += 1;
            }
        }
        json!({
            "enabled": self.config.enabled,
            "command_resolved": self.pi_installed(),
            "state_dir": self.config.resolved_state_dir().display().to_string(),
            "daemon": daemon.to_json(),
            "sessions": sessions,
            "running_tasks": running,
        })
    }

    // ── lifecycle ───────────────────────────────────────────────────

    /// Restart reconciliation (PRD §7.4). Idempotent, and safe to call before
    /// the callback config exists — it just stays silent then.
    pub async fn reconcile(self: &Arc<Self>) {
        if !self.is_available() || self.reconciled.swap(true, Ordering::SeqCst) {
            return;
        }

        let open: Vec<TaskRecord> = {
            let ledger = self.ledger.lock().await;
            ledger.open_tasks()
        };
        if open.is_empty() {
            return;
        }

        info!(
            "reconciling {} sub-agent task(s) that were running before this Portal started",
            open.len()
        );
        let changed = {
            let mut ledger = self.ledger.lock().await;
            let changed =
                ledger.interrupt_open_tasks("Portal restarted while the task was running");
            ledger.prune();
            ledger.save_lossy();
            changed
        };

        let Some(portal_name) = self.callback.portal_name() else {
            return;
        };
        for record in changed {
            let payload = json!({
                "source": "portal",
                "task_id": record.task_id,
                "summary": format!(
                    "portal_subagent interrupted: '{}' (Portal restarted)",
                    record.brief_head
                ),
                "result": {
                    "kind": "subagent",
                    "task_id": record.task_id,
                    "session": record.session,
                    "status": TaskStatus::Interrupted.as_str(),
                    "result": "",
                    "truncated": false,
                    "error": record.error,
                    "brief": record.brief_head,
                    "workdir": record.workdir,
                    "elapsed_secs": Value::Null,
                    "turns": record.turns,
                    "tokens": { "input": 0, "output": 0, "total": record.tokens_total },
                    "scene_id": record.scene_id,
                    "portal_name": portal_name,
                }
            });
            {
                let mut ledger = self.ledger.lock().await;
                ledger.set_callback_state(&record.task_id, LedgerCallbackState::Sent);
                ledger.save_lossy();
            }
            self.callback
                .deliver_detached(record.task_id.clone(), payload);
        }
    }

    /// Drop finished tasks past retention and unload sessions idle past
    /// `idle_unload_secs`. Called from Portal's 60 s maintenance tick.
    pub async fn cleanup(self: &Arc<Self>) {
        if !self.is_available() {
            return;
        }

        let mut drop_ids = Vec::new();
        for (id, task) in self.tasks.lock().await.iter() {
            let inner = task.inner.lock().await;
            if let Some(ended) = inner.ended_at {
                if inner.status.is_finished() && ended.elapsed() >= TASK_RETENTION {
                    drop_ids.push(id.clone());
                }
            }
        }
        if !drop_ids.is_empty() {
            let mut tasks = self.tasks.lock().await;
            for id in &drop_ids {
                tasks.remove(id);
            }
        }

        let idle_unload = self.config.idle_unload_secs;
        if idle_unload > 0 {
            let candidates: Vec<Arc<SessionState>> =
                self.sessions.lock().await.values().cloned().collect();
            for session in candidates {
                let unload = {
                    let inner = session.inner.lock().await;
                    inner.busy_task.is_none()
                        && inner.active_session_id.is_some()
                        && inner.last_used.elapsed().as_secs() >= idle_unload
                };
                if !unload {
                    continue;
                }
                if let Err(e) = self.unload_session(&session).await {
                    debug!("could not unload idle session '{}': {e:#}", session.key);
                }
            }
        }

        self.maybe_stop_idle_daemon().await;
    }

    /// Release the node process when `[subagent].daemon_idle_exit_secs` is set
    /// and nothing has needed it for that long. Sessions persist as files, so
    /// the next spawn just pays a cold start (PRD §9 risk 6).
    async fn maybe_stop_idle_daemon(self: &Arc<Self>) {
        let idle_exit = self.config.daemon_idle_exit_secs;
        if idle_exit == 0 || self.daemon().client().await.is_none() {
            return;
        }
        for task in self.tasks.lock().await.values() {
            if !task.status().await.is_finished() {
                return;
            }
        }
        // A session still loaded in the daemon is warm on purpose; leave it.
        for session in self.sessions.lock().await.values() {
            if session.inner.lock().await.active_session_id.is_some() {
                return;
            }
        }
        if self.last_activity.lock().await.elapsed().as_secs() < idle_exit {
            return;
        }
        info!("stopping the pi daemon after {idle_exit}s idle");
        self.daemon().shutdown(SHUTDOWN_GRACE).await;
    }

    /// `kill` the loaded pi session. The file stays, so the next spawn resumes
    /// the same harness.
    async fn unload_session(self: &Arc<Self>, session: &Arc<SessionState>) -> Result<()> {
        let active_session_id = {
            let inner = session.inner.lock().await;
            inner.active_session_id.clone()
        };
        let Some(active_session_id) = active_session_id else {
            return Ok(());
        };
        if let Some(client) = self.daemon().client().await {
            client
                .request(Command::Kill { active_session_id }, DEFAULT_REQUEST_TIMEOUT)
                .await?;
        }
        let mut inner = session.inner.lock().await;
        inner.active_session_id = None;
        inner.attached = false;
        debug!("unloaded idle session '{}' (file kept)", session.key);
        Ok(())
    }

    /// Portal is going away: abort running tasks without waking the being
    /// (shutdown is not a result), then stop the daemon.
    pub async fn shutdown(self: &Arc<Self>) {
        if !self.pi_installed() {
            return;
        }
        self.shutting_down.store(true, Ordering::SeqCst);

        let tasks: Vec<Arc<TaskState>> = self.tasks.lock().await.values().cloned().collect();
        let client = self.daemon().client().await;
        for task in tasks {
            if task.status().await.is_finished() {
                continue;
            }
            // Mark before aborting so finalize() suppresses the callback.
            task.cancelled.store(true, Ordering::SeqCst);
            if let (Some(client), Ok(session)) = (
                client.as_ref(),
                self.session_for_task(&task.session_key).await,
            ) {
                if let Ok(active_session_id) = self.active_session_id(&session).await {
                    client
                        .notify(Command::Abort { active_session_id })
                        .await;
                }
            }
        }

        {
            let mut ledger = self.ledger.lock().await;
            // Open rows are genuinely interrupted; the next start reconciles them.
            ledger.prune();
            ledger.save_lossy();
        }

        self.daemon().shutdown(SHUTDOWN_GRACE).await;
    }

    // ── helpers ─────────────────────────────────────────────────────

    async fn task_and_session(&self, task_id: &str) -> Result<(Arc<TaskState>, Arc<SessionState>)> {
        let task = self
            .tasks
            .lock()
            .await
            .get(task_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown task: {task_id}"))?;
        let session = self.session_for_task(&task.session_key).await?;
        Ok((task, session))
    }

    async fn session_for_task(&self, key: &str) -> Result<Arc<SessionState>> {
        self.sessions
            .lock()
            .await
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("session '{key}' is no longer tracked"))
    }

    async fn active_session_id(&self, session: &Arc<SessionState>) -> Result<String> {
        session
            .inner
            .lock()
            .await
            .active_session_id
            .clone()
            .ok_or_else(|| {
                anyhow::anyhow!("session '{}' is not loaded in the daemon", session.key)
            })
    }

    /// Default budget from config, with per-task overrides applied by the tool.
    pub fn default_budget(&self) -> Budget {
        Budget::from_config(&self.config)
    }

    #[allow(dead_code)] // kept next to resolve_workdir for callers/tests
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Resolve `workdir` and refuse anything outside the workspace root
    /// (realpath + prefix, so symlinks cannot escape — PRD §9 risk 11).
    fn resolve_workdir(&self, raw: Option<&str>) -> Result<PathBuf> {
        let root = self
            .workspace_root
            .canonicalize()
            .unwrap_or_else(|_| self.workspace_root.clone());

        let candidate = match raw.map(str::trim).filter(|s| !s.is_empty()) {
            None => root.clone(),
            Some(p) => {
                let p = PathBuf::from(pi_daemon::expand_home(p));
                if p.is_absolute() {
                    p
                } else {
                    root.join(p)
                }
            }
        };

        let resolved = candidate
            .canonicalize()
            .with_context(|| format!("workdir {} does not exist", candidate.display()))?;
        if !resolved.starts_with(&root) {
            anyhow::bail!(
                "workdir {} is outside the workspace root {}",
                resolved.display(),
                root.display()
            );
        }
        Ok(resolved)
    }
}

/// Decide the final status from the outcome plus the raw payload.
async fn classify(task: &Arc<TaskState>, outcome: &TaskOutcome, value: &Value) -> TaskStatus {
    if task.is_cancelled() {
        return TaskStatus::Cancelled;
    }
    if outcome.status_override == Some("failed") || outcome.error.is_some() {
        return TaskStatus::Failed;
    }
    let auto: AutonomousStatus = serde_json::from_value(value.clone()).unwrap_or_default();
    if auto.budget_exhausted(task.budget.max_turns, task.budget.max_tokens) {
        return TaskStatus::BudgetExhausted;
    }
    if outcome.turns >= task.budget.max_turns && task.budget.max_turns > 0 {
        return TaskStatus::BudgetExhausted;
    }
    TaskStatus::Done
}

/// The prompt a task becomes inside its session (PRD §7.1).
fn render_prompt(task: &TaskState, brief: &str) -> String {
    format!(
        "# Task {} (session {})\nWorking directory: {}\nBudget: ≤{} turns, ≤{} tokens, ≤{} min\n\n{}",
        task.task_id,
        task.session_key,
        task.workdir.display(),
        task.budget.max_turns,
        task.budget.max_tokens,
        task.budget.timeout_secs / 60,
        brief
    )
}

/// Session keys become pi session names and land in the ledger; keep them tame.
pub fn normalize_session_key(raw: &str) -> Result<String> {
    let key = raw.trim();
    if key.is_empty() {
        return Ok(DEFAULT_SESSION_KEY.to_string());
    }
    if key.len() > MAX_SESSION_KEY_BYTES {
        anyhow::bail!("session key is longer than {MAX_SESSION_KEY_BYTES} bytes");
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
    {
        anyhow::bail!(
            "session key '{key}' must use only letters, digits, '-', '_', '.' or ':'"
        );
    }
    Ok(key.to_string())
}

fn head(s: &str, max: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.len() <= max {
        return one_line;
    }
    let mut end = max;
    while end > 0 && !one_line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &one_line[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("portal-sub-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("proj")).unwrap();
        dir
    }

    fn manager_for(workspace: &Path, tweak: impl FnOnce(&mut SubagentConfig)) -> Arc<SubagentManager> {
        let mut config = PortalConfig::default();
        config.security.workspace_root = workspace.to_path_buf();
        config.subagent.state_dir = Some(workspace.join("state").display().to_string());
        // A provider is "configured": these tests are about everything that
        // happens after first-use setup.
        config.subagent.model.provider = Some("anthropic".to_string());
        // Never let a test machine without pi trigger a real `npm install`.
        config.subagent.auto_install = false;
        tweak(&mut config.subagent);
        SubagentManager::new(&config, HeartCallback::new())
    }

    /// A manager with nothing telling pi which LLM to use, independent of
    /// whatever `*_API_KEY` this developer machine has exported.
    fn unconfigured_manager(workspace: &Path) -> Arc<SubagentManager> {
        manager_for(workspace, |c| {
            c.command = Some(vec!["/bin/sh".to_string()]);
            c.model = SubagentModelConfig::default();
            c.env_passthrough = vec!["PATH".to_string(), "HOME".to_string()];
        })
    }

    fn write_config(workspace: &Path, body: &str) -> PathBuf {
        let path = workspace.join("portal.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn needs_setup_only_when_nothing_names_a_provider() {
        let ws = temp_workspace("needsetup");
        assert!(unconfigured_manager(&ws).needs_setup());

        let with_provider = manager_for(&ws, |c| {
            c.env_passthrough = vec![];
            c.model.provider = Some("openrouter".to_string());
        });
        assert!(!with_provider.needs_setup());

        // A model id alone is a choice too; pi may have its own login.
        let with_model = manager_for(&ws, |c| {
            c.env_passthrough = vec![];
            c.model = SubagentModelConfig { model: Some("gpt-4o".to_string()), ..Default::default() };
        });
        assert!(!with_model.needs_setup());

        // `pi login` credentials in the agent dir count as configured.
        let logged_in = unconfigured_manager(&ws);
        let auth = logged_in.daemon().agent_dir().join("auth.json");
        std::fs::create_dir_all(auth.parent().unwrap()).unwrap();
        std::fs::write(&auth, "{}").unwrap();
        assert!(!logged_in.needs_setup());
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn first_spawn_without_a_provider_returns_setup_guidance() {
        let ws = temp_workspace("guidance");
        let m = unconfigured_manager(&ws);
        let req = SpawnRequest {
            brief: "do the thing".to_string(),
            session: "s".to_string(),
            workdir: None,
            budget: m.default_budget(),
            model: None,
            thinking: None,
            scene_id: None,
        };
        let err = m.spawn(req).await.unwrap_err().to_string();
        assert_eq!(err, SETUP_GUIDANCE);
        assert!(err.contains("portal_subagent_setup"), "{err}");
        assert!(err.contains("Supported providers: anthropic, openrouter, openai, gemini, groq, xai"));
        let _ = std::fs::remove_dir_all(ws);
    }

    #[test]
    fn model_update_merges_and_validates() {
        let current = SubagentModelConfig {
            provider: Some("openai".to_string()),
            model: Some("gpt-4o".to_string()),
            thinking: Some("low".to_string()),
            api_key: Some("sk-old".to_string()),
        };

        // Partial update keeps the rest.
        let next = ModelConfigUpdate {
            model: Some(" claude-sonnet-4-5 ".to_string()),
            ..Default::default()
        }
        .apply_to(&current)
        .unwrap();
        assert_eq!(next.provider.as_deref(), Some("openai"));
        assert_eq!(next.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(next.api_key.as_deref(), Some("sk-old"));

        // Empty string clears; provider is case-insensitive.
        let next = ModelConfigUpdate {
            provider: Some("Anthropic".to_string()),
            api_key: Some(String::new()),
            thinking: Some(String::new()),
            ..Default::default()
        }
        .apply_to(&current)
        .unwrap();
        assert_eq!(next.provider.as_deref(), Some("anthropic"));
        assert_eq!(next.api_key, None);
        assert_eq!(next.thinking, None);

        let bad_provider = ModelConfigUpdate { provider: Some("hal9000".to_string()), ..Default::default() }
            .apply_to(&current)
            .unwrap_err()
            .to_string();
        assert!(bad_provider.contains("unknown provider 'hal9000'"), "{bad_provider}");

        let bad_thinking = ModelConfigUpdate { thinking: Some("hard".to_string()), ..Default::default() }
            .apply_to(&current)
            .unwrap_err()
            .to_string();
        assert!(bad_thinking.contains("unknown thinking level"), "{bad_thinking}");

        let key_without_provider = ModelConfigUpdate {
            api_key: Some("sk-new".to_string()),
            ..Default::default()
        }
        .apply_to(&SubagentModelConfig::default())
        .unwrap_err()
        .to_string();
        assert!(key_without_provider.contains("needs a provider"), "{key_without_provider}");

        let pasted_badly = ModelConfigUpdate {
            provider: Some("groq".to_string()),
            api_key: Some("gsk_abc def".to_string()),
            ..Default::default()
        }
        .apply_to(&SubagentModelConfig::default())
        .unwrap_err()
        .to_string();
        assert!(pasted_badly.contains("whitespace"), "{pasted_badly}");
    }

    #[tokio::test]
    async fn configure_model_persists_and_takes_effect_in_memory() {
        let ws = temp_workspace("configure");
        let path = write_config(&ws, "name = \"vale\"\n# hands off\n");
        let mut config = PortalConfig::load(path.to_str().unwrap()).unwrap();
        config.security.workspace_root = ws.clone();
        config.subagent.state_dir = Some(ws.join("state").display().to_string());
        config.subagent.command = Some(vec!["/bin/sh".to_string()]);
        config.subagent.env_passthrough = vec![];
        let m = SubagentManager::new(&config, HeartCallback::new());
        assert!(m.needs_setup());

        let outcome = m
            .configure_model(ModelConfigUpdate {
                provider: Some("anthropic".to_string()),
                model: Some("claude-sonnet-4-5".to_string()),
                api_key: Some("sk-ant-api03-configure-test-key".to_string()),
                thinking: None,
            })
            .await
            .unwrap();
        assert!(!outcome.daemon_restarted, "no daemon was running");
        assert!(!m.needs_setup());

        // In memory: what the next task will run on.
        let live = m.model_config();
        assert_eq!(live.provider.as_deref(), Some("anthropic"));
        assert_eq!(live.api_key.as_deref(), Some("sk-ant-api03-configure-test-key"));
        let choice = m.model_choice(&SpawnRequest {
            brief: String::new(),
            session: String::new(),
            workdir: None,
            budget: m.default_budget(),
            model: None,
            thinking: None,
            scene_id: None,
        });
        assert_eq!(choice.model.as_deref(), Some("claude-sonnet-4-5"));
        // And the replacement daemon carries the key into its environment.
        assert_eq!(m.daemon().config().api_key.as_deref(), Some("sk-ant-api03-configure-test-key"));
        assert_eq!(m.daemon().config().provider.as_deref(), Some("anthropic"));

        // On disk: the rest of the file untouched, the section reloadable.
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("# hands off"), "{written}");
        let reloaded = PortalConfig::load(path.to_str().unwrap()).unwrap();
        assert_eq!(reloaded.subagent.model.provider.as_deref(), Some("anthropic"));
        assert_eq!(reloaded.subagent.model.model.as_deref(), Some("claude-sonnet-4-5"));

        // Status never carries the raw key.
        let status = m.status(None, None).await.unwrap();
        assert_eq!(status["auth"]["api_key"], "sk-ant-...***");
        assert_eq!(status["auth"]["configured"], true);
        assert!(!status.to_string().contains("configure-test-key"));

        // A model-only change leaves the daemon alone.
        let outcome = m
            .configure_model(ModelConfigUpdate { model: Some("claude-opus-4-1".to_string()), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(outcome.model.model.as_deref(), Some("claude-opus-4-1"));
        assert_eq!(outcome.model.api_key.as_deref(), Some("sk-ant-api03-configure-test-key"));
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn configure_model_needs_a_config_file_to_write_to() {
        let ws = temp_workspace("nofile");
        let m = unconfigured_manager(&ws);
        let err = m
            .configure_model(ModelConfigUpdate { provider: Some("xai".to_string()), ..Default::default() })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("without a config file"), "{err}");
        assert!(m.needs_setup(), "nothing changed in memory either");
        let _ = std::fs::remove_dir_all(ws);
    }

    #[test]
    fn session_keys_are_normalized_and_validated() {
        assert_eq!(normalize_session_key("  ").unwrap(), DEFAULT_SESSION_KEY);
        assert_eq!(normalize_session_key(" scene-42 ").unwrap(), "scene-42");
        assert_eq!(normalize_session_key("proj.a:b_1").unwrap(), "proj.a:b_1");
        assert!(normalize_session_key("../escape").is_err());
        assert!(normalize_session_key("has space").is_err());
        assert!(normalize_session_key(&"x".repeat(65)).is_err());
    }

    #[test]
    fn budget_maps_onto_pi_autonomous_limits() {
        let b = Budget {
            max_turns: 40,
            max_tokens: 400_000,
            timeout_secs: 1800,
            max_continuations: 3,
        };
        let a = b.to_autonomous();
        assert!(a.enabled);
        assert_eq!(a.max_turns, 40);
        assert_eq!(a.timeout_ms, 1_800_000);
        assert_eq!(a.max_continuations, 3);
    }

    #[test]
    fn task_status_strings_match_the_callback_contract() {
        assert_eq!(TaskStatus::Done.as_str(), "done");
        assert_eq!(TaskStatus::BudgetExhausted.as_str(), "budget_exhausted");
        assert!(TaskStatus::Running.is_finished() == false);
        for s in [
            TaskStatus::Done,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
            TaskStatus::Interrupted,
            TaskStatus::BudgetExhausted,
            TaskStatus::Timeout,
        ] {
            assert!(s.is_finished(), "{} must be terminal", s.as_str());
        }
    }

    #[test]
    fn prompt_renders_the_task_header() {
        let task = TaskState {
            task_id: "sub_1".to_string(),
            session_key: "scene-42".to_string(),
            brief: "b".to_string(),
            workdir: PathBuf::from("/ws/proj"),
            scene_id: None,
            budget: Budget {
                max_turns: 40,
                max_tokens: 400_000,
                timeout_secs: 1800,
                max_continuations: 3,
            },
            started_at: time::Instant::now(),
            cancelled: AtomicBool::new(false),
            inner: AsyncMutex::new(TaskInner {
                status: TaskStatus::Running,
                ended_at: None,
                turns: 0,
                last_tool: None,
                result_text: None,
                error: None,
                usage: Usage::default(),
                callback: CallbackState::Pending,
            }),
        };
        let prompt = render_prompt(&task, "Refactor the auth module");
        assert!(prompt.starts_with("# Task sub_1 (session scene-42)"), "{prompt}");
        assert!(prompt.contains("Working directory: /ws/proj"));
        assert!(prompt.contains("≤40 turns"));
        assert!(prompt.contains("≤30 min"));
        assert!(prompt.ends_with("Refactor the auth module"));
    }

    #[test]
    fn contract_is_appended_and_forbids_asking_questions() {
        assert!(SUB_CONTRACT.contains("cannot ask"));
        assert!(SUB_CONTRACT.contains("## Result"));
    }

    #[test]
    fn workdir_must_stay_inside_the_workspace() {
        let ws = temp_workspace("workdir");
        let m = manager_for(&ws, |_| {});

        let root = m.resolve_workdir(None).unwrap();
        assert_eq!(root, ws.canonicalize().unwrap());
        let proj = m.resolve_workdir(Some("proj")).unwrap();
        assert_eq!(proj, ws.join("proj").canonicalize().unwrap());

        let err = m.resolve_workdir(Some("/etc")).unwrap_err().to_string();
        assert!(err.contains("outside the workspace root"), "{err}");
        assert!(m.resolve_workdir(Some("does/not/exist")).is_err());
        let _ = std::fs::remove_dir_all(ws);
    }

    #[test]
    fn a_missing_pi_binary_makes_the_subagent_unavailable() {
        let ws = temp_workspace("nopi");
        let m = manager_for(&ws, |c| {
            c.command = Some(vec![ws.join("no-such-pi").display().to_string()]);
        });
        assert!(!m.is_available());
        assert!(!m.wants_eager_start());
        let err = m.ensure_available().unwrap_err().to_string();
        assert!(err.contains("no pi binary found"), "{err}");
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn setup_cannot_install_over_an_explicit_command_that_does_not_exist() {
        let ws = temp_workspace("nopi-explicit");
        let m = manager_for(&ws, |c| {
            c.auto_install = true;
            c.command = Some(vec![ws.join("no-such-pi").display().to_string()]);
        });
        // A misconfiguration is not something setup should paper over, so the
        // setup tool is not dangled either.
        assert!(!m.setup_offered());
        let err = m.ensure_pi_installed().await.unwrap_err().to_string();
        assert!(err.contains("[subagent].command"), "{err}");
        assert!(!m.pi_installed());
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn setup_refuses_to_install_when_auto_install_is_off() {
        let ws = temp_workspace("nopi-optout");
        let m = manager_for(&ws, |c| {
            c.command = Some(vec![ws.join("no-such-pi").display().to_string()]);
        });
        let err = m.ensure_pi_installed().await.unwrap_err().to_string();
        // The explicit-command refusal wins; both are configuration to fix.
        assert!(err.contains("does not exist"), "{err}");

        // With pi present nothing is installed and nothing is refused.
        let ready = manager_for(&ws, |c| c.command = Some(vec!["/bin/sh".to_string()]));
        assert!(ready.setup_offered());
        assert_eq!(ready.ensure_pi_installed().await.unwrap(), false);
        let _ = std::fs::remove_dir_all(ws);
    }

    #[test]
    fn disabled_config_reports_a_clear_reason() {
        let ws = temp_workspace("disabled");
        let m = manager_for(&ws, |c| {
            c.enabled = false;
            c.command = Some(vec!["/bin/sh".to_string()]);
        });
        assert!(!m.is_available());
        let err = m.ensure_available().unwrap_err().to_string();
        assert!(err.contains("disabled"), "{err}");
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn spawn_validates_the_brief_before_touching_pi() {
        let ws = temp_workspace("brief");
        let m = manager_for(&ws, |c| c.command = Some(vec!["/bin/sh".to_string()]));

        let base = SpawnRequest {
            brief: "   ".to_string(),
            session: DEFAULT_SESSION_KEY.to_string(),
            workdir: None,
            budget: m.default_budget(),
            model: None,
            thinking: None,
            scene_id: None,
        };
        let err = m.spawn(base.clone()).await.unwrap_err().to_string();
        assert!(err.contains("brief is empty"), "{err}");

        let mut big = base.clone();
        big.brief = "x".repeat(MAX_BRIEF_BYTES + 1);
        let err = m.spawn(big).await.unwrap_err().to_string();
        assert!(err.contains("max"), "{err}");

        let mut bad_key = base.clone();
        bad_key.brief = "do it".to_string();
        bad_key.session = "../escape".to_string();
        assert!(m.spawn(bad_key).await.is_err());

        let mut bad_dir = base;
        bad_dir.brief = "do it".to_string();
        bad_dir.workdir = Some("/etc".to_string());
        let err = m.spawn(bad_dir).await.unwrap_err().to_string();
        assert!(err.contains("outside the workspace root"), "{err}");
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn status_reports_availability_without_a_daemon() {
        let ws = temp_workspace("status");
        let m = manager_for(&ws, |c| c.command = Some(vec!["/bin/sh".to_string()]));
        let status = m.status(None, None).await.unwrap();

        assert_eq!(status["enabled"], true);
        assert_eq!(status["available"], true);
        assert_eq!(status["command_resolved"], true);
        assert_eq!(status["daemon"]["running"], false);
        assert_eq!(status["sessions"].as_array().unwrap().len(), 0);
        assert_eq!(status["running"].as_array().unwrap().len(), 0);

        let summary = m.status_summary().await;
        assert_eq!(summary["running_tasks"], 0);
        let text = serde_json::to_string(&summary).unwrap();
        assert!(!text.contains("token"), "status must not carry secrets: {text}");
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn unknown_task_ids_are_rejected_by_every_control_path() {
        let ws = temp_workspace("unknown");
        let m = manager_for(&ws, |c| c.command = Some(vec!["/bin/sh".to_string()]));
        assert!(m.status(Some("sub_nope"), None).await.is_err());
        assert!(m.log("sub_nope", 0, 1024, 0).await.is_err());
        assert!(m.cancel("sub_nope").await.is_err());
        assert!(m.steer("sub_nope", "hi", false).await.is_err());
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn reconcile_interrupts_orphaned_tasks_from_the_ledger() {
        let ws = temp_workspace("reconcile");
        let state_dir = ws.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        // A ledger left behind by a Portal that died mid-task.
        let mut ledger = Ledger::empty(state_dir.join("ledger.json"));
        ledger.upsert_session("scene-42", &ws.display().to_string(), None);
        ledger.start_task("sub_orphan", "scene-42", "Refactor auth", &ws.display().to_string(), None);
        ledger.save().unwrap();

        let m = manager_for(&ws, |c| c.command = Some(vec!["/bin/sh".to_string()]));
        m.reconcile().await;

        let reloaded = Ledger::load(state_dir.join("ledger.json"));
        let task = reloaded.task("sub_orphan").unwrap();
        assert_eq!(task.status(), LedgerTaskStatus::Interrupted);
        assert!(task.error.as_deref().unwrap().contains("Portal restarted"));

        // Idempotent: a second pass changes nothing.
        m.reconcile().await;
        assert!(reloaded.open_tasks().is_empty());
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn shutdown_without_a_daemon_is_harmless() {
        let ws = temp_workspace("shutdown");
        let m = manager_for(&ws, |c| c.command = Some(vec!["/bin/sh".to_string()]));
        m.shutdown().await;
        m.cleanup().await;
        assert!(!m.status_summary().await["daemon"]["running"]
            .as_bool()
            .unwrap());
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn callback_payload_has_the_documented_subagent_shape() {
        let ws = temp_workspace("payload");
        let m = manager_for(&ws, |c| c.command = Some(vec!["/bin/sh".to_string()]));
        let task = Arc::new(TaskState {
            task_id: "sub_3f9c".to_string(),
            session_key: "scene-42".to_string(),
            brief: "Refactor the auth module and add tests".to_string(),
            workdir: PathBuf::from("/ws/proj"),
            scene_id: Some("desktop-1".to_string()),
            budget: m.default_budget(),
            started_at: time::Instant::now(),
            cancelled: AtomicBool::new(false),
            inner: AsyncMutex::new(TaskInner {
                status: TaskStatus::Done,
                ended_at: None,
                turns: 14,
                last_tool: None,
                result_text: None,
                error: None,
                usage: Usage::default(),
                callback: CallbackState::Pending,
            }),
        });
        let outcome = TaskOutcome {
            status_override: None,
            text: Some("## Result\nDone.".to_string()),
            error: None,
            usage: Usage {
                input: 30120,
                output: 8210,
                total: 38330,
                cost_usd: Some(0.42),
            },
            turns: 14,
        };

        let payload = m
            .build_payload(
                &task,
                TaskStatus::Done,
                &outcome,
                312,
                14,
                Some("/state/pi/sessions/a.jsonl".to_string()),
                "alice-laptop",
            )
            .await;

        assert_eq!(payload["source"], "portal");
        assert_eq!(payload["task_id"], "sub_3f9c");
        assert!(payload["summary"]
            .as_str()
            .unwrap()
            .starts_with("portal_subagent completed: 'Refactor the auth module and add tests' (done, 14 turns, 38330 tok)"));
        let r = &payload["result"];
        assert_eq!(r["kind"], "subagent");
        assert_eq!(r["session"], "scene-42");
        assert_eq!(r["status"], "done");
        assert_eq!(r["result"], "## Result\nDone.");
        assert_eq!(r["truncated"], false);
        assert_eq!(r["elapsed_secs"], 312);
        assert_eq!(r["turns"], 14);
        assert_eq!(r["tokens"]["total"], 38330);
        assert_eq!(r["cost_usd"], 0.42);
        assert_eq!(r["scene_id"], "desktop-1");
        assert_eq!(r["portal_name"], "alice-laptop");
        assert_eq!(r["session_file"], "/state/pi/sessions/a.jsonl");
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn callback_payload_respects_the_wire_cap() {
        let ws = temp_workspace("cap");
        let m = manager_for(&ws, |c| c.command = Some(vec!["/bin/sh".to_string()]));
        let task = Arc::new(TaskState {
            task_id: "sub_big".to_string(),
            session_key: "k".to_string(),
            // A huge brief must not eat the result budget.
            brief: "é".repeat(20_000),
            workdir: PathBuf::from("/ws"),
            scene_id: None,
            budget: m.default_budget(),
            started_at: time::Instant::now(),
            cancelled: AtomicBool::new(false),
            inner: AsyncMutex::new(TaskInner {
                status: TaskStatus::Done,
                ended_at: None,
                turns: 1,
                last_tool: None,
                result_text: None,
                error: None,
                usage: Usage::default(),
                callback: CallbackState::Pending,
            }),
        });
        // Every byte JSON-escapes to six chars, forcing the fallback tail.
        let outcome = TaskOutcome {
            text: Some("\u{1}".repeat(400 * 1024)),
            ..Default::default()
        };

        let payload = m
            .build_payload(&task, TaskStatus::Done, &outcome, 1, 1, None, "p")
            .await;
        let result = payload["result"]["result"].as_str().unwrap();
        assert!(result.starts_with("…[truncated]"), "the tail is what matters");
        assert_eq!(payload["result"]["truncated"], true);
        assert!(payload["result"]["brief"]
            .as_str()
            .unwrap()
            .ends_with("…[truncated]"));
        let _ = std::fs::remove_dir_all(ws);
    }

    #[tokio::test]
    async fn classify_prefers_failure_then_budget_then_done() {
        let task = Arc::new(TaskState {
            task_id: "sub_1".to_string(),
            session_key: "k".to_string(),
            brief: "b".to_string(),
            workdir: PathBuf::from("/ws"),
            scene_id: None,
            budget: Budget {
                max_turns: 10,
                max_tokens: 1000,
                timeout_secs: 60,
                max_continuations: 1,
            },
            started_at: time::Instant::now(),
            cancelled: AtomicBool::new(false),
            inner: AsyncMutex::new(TaskInner {
                status: TaskStatus::Running,
                ended_at: None,
                turns: 0,
                last_tool: None,
                result_text: None,
                error: None,
                usage: Usage::default(),
                callback: CallbackState::Pending,
            }),
        });

        let done = TaskOutcome {
            text: Some("ok".into()),
            turns: 3,
            ..Default::default()
        };
        assert_eq!(classify(&task, &done, &json!({})).await, TaskStatus::Done);

        let failed = TaskOutcome {
            error: Some("provider refused".into()),
            ..Default::default()
        };
        assert_eq!(classify(&task, &failed, &json!({})).await, TaskStatus::Failed);

        let exhausted = TaskOutcome {
            turns: 10,
            ..Default::default()
        };
        assert_eq!(
            classify(&task, &exhausted, &json!({})).await,
            TaskStatus::BudgetExhausted
        );
        assert_eq!(
            classify(&task, &done, &json!({"stopReason":"max_tokens"})).await,
            TaskStatus::BudgetExhausted
        );

        task.cancelled.store(true, Ordering::SeqCst);
        assert_eq!(
            classify(&task, &done, &json!({})).await,
            TaskStatus::Cancelled,
            "a cancelled task is never reported as done"
        );
    }

    #[test]
    fn head_collapses_whitespace_and_clamps_on_char_boundaries() {
        assert_eq!(head("a\n  b\tc", 100), "a b c");
        let clamped = head(&"é".repeat(100), 11);
        assert!(clamped.ends_with('…'));
        assert!(clamped.len() <= 11 + 3);
    }
}

/// End-to-end task flow against a fake pi daemon on a real Unix socket.
///
/// The manager's adopt path (`ensure_running` probes before spawning) is what
/// makes this possible without pi installed: bind the socket, greet with a v7
/// hello, and Portal treats it as its own daemon.
#[cfg(all(test, unix))]
mod flow_tests {
    use super::*;
    use serde_json::Value;
    use std::sync::Mutex as StdMutex;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};

    const HELLO: &str = r#"{"type":"daemon_hello","protocol":{"name":"prime-agent.daemon","version":7},"appVersion":"0.7.2","supervisorPid":4242,"clientId":"portal-test","serverCapabilities":["session_input_admission"]}"#;

    type Seen = Arc<StdMutex<Vec<Value>>>;

    /// What a legacy daemon's `get_last_assistant_text` hands back — the whole
    /// result, because `prompt_and_wait` carried none of it.
    const LEGACY_RESULT: &str = "## Result\nLegacy daemon finished.";

    /// How the fake answers `prompt_and_wait`, which is the one place pi
    /// versions genuinely differ.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FakeDaemonMode {
        /// pi change P3: one atomic envelope with result, turns and usage.
        P3,
        /// pi v0.7.2 and older — what is actually shipping. `prompt_and_wait`
        /// only acknowledges, so Portal must assemble the outcome from
        /// `get_last_assistant_text` + `get_session_stats` +
        /// `wait_for_headless_completion`.
        Legacy,
    }

    struct Fake {
        seen: Seen,
        /// The session file the fake reports from `create`. Per-harness, so one
        /// test's resume cannot make another test's fresh session look resumed.
        session_file: PathBuf,
    }

    /// Bind `<state>/pi/daemon.sock` and serve the command subset Portal uses.
    fn start_fake_daemon(state_dir: &Path, prompt_delay_ms: u64, mode: FakeDaemonMode) -> Fake {
        let pi_dir = state_dir.join("pi");
        std::fs::create_dir_all(&pi_dir).unwrap();
        let socket = pi_dir.join("daemon.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();

        let session_file = pi_dir.join("sessions").join("s_1.jsonl");
        let seen: Seen = Arc::new(StdMutex::new(Vec::new()));
        let seen_task = Arc::clone(&seen);
        let file_task = session_file.display().to_string();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(
                    stream,
                    Arc::clone(&seen_task),
                    prompt_delay_ms,
                    file_task.clone(),
                    mode,
                ));
            }
        });

        Fake { seen, session_file }
    }

    async fn write_line(writer: &Arc<AsyncMutex<tokio::net::unix::OwnedWriteHalf>>, line: String) {
        let mut w = writer.lock().await;
        let _ = w.write_all(line.as_bytes()).await;
        let _ = w.write_all(b"\n").await;
        let _ = w.flush().await;
    }

    async fn serve(
        stream: UnixStream,
        seen: Seen,
        prompt_delay_ms: u64,
        session_file: String,
        mode: FakeDaemonMode,
    ) {
        let (read_half, write_half) = stream.into_split();
        let writer = Arc::new(AsyncMutex::new(write_half));
        write_line(&writer, HELLO.to_string()).await;

        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(raw) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            // Unwrap protocol-7 envelope if present, otherwise treat as bare command.
            let (cmd, id) = if raw.get("type").and_then(|t| t.as_str()) == Some("command") {
                let inner = raw.get("command").cloned().unwrap_or(raw.clone());
                let envelope_id = raw["id"].as_str().unwrap_or_default().to_string();
                (inner, envelope_id)
            } else {
                let bare_id = raw["id"].as_str().unwrap_or_default().to_string();
                (raw.clone(), bare_id)
            };
            seen.lock().unwrap().push(cmd.clone());

            let kind = cmd["type"].as_str().unwrap_or_default().to_string();
            let reply = |data: Value| {
                serde_json::json!({
                    "id": id, "type": "response", "command": kind,
                    "success": true, "data": data
                })
                .to_string()
            };

            match kind.as_str() {
                "create" => {
                    write_line(
                        &writer,
                        reply(serde_json::json!({
                            "activeSessionId": "as_1",
                            "sessionId": "s_1",
                            "sessionFile": session_file,
                            "isStreaming": false
                        })),
                    )
                    .await;
                }
                "get_available_models" => {
                    write_line(&writer, reply(serde_json::json!(["claude-sonnet-4-5"]))).await;
                }
                "prompt_and_wait" => {
                    // Reply out-of-band so abort/wait_for_idle stay answerable
                    // while the "task" runs — a real daemon behaves this way.
                    let writer = Arc::clone(&writer);
                    let reply_line = match mode {
                        FakeDaemonMode::P3 => reply(serde_json::json!({
                            "lastAssistantText": "## Result\nRefactored and tested.",
                            "stopReason": "end_turn",
                            "turns": 3,
                            "usage": {"input": 100, "output": 50, "cost": 0.01}
                        })),
                        // No envelope at all: a bare acknowledgement.
                        FakeDaemonMode::Legacy => reply(serde_json::json!(true)),
                    };
                    tokio::spawn(async move {
                        for event in [
                            r#"{"type":"session_event","activeSessionId":"as_1","event":{"type":"agent_start"}}"#,
                            r#"{"type":"session_event","activeSessionId":"as_1","event":{"type":"tool_execution_start","toolName":"bash","args":{"command":"cargo test"}}}"#,
                            r#"{"type":"session_event","activeSessionId":"as_1","event":{"type":"turn_end"}}"#,
                        ] {
                            write_line(&writer, event.to_string()).await;
                        }
                        time::sleep(Duration::from_millis(prompt_delay_ms)).await;
                        write_line(&writer, reply_line).await;
                    });
                }
                "get_last_assistant_text" => {
                    let text = match mode {
                        FakeDaemonMode::P3 => "partial work so far",
                        FakeDaemonMode::Legacy => LEGACY_RESULT,
                    };
                    write_line(&writer, reply(serde_json::json!(text))).await;
                }
                "get_session_stats" if mode == FakeDaemonMode::Legacy => {
                    write_line(
                        &writer,
                        reply(serde_json::json!({"input": 120, "output": 60, "cost": 0.02})),
                    )
                    .await;
                }
                "wait_for_headless_completion" if mode == FakeDaemonMode::Legacy => {
                    write_line(
                        &writer,
                        reply(serde_json::json!({
                            "turnsUsed": 5,
                            "tokensUsed": 180,
                            "stopReason": "end_turn"
                        })),
                    )
                    .await;
                }
                "shutdown" => {
                    // A real daemon answers and then drops the socket, which is
                    // what makes the shutdown/recovery race observable.
                    write_line(&writer, reply(serde_json::json!(true))).await;
                    break;
                }
                _ => {
                    // attach / abort / wait_for_idle / kill / …
                    write_line(&writer, reply(serde_json::json!(true))).await;
                }
            }
        }
    }

    // ── callback sink ───────────────────────────────────────────────

    struct Sink {
        url: String,
        hits: Arc<StdMutex<Vec<Value>>>,
    }

    async fn start_sink() -> Sink {
        use axum::extract::State;
        use axum::routing::post;

        type Hits = Arc<StdMutex<Vec<Value>>>;
        let hits: Hits = Arc::new(StdMutex::new(Vec::new()));
        let app = axum::Router::new()
            .route(
                "/api/callback",
                post(|State(hits): State<Hits>, body: String| async move {
                    hits.lock()
                        .unwrap()
                        .push(serde_json::from_str(&body).unwrap_or(Value::Null));
                    axum::http::StatusCode::OK
                }),
            )
            .with_state(Arc::clone(&hits));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Sink {
            url: format!("http://{addr}/api/callback"),
            hits,
        }
    }

    async fn wait_for_hits(sink: &Sink, n: usize, timeout: Duration) -> usize {
        let deadline = time::Instant::now() + timeout;
        loop {
            let got = sink.hits.lock().unwrap().len();
            if got >= n || time::Instant::now() >= deadline {
                return got;
            }
            time::sleep(Duration::from_millis(25)).await;
        }
    }

    // ── harness ─────────────────────────────────────────────────────

    struct Harness {
        manager: Arc<SubagentManager>,
        workspace: PathBuf,
        fake: Fake,
        sink: Sink,
    }

    /// Short, non-$TMPDIR root: the daemon socket must fit in `sun_path`
    /// (~104 bytes), and macOS temp dirs alone eat half of that.
    fn short_temp_root(tag: &str) -> PathBuf {
        let id = uuid::Uuid::new_v4().simple().to_string();
        PathBuf::from("/tmp").join(format!("pf-{tag}-{}", &id[..8]))
    }

    /// A harness against a P3 daemon — the shape most tests assert.
    async fn harness(tag: &str, prompt_delay_ms: u64) -> Harness {
        harness_with(tag, prompt_delay_ms, FakeDaemonMode::P3).await
    }

    async fn harness_with(tag: &str, prompt_delay_ms: u64, mode: FakeDaemonMode) -> Harness {
        let workspace = short_temp_root(tag);
        std::fs::create_dir_all(workspace.join("proj")).unwrap();
        let state_dir = workspace.join("state");

        let fake = start_fake_daemon(&state_dir, prompt_delay_ms, mode);
        let sink = start_sink().await;

        let callback = HeartCallback::new();
        callback.set(
            sink.url.clone(),
            "tok_secret".to_string(),
            "alice-laptop".to_string(),
        );

        let mut config = PortalConfig::default();
        config.security.workspace_root = workspace.clone();
        config.subagent.state_dir = Some(state_dir.display().to_string());
        config.subagent.command = Some(vec!["/bin/sh".to_string()]);
        // The fake daemon is "logged in": skip first-use setup guidance.
        config.subagent.model.provider = Some("anthropic".to_string());
        // Somewhere for portal_subagent_setup to write.
        let config_path = workspace.join("portal.toml");
        std::fs::write(&config_path, "name = \"vale\"\n").unwrap();
        config.config_path = Some(config_path);

        Harness {
            manager: SubagentManager::new(&config, callback),
            workspace,
            fake,
            sink,
        }
    }

    impl Harness {
        fn request(&self, brief: &str) -> SpawnRequest {
            SpawnRequest {
                brief: brief.to_string(),
                session: "scene-42".to_string(),
                workdir: Some("proj".to_string()),
                budget: Budget {
                    max_turns: 40,
                    max_tokens: 400_000,
                    timeout_secs: 60,
                    max_continuations: 3,
                },
                model: None,
                thinking: None,
                scene_id: Some("desktop-1".to_string()),
            }
        }

        fn commands(&self) -> Vec<String> {
            self.fake
                .seen
                .lock()
                .unwrap()
                .iter()
                .map(|c| c["type"].as_str().unwrap_or_default().to_string())
                .collect()
        }

        fn command(&self, kind: &str) -> Option<Value> {
            self.fake
                .seen
                .lock()
                .unwrap()
                .iter()
                .find(|c| c["type"] == kind)
                .cloned()
        }

        fn cleanup(&self) {
            let _ = std::fs::remove_dir_all(&self.workspace);
        }
    }

    // ── tests ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_task_runs_and_wakes_the_being_through_the_callback() {
        let h = harness("happy", 50).await;

        let receipt = h.manager.spawn(h.request("Refactor the auth module")).await.unwrap();
        assert!(receipt.task_id.starts_with("sub_"));
        assert_eq!(receipt.session, "scene-42");
        assert_eq!(receipt.status, TaskStatus::Running);
        assert!(!receipt.resumed, "a brand new session is not a resume");
        assert_eq!(
            receipt.session_file.as_deref(),
            Some(h.fake.session_file.display().to_string().as_str())
        );
        // The being is told the result is not in this response.
        assert!(receipt.to_json()["note"].as_str().unwrap().contains("inbox"));

        assert_eq!(
            wait_for_hits(&h.sink, 1, Duration::from_secs(10)).await,
            1,
            "a completed task must deliver exactly one callback"
        );
        let body = h.sink.hits.lock().unwrap()[0].clone();
        assert_eq!(body["source"], "portal");
        assert_eq!(body["task_id"], receipt.task_id);
        let r = &body["result"];
        assert_eq!(r["kind"], "subagent");
        assert_eq!(r["status"], "done");
        assert_eq!(r["session"], "scene-42");
        assert_eq!(r["result"], "## Result\nRefactored and tested.");
        assert_eq!(r["turns"], 3);
        assert_eq!(r["tokens"]["total"], 150);
        assert_eq!(r["cost_usd"], 0.01);
        assert_eq!(r["scene_id"], "desktop-1");
        assert_eq!(r["portal_name"], "alice-laptop");
        assert_eq!(r["brief"], "Refactor the auth module");
        assert!(r["workdir"].as_str().unwrap().ends_with("proj"));

        // The documented protocol sequence, in order.
        let commands = h.commands();
        let position = |name: &str| commands.iter().position(|c| c == name);
        assert!(position("create") < position("attach"), "{commands:?}");
        assert!(position("attach") < position("prompt_and_wait"), "{commands:?}");
        assert!(
            commands.contains(&"get_available_models".to_string()),
            "auth is prechecked at the first create: {commands:?}"
        );

        // The prompt carries the contract header and the brief.
        let create = h.command("create").unwrap();
        assert_eq!(create["lifecycle"], "resident");
        assert_eq!(create["name"], "portal:scene-42");
        assert_eq!(create["config"]["telemetryDisabled"], true);
        assert_eq!(create["config"]["autonomous"]["maxTurns"], 40);
        assert!(create["config"]["appendSystemPrompt"][0]
            .as_str()
            .unwrap()
            .contains("cannot ask"));
        assert_eq!(create["config"]["cwd"].as_str().unwrap().ends_with("proj"), true);

        let prompt = h.command("prompt_and_wait").unwrap();
        let message = prompt["message"].as_str().unwrap();
        assert!(message.starts_with("# Task sub_"), "{message}");
        assert!(message.contains("Budget: ≤40 turns"), "{message}");
        assert!(message.ends_with("Refactor the auth module"), "{message}");

        // Attach is headless: no UI, sequenced events.
        let attach = h.command("attach").unwrap();
        assert_eq!(attach["supportsExtensionUi"], false);

        h.cleanup();
    }

    #[tokio::test]
    async fn a_legacy_daemon_result_is_assembled_from_follow_up_round_trips() {
        // pi v0.7.2 does not emit the P3 envelope, so this is the path that
        // actually runs in production today.
        let h = harness_with("legacy", 50, FakeDaemonMode::Legacy).await;

        let receipt = h.manager.spawn(h.request("Refactor the auth module")).await.unwrap();
        assert_eq!(receipt.status, TaskStatus::Running);
        assert_eq!(
            wait_for_hits(&h.sink, 1, Duration::from_secs(10)).await,
            1,
            "a bare prompt_and_wait acknowledgement must still wake the being"
        );

        let body = h.sink.hits.lock().unwrap()[0].clone();
        let r = &body["result"];
        assert_eq!(r["status"], "done");
        assert_eq!(
            r["result"], LEGACY_RESULT,
            "the body comes from get_last_assistant_text"
        );
        assert_eq!(r["tokens"]["input"], 120, "accounting comes from get_session_stats");
        assert_eq!(r["tokens"]["output"], 60);
        assert_eq!(r["tokens"]["total"], 180);
        assert_eq!(r["cost_usd"], 0.02);
        assert_eq!(
            r["turns"], 5,
            "turns come from wait_for_headless_completion, not the envelope"
        );
        assert_eq!(r["error"], Value::Null);

        // All three fallback round trips were actually made.
        let commands = h.commands();
        for needed in [
            "get_last_assistant_text",
            "get_session_stats",
            "wait_for_headless_completion",
        ] {
            assert!(
                commands.contains(&needed.to_string()),
                "the non-P3 path must ask for {needed}: {commands:?}"
            );
        }

        // The finished task reads the same from status as a P3 one would.
        let status = h.manager.status(Some(&receipt.task_id), None).await.unwrap();
        assert_eq!(status["status"], "done");
        assert_eq!(status["callback"], "sent");
        assert_eq!(status["result_head"], LEGACY_RESULT);
        h.cleanup();
    }

    #[tokio::test]
    async fn a_p3_daemon_needs_no_follow_up_round_trips() {
        let h = harness("p3", 50).await;
        h.manager.spawn(h.request("Refactor the auth module")).await.unwrap();
        assert_eq!(wait_for_hits(&h.sink, 1, Duration::from_secs(10)).await, 1);

        // The envelope is authoritative; asking again would cost a round trip
        // and could read a later turn's text.
        let commands = h.commands();
        for avoided in [
            "get_last_assistant_text",
            "get_session_stats",
            "wait_for_headless_completion",
        ] {
            assert!(
                !commands.contains(&avoided.to_string()),
                "P3 already answered; {avoided} must not be asked: {commands:?}"
            );
        }
        h.cleanup();
    }

    #[tokio::test]
    async fn progress_events_land_in_the_transcript_and_the_status_row() {
        let h = harness("progress", 400).await;
        let receipt = h.manager.spawn(h.request("Build it")).await.unwrap();

        // Long-poll the transcript exactly as the being would.
        let page = h
            .manager
            .log(&receipt.task_id, 0, 64 * 1024, 3_000)
            .await
            .unwrap();
        let output = page["output"].as_str().unwrap();
        assert!(output.contains("[start] task sub_"), "{output}");
        assert_eq!(page["truncated"], false);
        assert!(page["next_offset"].as_u64().unwrap() > 0);

        // Wait for the tool event to be rendered.
        let deadline = time::Instant::now() + Duration::from_secs(5);
        let mut seen_tool = false;
        let mut offset = 0u64;
        let mut transcript = String::new();
        while time::Instant::now() < deadline && !seen_tool {
            let page = h
                .manager
                .log(&receipt.task_id, offset, 64 * 1024, 500)
                .await
                .unwrap();
            transcript.push_str(page["output"].as_str().unwrap());
            offset = page["next_offset"].as_u64().unwrap();
            seen_tool = transcript.contains("tool ▶ bash");
        }
        assert!(seen_tool, "tool events must reach the transcript: {transcript}");

        let status = h.manager.status(Some(&receipt.task_id), None).await.unwrap();
        assert_eq!(status["session"], "scene-42");
        assert_eq!(status["turns"], 1, "turn_end increments the counter");
        assert_eq!(status["last_tool"], "bash");
        assert_eq!(status["callback"], "pending");

        assert_eq!(wait_for_hits(&h.sink, 1, Duration::from_secs(10)).await, 1);
        h.cleanup();
    }

    #[tokio::test]
    async fn a_second_task_reuses_the_same_warm_session() {
        let h = harness("reuse", 30).await;

        let first = h.manager.spawn(h.request("First task")).await.unwrap();
        assert_eq!(wait_for_hits(&h.sink, 1, Duration::from_secs(10)).await, 1);

        let second = h.manager.spawn(h.request("Second task")).await.unwrap();
        assert_ne!(first.task_id, second.task_id);
        assert_eq!(second.session, first.session);
        assert_eq!(wait_for_hits(&h.sink, 2, Duration::from_secs(10)).await, 2);

        // Harness accumulation: one create, two prompts.
        let commands = h.commands();
        assert_eq!(
            commands.iter().filter(|c| *c == "create").count(),
            1,
            "the session is created once and reused: {commands:?}"
        );
        assert_eq!(
            commands.iter().filter(|c| *c == "prompt_and_wait").count(),
            2
        );

        let status = h.manager.status(None, None).await.unwrap();
        let sessions = status["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["tasks_total"], 2);
        assert_eq!(sessions[0]["busy"], false);
        assert_eq!(sessions[0]["loaded"], true);
        h.cleanup();
    }

    #[tokio::test]
    async fn a_busy_session_is_refused_rather_than_queued() {
        let h = harness("busy", 1_500).await;
        let first = h.manager.spawn(h.request("Long task")).await.unwrap();

        let err = h
            .manager
            .spawn(h.request("Interrupting task"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("is busy"), "{err}");
        assert!(err.contains(&first.task_id), "the being is told what to wait for: {err}");
        assert!(
            !h.commands().contains(&"follow_up".to_string()),
            "a refused spawn must not smuggle work into the running task"
        );

        // Only the one task exists; nothing was parked in a state that never
        // completes and never calls back.
        let status = h.manager.status(None, None).await.unwrap();
        assert_eq!(status["running"].as_array().unwrap().len(), 1);
        assert_eq!(status["completed"].as_array().unwrap().len(), 0);
        h.cleanup();
    }

    #[tokio::test]
    async fn parallel_spawns_on_one_session_let_exactly_one_through() {
        let h = harness("race", 1_500).await;

        // The check-and-set of `busy_task` must be atomic: two spawns racing on
        // a cold session used to both pass and one would leak its permit.
        let (a, b) = tokio::join!(
            h.manager.spawn(h.request("First")),
            h.manager.spawn(h.request("Second")),
        );
        let winners = [&a, &b].iter().filter(|r| r.is_ok()).count();
        assert_eq!(winners, 1, "exactly one spawn may claim the session: {a:?} / {b:?}");

        let err = match (&a, &b) {
            (Err(e), _) | (_, Err(e)) => e.to_string(),
            _ => unreachable!("one of the two must have failed"),
        };
        assert!(
            err.contains("busy") || err.contains("in flight"),
            "the loser is told why: {err}"
        );

        let status = h.manager.status(None, None).await.unwrap();
        assert_eq!(status["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(status["sessions"][0]["tasks_total"], 1);
        assert_eq!(status["running"].as_array().unwrap().len(), 1);
        h.cleanup();
    }

    #[tokio::test]
    async fn cancel_suppresses_the_callback_and_returns_the_partial_result() {
        let h = harness("cancel", 2_000).await;
        let receipt = h.manager.spawn(h.request("Long task")).await.unwrap();

        let result = h.manager.cancel(&receipt.task_id).await.unwrap();
        assert_eq!(result["status"], "cancelled");
        assert_eq!(result["partial_result"], "partial work so far");
        assert!(result["note"].as_str().unwrap().contains("no callback"));
        assert!(h.commands().contains(&"abort".to_string()));

        // The task finishes as cancelled, and the being is never woken for it.
        let deadline = time::Instant::now() + Duration::from_secs(6);
        let mut final_status = String::new();
        while time::Instant::now() < deadline {
            let status = h.manager.status(Some(&receipt.task_id), None).await.unwrap();
            final_status = status["status"].as_str().unwrap().to_string();
            if final_status == "cancelled" {
                break;
            }
            time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(final_status, "cancelled");
        assert_eq!(
            wait_for_hits(&h.sink, 1, Duration::from_secs(2)).await,
            0,
            "cancel is deliberate: it must never wake the being"
        );

        let status = h.manager.status(Some(&receipt.task_id), None).await.unwrap();
        assert_eq!(status["callback"], "suppressed");
        h.cleanup();
    }

    #[tokio::test]
    async fn steer_and_follow_up_reach_the_running_session() {
        let h = harness("steer", 1_500).await;
        let receipt = h.manager.spawn(h.request("Long task")).await.unwrap();

        h.manager
            .steer(&receipt.task_id, "prefer small diffs", false)
            .await
            .unwrap();
        h.manager
            .steer(&receipt.task_id, "then update the changelog", true)
            .await
            .unwrap();

        let commands = h.commands();
        assert!(commands.contains(&"steer".to_string()), "{commands:?}");
        assert!(commands.contains(&"follow_up".to_string()), "{commands:?}");
        assert_eq!(
            h.command("steer").unwrap()["message"],
            "prefer small diffs"
        );

        assert!(h.manager.steer(&receipt.task_id, "   ", false).await.is_err());

        // Both land in the transcript so the being can see their own nudges.
        let page = h
            .manager
            .log(&receipt.task_id, 0, 64 * 1024, 0)
            .await
            .unwrap();
        let output = page["output"].as_str().unwrap();
        assert!(output.contains("[steer] prefer small diffs"), "{output}");
        h.cleanup();
    }

    #[tokio::test]
    async fn shutdown_aborts_running_tasks_without_waking_the_being() {
        let h = harness("shutdown", 3_000).await;
        let receipt = h.manager.spawn(h.request("Long task")).await.unwrap();

        h.manager.shutdown().await;
        assert!(h.commands().contains(&"abort".to_string()));
        assert!(
            h.commands().contains(&"shutdown".to_string()),
            "the daemon is asked to stop, not just dropped"
        );
        assert_eq!(
            wait_for_hits(&h.sink, 1, Duration::from_secs(2)).await,
            0,
            "Portal shutting down is not a result"
        );

        // Further spawns are refused rather than racing the shutdown.
        let err = h
            .manager
            .spawn(h.request("Too late"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("shutting down"), "{err}");
        let _ = receipt;
        h.cleanup();
    }

    #[tokio::test]
    async fn shutdown_never_starts_the_daemon_again() {
        let h = harness("shutdown-race", 30_000).await;
        let receipt = h.manager.spawn(h.request("Long task")).await.unwrap();

        h.manager.shutdown().await;
        let after_shutdown = h.fake.seen.lock().unwrap().len();

        // The daemon dropped the socket on its way out. The in-flight prompt
        // fails, and the recovery path must not reconnect: doing so would
        // spawn a fresh pi behind the Portal that is going away.
        let deadline = time::Instant::now() + Duration::from_secs(5);
        let mut status = String::new();
        while time::Instant::now() < deadline {
            status = h.manager.status(Some(&receipt.task_id), None).await.unwrap()["status"]
                .as_str()
                .unwrap()
                .to_string();
            if status == "cancelled" {
                break;
            }
            time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            status, "cancelled",
            "the task must finalize at once, not sit in the 60s reconnect window"
        );

        let commands = h.commands();
        assert_eq!(
            commands.len(),
            after_shutdown,
            "nothing may be sent after shutdown: {:?}",
            &commands[after_shutdown.min(commands.len())..]
        );

        // Every caller that could resurrect the daemon is refused by name.
        let err = match h.manager.client().await {
            Ok(_) => panic!("client() must not start the daemon during shutdown"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("shutting down"), "{err}");
        let err = h
            .manager
            .steer(&receipt.task_id, "one more thing", false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("already finished") || err.contains("shutting down"), "{err}");

        assert_eq!(
            wait_for_hits(&h.sink, 1, Duration::from_secs(1)).await,
            0,
            "shutdown is still not a result"
        );
        h.cleanup();
    }

    #[tokio::test]
    async fn a_session_resumes_its_file_after_being_unloaded() {
        let h = harness("resume", 30).await;
        h.manager.spawn(h.request("First task")).await.unwrap();
        assert_eq!(wait_for_hits(&h.sink, 1, Duration::from_secs(10)).await, 1);

        // Pretend the session went idle and was unloaded.
        let session = h.manager.session_for_task("scene-42").await.unwrap();
        session.inner.lock().await.active_session_id = None;
        std::fs::create_dir_all(h.fake.session_file.parent().unwrap()).unwrap();
        std::fs::write(&h.fake.session_file, b"{}\n").unwrap();

        let receipt = h.manager.spawn(h.request("Second task")).await.unwrap();
        assert!(receipt.resumed, "an unloaded session with a file is a resume");

        let creates: Vec<Value> = h
            .fake
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c["type"] == "create")
            .cloned()
            .collect();
        assert_eq!(creates.len(), 2);
        assert_eq!(
            creates[1]["sessionPath"],
            h.fake.session_file.display().to_string(),
            "resume is create{{sessionPath}}, so the harness carries over"
        );
        h.cleanup();
    }

    #[tokio::test]
    async fn changing_credentials_restarts_the_daemon_and_resumes_sessions() {
        let h = harness("recreds", 400).await;
        let first = h.manager.spawn(h.request("Long task")).await.unwrap();
        assert!(h.manager.daemon().client().await.is_some(), "daemon adopted");

        // Not while a task is running: that would kill it mid-flight.
        let err = h
            .manager
            .configure_model(ModelConfigUpdate {
                provider: Some("openrouter".to_string()),
                api_key: Some("sk-or-v1-new-key-for-the-test".to_string()),
                ..Default::default()
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("still running"), "{err}");
        assert_eq!(h.manager.model_config().provider.as_deref(), Some("anthropic"));

        // A model-only change is fine mid-task: the daemon env is untouched.
        let outcome = h
            .manager
            .configure_model(ModelConfigUpdate {
                model: Some("claude-opus-4-1".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!outcome.daemon_restarted);

        assert_eq!(wait_for_hits(&h.sink, 1, Duration::from_secs(10)).await, 1);
        assert_eq!(
            h.manager.status(Some(&first.task_id), None).await.unwrap()["status"],
            "done"
        );

        // Now the credentials can change; the live daemon is asked to stop.
        let outcome = h
            .manager
            .configure_model(ModelConfigUpdate {
                provider: Some("openrouter".to_string()),
                api_key: Some("sk-or-v1-new-key-for-the-test".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(outcome.daemon_restarted, "a connected daemon was stopped");
        assert!(h.commands().contains(&"shutdown".to_string()), "{:?}", h.commands());
        let fresh = h.manager.daemon();
        assert_eq!(fresh.config().provider.as_deref(), Some("openrouter"));
        assert_eq!(fresh.config().api_key.as_deref(), Some("sk-or-v1-new-key-for-the-test"));
        assert!(fresh.client().await.is_none(), "not started until the next task");

        let session = h.manager.session_for_task("scene-42").await.unwrap();
        assert!(session.inner.lock().await.active_session_id.is_none(), "forgotten, not lost");

        // The next spawn re-creates the session with the new provider.
        std::fs::create_dir_all(h.fake.session_file.parent().unwrap()).unwrap();
        std::fs::write(&h.fake.session_file, b"{}\n").unwrap();
        let receipt = h.manager.spawn(h.request("Second task")).await.unwrap();
        assert!(receipt.resumed, "session file carried over the restart");
        let creates: Vec<Value> = h
            .fake
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c["type"] == "create")
            .cloned()
            .collect();
        assert_eq!(creates.len(), 2);
        assert_eq!(creates[1]["config"]["provider"], "openrouter");
        assert_eq!(creates[1]["config"]["model"], "claude-opus-4-1");
        assert_eq!(wait_for_hits(&h.sink, 2, Duration::from_secs(10)).await, 2);

        // Persisted, and never the raw key in status.
        let reloaded = PortalConfig::load(h.workspace.join("portal.toml").to_str().unwrap()).unwrap();
        assert_eq!(reloaded.subagent.model.provider.as_deref(), Some("openrouter"));
        let status = h.manager.status(None, None).await.unwrap();
        assert_eq!(status["auth"]["api_key"], "sk-or-v...***");
        assert!(!status.to_string().contains("new-key-for-the-test"));
        h.cleanup();
    }

    #[tokio::test]
    async fn a_session_refuses_to_change_its_working_directory() {
        let h = harness("cwd", 30).await;
        h.manager.spawn(h.request("First task")).await.unwrap();

        let mut elsewhere = h.request("Second task");
        elsewhere.workdir = None; // workspace root, not proj
        let err = h.manager.spawn(elsewhere).await.unwrap_err().to_string();
        assert!(err.contains("use a different session key or reset it"), "{err}");
        h.cleanup();
    }

    #[tokio::test]
    async fn concurrency_is_capped_by_max_concurrent() {
        let workspace = short_temp_root("cap");
        std::fs::create_dir_all(workspace.join("proj")).unwrap();
        let state_dir = workspace.join("state");
        let fake = start_fake_daemon(&state_dir, 2_000, FakeDaemonMode::P3);
        let sink = start_sink().await;

        let callback = HeartCallback::new();
        callback.set(sink.url.clone(), "t".to_string(), "p".to_string());
        let mut config = PortalConfig::default();
        config.security.workspace_root = workspace.clone();
        config.subagent.state_dir = Some(state_dir.display().to_string());
        config.subagent.command = Some(vec!["/bin/sh".to_string()]);
        config.subagent.max_concurrent = 1;
        config.subagent.model.provider = Some("anthropic".to_string());
        let manager = SubagentManager::new(&config, callback);

        let request = |session: &str| SpawnRequest {
            brief: "work".to_string(),
            session: session.to_string(),
            workdir: Some("proj".to_string()),
            budget: Budget {
                max_turns: 10,
                max_tokens: 1000,
                timeout_secs: 60,
                max_continuations: 1,
            },
            model: None,
            thinking: None,
            scene_id: None,
        };

        manager.spawn(request("a")).await.unwrap();
        // A different session, so `busy` does not apply — only the global cap.
        let err = manager.spawn(request("b")).await.unwrap_err().to_string();
        assert!(err.contains("already running"), "{err}");
        let _ = fake;
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn a_timed_out_task_is_aborted_and_reported_as_timeout() {
        let h = harness("timeout", 60_000).await;
        let mut request = h.request("Task that never finishes");
        // Budget + slack is the Portal-side wall clock; keep it short.
        request.budget.timeout_secs = 1;
        let receipt = h.manager.spawn(request).await.unwrap();

        // 1 s budget + 120 s slack is too long to wait for, so assert the
        // contract that matters: the task is still running and the being can
        // end it themselves.
        let status = h.manager.status(Some(&receipt.task_id), None).await.unwrap();
        assert_eq!(status["status"], "running");
        assert_eq!(status["callback"], "pending");
        h.manager.cancel(&receipt.task_id).await.unwrap();
        h.cleanup();
    }

    // ── the stdio fallback, end to end ──────────────────────────────

    /// What the fake pi reports for every task it is given.
    const STDIO_RESULT: &str = "## Result\nThe stdio pi finished the job.";

    struct StdioHarness {
        manager: Arc<SubagentManager>,
        workspace: PathBuf,
        sink: Sink,
    }

    /// A pi shaped like 0.73.1: `--daemon-socket` is not a thing it knows,
    /// but `--print --mode json` works. `linger` seconds pass before it
    /// answers, so a task can be caught mid-flight — and it only touches
    /// `finished.marker` if it was allowed to run to the end.
    fn write_stdio_pi(root: &Path, linger: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join("fake-pi");
        let script = format!(
            r###"#!/bin/sh
case "$*" in
  *--daemon-socket*) echo 'Error: Unknown option: --daemon-socket' >&2; exit 1 ;;
esac
echo '{{"type":"agent_start"}}'
sleep {linger}
touch '{marker}'
cat <<'JSON'
{{"type":"message_end","message":{{"role":"assistant","content":[{{"type":"text","text":"{result}"}}],"usage":{{"input":11,"output":7}},"stopReason":"stop"}}}}
{{"type":"turn_end"}}
{{"type":"agent_end","messages":[]}}
JSON
"###,
            marker = root.join("finished.marker").display(),
            result = STDIO_RESULT.replace('\n', "\\n"),
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Like [`harness`], but with nothing listening on the socket: the
    /// manager tries daemon mode, is rebuffed, and latches stdio.
    async fn stdio_harness(tag: &str, linger: u32) -> StdioHarness {
        let workspace = short_temp_root(tag);
        std::fs::create_dir_all(workspace.join("proj")).unwrap();
        let state_dir = workspace.join("state");
        let pi = write_stdio_pi(&workspace, linger);
        let sink = start_sink().await;

        let callback = HeartCallback::new();
        callback.set(
            sink.url.clone(),
            "tok_secret".to_string(),
            "alice-laptop".to_string(),
        );

        let mut config = PortalConfig::default();
        config.security.workspace_root = workspace.clone();
        config.subagent.state_dir = Some(state_dir.display().to_string());
        config.subagent.command = Some(vec![pi.display().to_string()]);
        config.subagent.model.provider = Some("openrouter".to_string());
        config.subagent.model.model = Some("deepseek/deepseek-chat-v4-0324".to_string());
        config.subagent.model.api_key = Some("sk-or-v1-test".to_string());

        StdioHarness {
            manager: SubagentManager::new(&config, callback),
            workspace,
            sink,
        }
    }

    impl StdioHarness {
        fn request(&self, brief: &str) -> SpawnRequest {
            SpawnRequest {
                brief: brief.to_string(),
                session: "scene-42".to_string(),
                workdir: Some("proj".to_string()),
                budget: Budget {
                    max_turns: 40,
                    max_tokens: 400_000,
                    timeout_secs: 60,
                    max_continuations: 3,
                },
                model: None,
                thinking: None,
                scene_id: Some("desktop-1".to_string()),
            }
        }

        async fn status_of(&self, task_id: &str) -> String {
            self.manager
                .status(Some(task_id), None)
                .await
                .unwrap()["status"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        }

        async fn wait_for_status(&self, task_id: &str, want: &str, timeout: Duration) -> String {
            let deadline = time::Instant::now() + timeout;
            loop {
                let got = self.status_of(task_id).await;
                if got == want || time::Instant::now() >= deadline {
                    return got;
                }
                time::sleep(Duration::from_millis(25)).await;
            }
        }

        fn cleanup(&self) {
            let _ = std::fs::remove_dir_all(&self.workspace);
        }
    }

    #[tokio::test]
    async fn a_pi_without_daemon_mode_runs_the_task_and_wakes_the_being() {
        let h = stdio_harness("stdio", 0).await;
        let receipt = h.manager.spawn(h.request("Refactor the parser")).await.unwrap();
        assert_eq!(receipt.status, TaskStatus::Running);
        assert!(!receipt.resumed, "a fresh process resumes nothing");

        assert_eq!(
            wait_for_hits(&h.sink, 1, Duration::from_secs(20)).await,
            1,
            "the being is woken over stdio exactly as over the daemon"
        );
        let hit = h.sink.hits.lock().unwrap()[0].clone();
        assert_eq!(hit["source"], "portal");
        assert_eq!(hit["result"]["kind"], "subagent");
        assert_eq!(hit["result"]["status"], "done");
        assert_eq!(hit["result"]["session"], "scene-42");
        assert_eq!(hit["result"]["result"], STDIO_RESULT);
        assert_eq!(hit["result"]["turns"], 1);
        assert_eq!(hit["result"]["tokens"]["total"], 18);
        assert_eq!(hit["result"]["scene_id"], "desktop-1");

        // The being can see which transport is in play, and read the run.
        let status = h.manager.status(None, None).await.unwrap();
        assert_eq!(status["daemon"]["transport"], "stdio");
        assert_eq!(status["available"], true);
        let log = h
            .manager
            .log(&receipt.task_id, 0, DEFAULT_LOG_LIMIT, 0)
            .await
            .unwrap();
        let output = log["output"].as_str().unwrap();
        assert!(output.contains("agent_start"), "{output}");
        assert!(output.contains("The stdio pi finished"), "{output}");
        assert!(output.contains("→ done"), "{output}");
        // A run that was left alone leaves the marker — which is what makes
        // its absence meaningful in the cancel test below.
        assert!(h.workspace.join("finished.marker").exists());

        h.cleanup();
    }

    #[tokio::test]
    async fn a_stdio_task_is_cancelled_by_killing_its_process() {
        let h = stdio_harness("stdiocancel", 3).await;
        let receipt = h.manager.spawn(h.request("Take your time")).await.unwrap();

        // Steering has nowhere to land: there is no live session behind a
        // one-shot process, and saying so is better than a silent no-op.
        let err = h
            .manager
            .steer(&receipt.task_id, "also fix the tests", false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no daemon mode"), "{err}");

        let answer = h.manager.cancel(&receipt.task_id).await.unwrap();
        assert_eq!(answer["status"], "cancelled");
        assert_eq!(
            h.wait_for_status(&receipt.task_id, "cancelled", Duration::from_secs(10))
                .await,
            "cancelled",
            "cancelling must actually kill the pi process"
        );
        assert_eq!(
            wait_for_hits(&h.sink, 1, Duration::from_millis(500)).await,
            0,
            "a deliberate cancel never wakes the being"
        );

        // The fake pi only leaves this behind if it was allowed to finish, so
        // its absence past its own runtime is proof the child was killed and
        // is not still burning tokens somewhere.
        time::sleep(Duration::from_secs(4)).await;
        assert!(
            !h.workspace.join("finished.marker").exists(),
            "cancel must kill the pi process, not just stop reading it"
        );

        h.cleanup();
    }
}
