//! Lifecycle of the Portal-owned pi daemon (PRD §4.4 / §4.11).
//!
//! Portal spawns `pi --mode daemon --daemon-socket <private>` as a *supervised
//! child* — not detached — pointed at a Portal-private agent dir and sessions
//! dir. `ensure_running()` probes first, so a daemon that outlived a Portal
//! restart is adopted rather than duplicated.
//!
//! ```text
//! ensure_transport()
//!   mode == Stdio? ───────────────────────────────────► per-task stdio
//!   connected client? ────────────────────────────────► reuse
//!   probe socket  ─ v7 hello ─────────────────────────► adopt (child = None)
//!                 ─ wrong version ─► shutdown, respawn
//!                 ─ nothing there ─► spawn + readiness loop (≤30 s)
//!                                    └ no daemon mode ─► latch Stdio
//! shutdown(grace)
//!   `shutdown{}` ─► wait for exit ─► SIGTERM ─► SIGKILL   (adopted: cmd only)
//! ```
//!
//! Not every pi has a daemon mode — 0.73.1 rejects `--daemon-socket` outright
//! and exits. Rather than making the whole sub-agent unavailable, the first
//! failed start latches [`TransportMode::Stdio`] and every task then runs as
//! its own `pi --print --mode json` process ([`PiDaemon::run_stdio_task`]).
//! The decision is cached for the life of the process: a pi that cannot
//! daemonise will not learn to during one Portal run, and retrying would cost
//! a doomed spawn on every single task.
//!
//! When no pi resolves at all, [`auto_provision_pi`] installs Portal's own
//! pinned copy under `~/.heart-portal/pi` with npm:
//!
//! ```text
//! ~/.heart-portal/pi/
//!   VERSION                    ← PI_PINNED_VERSION; a mismatch re-installs
//!   bin/pi                     → node_modules/.bin/pi   (what resolve_command finds)
//!   node_modules/.bin/pi       ← npm install --prefix ~/.heart-portal/pi <pkg>@<ver>
//!   .provision-attempt         ← unix time of the last failed automatic try
//! ```
//!
//! Every failure — no npm, timeout, npm error — is a [`Provision`] variant and
//! a log line, never a crash: Portal runs without the sub-agent.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStdin, Command as TokioCommand};
use tokio::sync::mpsc;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time;
use tracing::{debug, info, warn};

#[cfg(not(unix))]
use super::pi_client::ConnectPolicy;
use super::pi_client::{PiClient, DEFAULT_REQUEST_TIMEOUT};
use super::protocol::{Command, StdioResult};
use super::transcript;

/// Candidate binaries, in resolution order, when `[subagent].command` is unset.
const PI_BINARY_CANDIDATES: [&str; 2] = ["pi", "prime-agent"];
/// Where Portal keeps the pi it installs itself (`~`-relative).
const BUNDLED_PI_ROOT: &str = "~/.heart-portal/pi";
/// The npm package that ships the `pi` binary.
pub const PI_NPM_PACKAGE: &str = "@mariozechner/pi-coding-agent";
/// The exact version `auto_provision_pi` installs. Bumping this makes the
/// next startup re-install (`VERSION` no longer matches).
pub const PI_PINNED_VERSION: &str = "0.65.2";
/// Upper bound on one `npm install` run; past it npm is killed and the
/// attempt counts as failed.
pub const NPM_INSTALL_TIMEOUT: Duration = Duration::from_secs(120);
/// A failed *automatic* attempt is not retried more often than this, so a
/// machine without network does not pay 30 s on every Portal start. An
/// explicit request (`portal_subagent_setup`) ignores the backoff.
const PROVISION_RETRY_BACKOFF: Duration = Duration::from_secs(60 * 60);
/// Poll interval while waiting for npm.
const NPM_POLL: Duration = Duration::from_millis(200);
/// How long `ensure_running` waits for a freshly spawned daemon to greet.
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Interval of the readiness loop.
const READINESS_POLL: Duration = Duration::from_millis(100);
/// Grace given to `shutdown{}` before signalling.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// Grace between SIGTERM and SIGKILL.
const SIGKILL_GRACE: Duration = Duration::from_secs(2);
/// Time allowed for a wrong-version daemon to vacate the socket.
const REPLACE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a fresh child gets to *create* the socket file before Portal
/// decides this pi has no daemon mode at all. Well under [`STARTUP_TIMEOUT`],
/// which stays the budget for a daemon that exists but is still warming up.
#[cfg(unix)]
const STDIO_FALLBACK_PROBE: Duration = Duration::from_secs(5);
/// Grace between SIGTERM and SIGKILL for a timed-out stdio task.
const STDIO_KILL_GRACE: Duration = Duration::from_secs(2);
/// Stderr kept from a stdio task, to explain a failure without the log file.
const STDIO_STDERR_TAIL_BYTES: usize = 4096;
/// daemon.log is truncated once it passes this size.
const DAEMON_LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;
/// Tail of daemon.log quoted in startup failures.
const DAEMON_LOG_TAIL_BYTES: usize = 4096;
/// `sockaddr_un.sun_path` is ~104 bytes on macOS and 108 on Linux. Bind fails
/// with an opaque EINVAL past that, so check it and say what to do instead.
#[cfg(unix)]
const MAX_SOCKET_PATH_BYTES: usize = 100;

/// Everything `PiDaemon` needs that comes from configuration.
#[derive(Debug, Clone)]
pub struct PiDaemonConfig {
    /// Resolved argv for pi (`command[0]` exists or is on PATH).
    pub command: Vec<String>,
    /// `<state_dir>` — ledger, socket, agent dir and sessions all live under it.
    pub state_dir: PathBuf,
    /// cwd for the daemon process.
    pub workspace_root: PathBuf,
    /// Environment variables forwarded into the `env_clear()`ed child.
    pub env_passthrough: Vec<String>,
    /// API key from portal.toml — injected into daemon env so sessions can auth.
    pub api_key: Option<String>,
    /// Provider name (openrouter, anthropic, etc.) — determines which env var
    /// receives the api_key.
    pub provider: Option<String>,
}

/// How Portal talks to pi.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TransportMode {
    /// One long-lived `pi --mode daemon` child, one session per session key.
    #[default]
    Daemon,
    /// One `pi --print --mode json` process per task. The fallback for pi
    /// builds without a daemon mode; no session reuse, no steering.
    Stdio,
}

impl TransportMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TransportMode::Daemon => "daemon",
            TransportMode::Stdio => "stdio",
        }
    }
}

/// What [`PiDaemon::ensure_transport`] resolved to for this Portal run.
pub enum Transport {
    Daemon(Arc<PiClient>),
    Stdio,
}

impl Transport {
    #[allow(dead_code)] // read by tests and by future status surfaces
    pub fn mode(&self) -> TransportMode {
        match self {
            Transport::Daemon(_) => TransportMode::Daemon,
            Transport::Stdio => TransportMode::Stdio,
        }
    }
}

/// Snapshot for `portal_subagent_status` / `portal_status`. Paths and versions
/// only — never credentials (PRD §8 acceptance).
#[derive(Debug, Clone, Default)]
pub struct DaemonHealth {
    pub running: bool,
    pub pid: Option<u32>,
    pub protocol: Option<u32>,
    pub app_version: Option<String>,
    pub adopted: bool,
    pub socket: Option<String>,
    pub transport: TransportMode,
}

impl DaemonHealth {
    pub fn to_json(&self) -> Value {
        json!({
            "running": self.running,
            "pid": self.pid,
            "protocol": self.protocol,
            "app_version": self.app_version,
            "adopted": self.adopted,
            "socket": self.socket,
            "transport": self.transport.as_str(),
        })
    }
}

#[derive(Default)]
struct DaemonState {
    child: Option<Child>,
    /// Held open for pi change P1: stdin EOF is the daemon's exit signal, so
    /// a SIGKILLed Portal cannot leave a daemon behind.
    child_stdin: Option<ChildStdin>,
    client: Option<Arc<PiClient>>,
    adopted: bool,
    pid: Option<u32>,
    /// Latched to `Stdio` by a failed daemon start; never latched back.
    mode: TransportMode,
}

pub struct PiDaemon {
    config: PiDaemonConfig,
    state: AsyncMutex<DaemonState>,
}

impl PiDaemon {
    pub fn new(config: PiDaemonConfig) -> Self {
        Self {
            config,
            state: AsyncMutex::new(DaemonState::default()),
        }
    }

    /// Read back the resolved argv / paths (used by diagnostics).
    #[allow(dead_code)] // surfaced by portal_status once that tool exists
    pub fn config(&self) -> &PiDaemonConfig {
        &self.config
    }

    /// `<state_dir>/pi` — everything pi-specific.
    pub fn pi_dir(&self) -> PathBuf {
        self.config.state_dir.join("pi")
    }

    /// `PRIME_AGENT_CODING_AGENT_DIR`: auth.json, settings.json, harness, logs.
    pub fn agent_dir(&self) -> PathBuf {
        self.pi_dir().join("agent")
    }

    /// `PRIME_AGENT_SESSION_DIR`: the session JSONL files that *are* the
    /// sub-agent's accumulated harness (PRD §7.5).
    pub fn sessions_dir(&self) -> PathBuf {
        self.pi_dir().join("sessions")
    }

    pub fn log_path(&self) -> PathBuf {
        self.pi_dir().join("daemon.log")
    }

    /// Private transport endpoint. A private path means a leaked daemon can
    /// never collide with the user's own interactive pi (PRD §9 risk 2).
    pub fn socket_path(&self) -> PathBuf {
        self.pi_dir().join("daemon.sock")
    }

    /// Reject a `state_dir` so deep that the daemon socket cannot be bound.
    #[cfg(unix)]
    fn check_socket_path(&self) -> Result<()> {
        let socket = self.socket_path();
        let len = socket.as_os_str().len();
        if len > MAX_SOCKET_PATH_BYTES {
            anyhow::bail!(
                "the daemon socket path is {len} bytes ({}), over the {MAX_SOCKET_PATH_BYTES}-byte \
                 limit for unix sockets; set a shorter [subagent].state_dir",
                socket.display()
            );
        }
        Ok(())
    }

    /// Create `<state_dir>` and the pi subtree, 0700 on unix (§4.10).
    pub fn prepare_dirs(&self) -> Result<()> {
        #[cfg(unix)]
        self.check_socket_path()?;
        for dir in [
            self.config.state_dir.clone(),
            self.pi_dir(),
            self.agent_dir(),
            self.sessions_dir(),
        ] {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("creating subagent state dir {}", dir.display()))?;
            restrict_permissions(&dir);
        }
        Ok(())
    }

    /// The connected client, if any. Does not start anything.
    pub async fn client(&self) -> Option<Arc<PiClient>> {
        let state = self.state.lock().await;
        state
            .client
            .as_ref()
            .filter(|c| c.is_connected())
            .map(Arc::clone)
    }

    pub async fn health(&self) -> DaemonHealth {
        let state = self.state.lock().await;
        let connected = state.client.as_ref().is_some_and(|c| c.is_connected());
        let hello = state.client.as_ref().map(|c| c.hello().clone());
        DaemonHealth {
            running: connected,
            pid: state.pid,
            protocol: hello.as_ref().map(|h| h.protocol.version),
            app_version: hello.and_then(|h| h.app_version),
            adopted: state.adopted,
            socket: Some(self.socket_path().display().to_string()),
            transport: state.mode,
        }
    }

    /// Which transport this Portal run settled on. `Daemon` until a start
    /// actually fails the daemon way.
    pub async fn transport_mode(&self) -> TransportMode {
        self.state.lock().await.mode
    }

    /// Idempotent: the transport to run the next task over. Falls back to the
    /// per-task stdio mode — permanently, for this process — the first time a
    /// daemon start shows that this pi has no daemon mode.
    pub async fn ensure_transport(&self) -> Result<Transport> {
        if self.transport_mode().await == TransportMode::Stdio {
            return Ok(Transport::Stdio);
        }
        match self.ensure_running().await {
            Ok(client) => Ok(Transport::Daemon(client)),
            // `await_socket_ready` latches the mode when the failure *was*
            // "this pi has no daemon mode"; any other failure is still one.
            Err(e) if self.transport_mode().await == TransportMode::Stdio => {
                warn!(
                    "pi has no usable daemon mode ({e:#}); falling back to one \
                     `pi --print --mode json` process per task"
                );
                Ok(Transport::Stdio)
            }
            Err(e) => Err(e),
        }
    }

    /// Idempotent: returns a live client, starting or adopting a daemon if needed.
    pub async fn ensure_running(&self) -> Result<Arc<PiClient>> {
        let mut state = self.state.lock().await;

        if state.mode == TransportMode::Stdio {
            anyhow::bail!(
                "this pi has no daemon mode; the sub-agent is running tasks over the \
                 per-task stdio transport"
            );
        }

        if let Some(client) = state.client.as_ref().filter(|c| c.is_connected()) {
            return Ok(Arc::clone(client));
        }
        // A dead client is dropped so the pieces below can be replaced wholesale.
        state.client = None;

        self.prepare_dirs()?;

        #[cfg(unix)]
        {
            let socket = self.socket_path();
            match PiClient::probe_unix(&socket).await {
                Ok(client) => match client.hello().check_protocol() {
                    Ok(()) => {
                        let pid = client.hello().supervisor_pid;
                        info!(
                            "adopted the pi daemon already listening on {} (pid {:?})",
                            socket.display(),
                            pid
                        );
                        state.adopted = state.child.is_none();
                        state.pid = pid.or(state.pid);
                        state.client = Some(Arc::clone(&client));
                        return Ok(client);
                    }
                    Err(e) => {
                        warn!("replacing the daemon on {}: {e:#}", socket.display());
                        client
                            .notify(Command::Shutdown { force: false })
                            .await;
                        wait_for_socket_gone(&socket, REPLACE_TIMEOUT).await;
                    }
                },
                Err(e) => debug!("no usable pi daemon on {}: {e:#}", socket.display()),
            }
        }

        let client = self.spawn_locked(&mut state).await?;
        Ok(client)
    }

    /// Drop the current daemon (if Portal owns it) and start a fresh one.
    #[allow(dead_code)] // operator escape hatch; no tool exposes it yet
    pub async fn restart(&self) -> Result<Arc<PiClient>> {
        self.shutdown(SHUTDOWN_GRACE).await;
        self.ensure_running().await
    }

    /// Spawn pi, wire up stderr capture, and wait for it to accept a client.
    async fn spawn_locked(&self, state: &mut DaemonState) -> Result<Arc<PiClient>> {
        let argv = &self.config.command;
        if argv.is_empty() {
            anyhow::bail!("no pi command configured; set [subagent].command in portal.toml");
        }

        let socket = self.socket_path();
        let mut cmd = TokioCommand::new(&argv[0]);
        cmd.args(&argv[1..]);

        #[cfg(unix)]
        {
            cmd.args(["--mode", "daemon", "--daemon-socket"])
                .arg(&socket);
        }
        #[cfg(not(unix))]
        {
            // Windows named pipes are Phase 4; until then pi is driven over
            // piped stdio, which the same PiClient speaks.
            cmd.args(["--mode", "rpc"]);
        }

        cmd.env_clear();
        cmd.envs(child_environment(&self.config.env_passthrough, |key| std::env::var_os(key)));
        // Inject api_key from portal.toml into daemon env — the provider
        // determines the env var name. This is the ONLY place where config-level
        // auth reaches daemon-spawned sessions.
        if let Some(key) = &self.config.api_key {
            let env_name = match self.config.provider.as_deref() {
                Some("openrouter") => "OPENROUTER_API_KEY",
                Some("anthropic") => "ANTHROPIC_API_KEY",
                Some("openai") => "OPENAI_API_KEY",
                Some("gemini" | "google") => "GEMINI_API_KEY",
                Some("groq") => "GROQ_API_KEY",
                Some("xai") => "XAI_API_KEY",
                _ => "OPENAI_API_KEY", // sensible default
            };
            cmd.env(env_name, key);
        }
        cmd.env("PRIME_AGENT_CODING_AGENT_DIR", self.agent_dir())
            .env("PRIME_AGENT_SESSION_DIR", self.sessions_dir())
            // pi change P1: exit gracefully when Portal's stdin pipe closes.
            .env("PRIME_AGENT_DAEMON_EXIT_ON_STDIN_CLOSE", "1")
            .env("PRIME_AGENT_HEADLESS", "1")
            .current_dir(&self.config.workspace_root)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // On unix the protocol lives on the socket, so stdout is noise. On the
        // stdio fallback it *is* the protocol.
        #[cfg(unix)]
        cmd.stdout(Stdio::null());
        #[cfg(not(unix))]
        cmd.stdout(Stdio::piped());

        let mut child = cmd.spawn().with_context(|| {
            format!("spawning the pi daemon: {}", self.config.command.join(" "))
        })?;
        let pid = child.id();
        // Kept for the lifetime of the daemon; dropping it asks pi to exit.
        let child_stdin = child.stdin.take();
        #[cfg(not(unix))]
        let child_stdout = child.stdout.take();

        if let Some(stderr) = child.stderr.take() {
            spawn_log_capture(stderr, self.log_path());
        }

        info!(
            "spawned pi daemon (pid {:?}); agent dir {}, sessions {}",
            pid,
            self.agent_dir().display(),
            self.sessions_dir().display()
        );

        state.child = Some(child);
        state.child_stdin = child_stdin;
        state.adopted = false;
        state.pid = pid;

        #[cfg(unix)]
        let connect = self.await_socket_ready(state, &socket).await;
        #[cfg(not(unix))]
        let connect = self.await_stdio_ready(state, child_stdout).await;

        match connect {
            Ok(client) => {
                state.client = Some(Arc::clone(&client));
                Ok(client)
            }
            Err(e) => {
                // A failed start must not leave a half-live child around.
                if let Some(mut child) = state.child.take() {
                    let _ = child.start_kill();
                }
                state.child_stdin = None;
                state.pid = None;
                Err(e)
            }
        }
    }

    /// Readiness loop: connect + hello every 100 ms for up to 30 s, aborting
    /// early (with the log tail) if the child dies first.
    ///
    /// Two shapes of failure mean "this pi cannot daemonise" rather than
    /// "this start went wrong", and both latch [`TransportMode::Stdio`]: the
    /// child exiting during startup (pi 0.73.1 prints `Unknown option:
    /// --daemon-socket` and exits 1), and no socket file existing at all
    /// after [`STDIO_FALLBACK_PROBE`]. A socket that exists but does not
    /// answer yet is a slow daemon, not a missing one, so it keeps the full
    /// [`STARTUP_TIMEOUT`].
    #[cfg(unix)]
    async fn await_socket_ready(
        &self,
        state: &mut DaemonState,
        socket: &Path,
    ) -> Result<Arc<PiClient>> {
        let started = time::Instant::now();
        let deadline = started + STARTUP_TIMEOUT;
        let mut last_err;
        loop {
            if let Some(child) = state.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    state.mode = TransportMode::Stdio;
                    anyhow::bail!(
                        "pi daemon exited during startup ({status}){}",
                        self.log_tail_suffix()
                    );
                }
            }

            match PiClient::connect_unix(socket).await {
                Ok(client) => return Ok(client),
                Err(e) => last_err = format!("{e:#}"),
            }

            if !socket.exists() && started.elapsed() >= STDIO_FALLBACK_PROBE {
                state.mode = TransportMode::Stdio;
                anyhow::bail!(
                    "pi never created the daemon socket {} within {STDIO_FALLBACK_PROBE:?}{}",
                    socket.display(),
                    self.log_tail_suffix()
                );
            }

            if time::Instant::now() >= deadline {
                anyhow::bail!(
                    "pi daemon not ready within {STARTUP_TIMEOUT:?}: {last_err}{}",
                    self.log_tail_suffix()
                );
            }
            time::sleep(READINESS_POLL).await;
        }
    }

    /// Degraded transport for platforms without a Unix socket client.
    #[cfg(not(unix))]
    async fn await_stdio_ready(
        &self,
        state: &mut DaemonState,
        stdout: Option<tokio::process::ChildStdout>,
    ) -> Result<Arc<PiClient>> {
        let stdout = stdout.ok_or_else(|| anyhow::anyhow!("pi stdout was not piped"))?;
        let stdin = state
            .child_stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("pi stdin was not piped"))?;
        warn!("using the experimental stdio transport for pi; named-pipe support is pending");
        PiClient::connect_io(stdout, stdin, "pi stdio".to_string(), ConnectPolicy::LENIENT).await
    }

    /// Ask the daemon to stop, then escalate. Adopted daemons get the request
    /// only — Portal never signals a process it does not own.
    pub async fn shutdown(&self, grace: Duration) {
        let mut state = self.state.lock().await;

        if let Some(client) = state.client.take() {
            if client.is_connected() {
                debug!("asking the pi daemon to shut down");
                let _ = time::timeout(
                    DEFAULT_REQUEST_TIMEOUT,
                    client.request(Command::Shutdown { force: false }, DEFAULT_REQUEST_TIMEOUT),
                )
                .await;
            }
        }

        // Closing stdin is the P1 exit signal; do it before escalating.
        state.child_stdin = None;

        let Some(mut child) = state.child.take() else {
            if state.adopted {
                debug!("adopted daemon left running; shutdown requested only");
            }
            state.pid = None;
            return;
        };

        match time::timeout(grace, child.wait()).await {
            Ok(Ok(status)) => {
                debug!("pi daemon exited cleanly ({status})");
                state.pid = None;
                return;
            }
            Ok(Err(e)) => warn!("waiting for the pi daemon failed: {e}"),
            Err(_) => warn!("pi daemon ignored shutdown after {grace:?}; terminating"),
        }

        let _ = child.start_kill();
        if time::timeout(SIGKILL_GRACE, child.wait()).await.is_err() {
            warn!("pi daemon survived termination; killing");
            let _ = child.kill().await;
        }
        state.pid = None;
    }

    // ── per-task stdio transport ────────────────────────────────────

    /// Run one task as its own `pi --print --mode json` process.
    ///
    /// This is the whole of the stdio fallback: no daemon, no session reuse,
    /// no steering. The prompt goes in as a positional message (`--print` is
    /// a flag, not an option that takes one), pi's JSONL lands on stdout and
    /// is folded into a [`StdioResult`] as it streams, and its human-readable
    /// noise on stderr is appended to the same `daemon.log`.
    ///
    /// Cancellation is by drop: the child is `kill_on_drop`, so aborting the
    /// future kills pi. The only deadline enforced here is `task.timeout` —
    /// turn and token budgets have no CLI equivalent.
    pub async fn run_stdio_task(&self, task: StdioTask) -> Result<StdioRun> {
        let argv = &self.config.command;
        if argv.is_empty() {
            anyhow::bail!("no pi command configured; set [subagent].command in portal.toml");
        }
        self.prepare_dirs()?;

        let mut cmd = TokioCommand::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.args(["--print", "--mode", "json"]);
        if let Some(provider) = task.provider.as_deref() {
            cmd.args(["--provider", provider]);
        }
        if let Some(model) = task.model.as_deref() {
            cmd.args(["--model", model]);
        }
        if let Some(thinking) = task.thinking.as_deref() {
            cmd.args(["--thinking", thinking]);
        }
        if let Some(key) = task.api_key.as_deref() {
            cmd.args(["--api-key", key]);
        }
        cmd.arg(&task.prompt);

        cmd.env_clear();
        cmd.envs(child_environment(&self.config.env_passthrough, |key| std::env::var_os(key)));
        cmd.env("PRIME_AGENT_CODING_AGENT_DIR", self.agent_dir())
            .env("PRIME_AGENT_SESSION_DIR", self.sessions_dir())
            .env("PRIME_AGENT_HEADLESS", "1")
            .current_dir(&task.workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning a pi task: {}", argv.join(" ")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("pi stdout was not piped"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("pi stderr was not piped"))?;

        debug!(
            "running a pi task (pid {:?}) in {}",
            child.id(),
            task.workdir.display()
        );

        let stdout_reader = tokio::spawn(fold_stdout(stdout, task.transcript.clone()));
        let stderr_reader = tokio::spawn(read_stderr_tail(stderr, STDIO_STDERR_TAIL_BYTES));

        let (exit_code, timed_out) = match time::timeout(task.timeout, child.wait()).await {
            Ok(Ok(status)) => (status.code(), false),
            Ok(Err(e)) => {
                warn!("waiting for the pi task failed: {e}");
                (None, false)
            }
            Err(_) => {
                warn!("pi task exceeded {:?}; terminating", task.timeout);
                let _ = child.start_kill();
                let _ = time::timeout(STDIO_KILL_GRACE, child.wait()).await;
                (None, true)
            }
        };

        // The readers end at EOF, which the exit above guarantees — but a
        // grandchild holding the pipe open must not hang the task forever.
        let result = time::timeout(STDIO_KILL_GRACE, stdout_reader)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let stderr_tail = time::timeout(STDIO_KILL_GRACE, stderr_reader)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        if !stderr_tail.is_empty() {
            append_log(&self.log_path(), &stderr_tail);
        }

        Ok(StdioRun {
            result,
            exit_code,
            timed_out,
            stderr_tail,
        })
    }

    /// `" (daemon.log tail: …)"`, or empty when there is nothing to show.
    fn log_tail_suffix(&self) -> String {
        match read_log_tail(&self.log_path(), DAEMON_LOG_TAIL_BYTES) {
            Some(tail) if !tail.trim().is_empty() => format!("; daemon.log tail: {}", tail.trim()),
            _ => String::new(),
        }
    }

    /// Resolve the pi binary: explicit config, then Portal's bundled copy,
    /// then `pi` / `prime-agent` on PATH. `None` ⇒ the sub-agent is unavailable
    /// and its tools are not advertised.
    pub fn resolve_command(configured: Option<&[String]>) -> Option<Vec<String>> {
        Self::resolve_command_with_bundled(configured, bundled_pi_root().as_deref())
    }

    /// [`Self::resolve_command`] with the bundled install root made explicit,
    /// so tests can point it at a scratch directory instead of `$HOME`.
    pub fn resolve_command_with_bundled(
        configured: Option<&[String]>,
        bundled_root: Option<&Path>,
    ) -> Option<Vec<String>> {
        if let Some(argv) = configured.filter(|a| !a.is_empty()) {
            let program = expand_home(&argv[0]);
            let resolved = if argv[0].contains('/') || argv[0].contains('\\') {
                program.exists().then(|| program.display().to_string())
            } else {
                find_on_path(&argv[0]).map(|p| p.display().to_string())
            }?;
            let mut out = vec![resolved];
            out.extend(argv[1..].iter().cloned());
            return Some(out);
        }

        if let Some(root) = bundled_root {
            let bundled = bundled_pi_binary(root);
            if bundled.exists() {
                return Some(vec![bundled.display().to_string()]);
            }
        }

        PI_BINARY_CANDIDATES
            .iter()
            .find_map(|name| find_on_path(name))
            .map(|p| vec![p.display().to_string()])
    }
}

// ── auto-provisioning ───────────────────────────────────────────────

/// `~/.heart-portal/pi`, or `None` when there is no home directory to put
/// it under.
pub fn bundled_pi_root() -> Option<PathBuf> {
    home_dir().map(|_| expand_home(BUNDLED_PI_ROOT))
}

/// `<root>/bin/pi` — the path `resolve_command` looks at.
pub fn bundled_pi_binary(root: &Path) -> PathBuf {
    root.join("bin").join("pi")
}

/// `<root>/VERSION` — which pinned version the install under `root` is.
pub fn bundled_pi_version_file(root: &Path) -> PathBuf {
    root.join("VERSION")
}

/// Where npm puts the launcher for the package's `bin` entry.
fn npm_bin_pi(root: &Path) -> PathBuf {
    root.join("node_modules").join(".bin").join("pi")
}

/// Marker for the last automatic attempt (unix seconds), so a failing
/// install is not retried on every start.
fn last_attempt_file(root: &Path) -> PathBuf {
    root.join(".provision-attempt")
}

/// What [`auto_provision_pi`] found or did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provision {
    /// pi is installed at this path at the pinned version — either it was
    /// already, or it just got installed.
    Ready(PathBuf),
    /// No `npm` on PATH; nothing was attempted.
    NoNpm,
    /// An automatic attempt failed recently and the backoff has not elapsed.
    Deferred,
    /// npm ran and did not produce a working binary. The string is the
    /// human-readable reason (npm's stderr tail, a timeout, …).
    Failed(String),
    /// Auto-provisioning is not implemented for this platform.
    Unsupported,
}

impl Provision {
    /// The installed binary, if there is one.
    pub fn binary(&self) -> Option<&Path> {
        match self {
            Provision::Ready(path) => Some(path),
            _ => None,
        }
    }
}

/// True when the install under `root` is complete and at the pinned version.
pub fn bundled_pi_is_current(root: &Path) -> bool {
    bundled_pi_binary(root).exists()
        && std::fs::read_to_string(bundled_pi_version_file(root))
            .map(|v| v.trim() == PI_PINNED_VERSION)
            .unwrap_or(false)
}

/// Whether `resolved` (the output of `resolve_command`) calls for a
/// provisioning attempt: no pi at all, or Portal's own bundled copy at some
/// other version than the pinned one. A pi found on PATH is the user's and is
/// left alone.
pub fn bundled_pi_wants_provisioning(resolved: Option<&[String]>, root: &Path) -> bool {
    match resolved {
        None => true,
        Some(argv) => {
            let bundled = bundled_pi_binary(root);
            argv.first().is_some_and(|p| Path::new(p) == bundled) && !bundled_pi_is_current(root)
        }
    }
}

/// Install Portal's own pinned pi under `~/.heart-portal/pi` if it is not
/// already there at [`PI_PINNED_VERSION`]. Best effort, never fatal: every
/// way this can go wrong is a [`Provision`] variant, not an error.
///
/// `force` skips the retry backoff — for an explicit request from the being,
/// as opposed to Portal's own startup check.
///
/// Blocks for up to [`NPM_INSTALL_TIMEOUT`]; call it from a blocking
/// context (`spawn_blocking`) when inside the runtime.
pub fn auto_provision_pi(force: bool) -> Provision {
    let Some(root) = bundled_pi_root() else {
        return Provision::Failed("no home directory to install pi under".to_string());
    };
    let npm = find_on_path(if cfg!(windows) { "npm.cmd" } else { "npm" });
    provision_pi_into(&root, npm.as_deref(), force)
}

/// The testable core of [`auto_provision_pi`]: `root` and the npm program are
/// explicit. `npm = None` models a machine without npm.
pub fn provision_pi_into(root: &Path, npm: Option<&Path>, force: bool) -> Provision {
    if bundled_pi_is_current(root) {
        return Provision::Ready(bundled_pi_binary(root));
    }

    let Some(npm) = npm else {
        warn!("Could not auto-install pi (npm not found), sub-agent will be unavailable");
        return Provision::NoNpm;
    };

    if !cfg!(unix) {
        warn!("auto-installing pi is only supported on unix; install pi manually");
        return Provision::Unsupported;
    }

    if !force && attempted_recently(root) {
        info!(
            "skipping pi auto-install: the last attempt failed less than {}h ago",
            PROVISION_RETRY_BACKOFF.as_secs() / 3600
        );
        return Provision::Deferred;
    }

    if let Err(e) = std::fs::create_dir_all(root) {
        return Provision::Failed(format!("creating {}: {e}", root.display()));
    }
    record_attempt(root);

    match std::fs::read_to_string(bundled_pi_version_file(root)) {
        Ok(old) => info!(
            "Auto-installing pi agent... (replacing {} with {PI_PINNED_VERSION})",
            old.trim()
        ),
        Err(_) => info!(
            "Auto-installing pi agent... ({PI_NPM_PACKAGE}@{PI_PINNED_VERSION} into {})",
            root.display()
        ),
    }
    if let Err(reason) = run_npm_install(npm, root) {
        warn!("Could not auto-install pi: {reason}; sub-agent will be unavailable");
        return Provision::Failed(reason);
    }

    let target = npm_bin_pi(root);
    if !target.exists() {
        let reason = format!(
            "npm finished but {} does not exist; the package layout may have changed",
            target.display()
        );
        warn!("Could not auto-install pi: {reason}; sub-agent will be unavailable");
        return Provision::Failed(reason);
    }

    let binary = bundled_pi_binary(root);
    if let Err(e) = link_binary(&target, &binary) {
        let reason = format!("linking {} -> {}: {e}", binary.display(), target.display());
        warn!("Could not auto-install pi: {reason}; sub-agent will be unavailable");
        return Provision::Failed(reason);
    }

    if let Err(e) = std::fs::write(bundled_pi_version_file(root), format!("{PI_PINNED_VERSION}\n")) {
        // The install is usable; only the pin is missing, so the next start
        // will re-install. Say so rather than fail.
        warn!("pi installed but could not write its VERSION file: {e}");
    }
    let _ = std::fs::remove_file(last_attempt_file(root));

    info!("pi agent installed successfully ({PI_PINNED_VERSION} at {})", binary.display());
    Provision::Ready(binary)
}

/// `npm install --prefix <root> <package>@<version>`, killed after
/// [`NPM_INSTALL_TIMEOUT`]. `Err` carries a one-line reason.
fn run_npm_install(npm: &Path, root: &Path) -> std::result::Result<(), String> {
    let spec = format!("{PI_NPM_PACKAGE}@{PI_PINNED_VERSION}");
    let mut child = std::process::Command::new(npm)
        .arg("install")
        .arg("--prefix")
        .arg(root)
        .arg("--no-fund")
        .arg("--no-audit")
        .arg("--loglevel=error")
        .arg(&spec)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawning {}: {e}", npm.display()))?;

    // Drain stderr on a helper thread so a chatty npm cannot block on a
    // full pipe while we wait for it.
    let stderr = child.stderr.take();
    let stderr_reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        if let Some(mut stderr) = stderr {
            let _ = stderr.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = std::time::Instant::now() + NPM_INSTALL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!("npm install did not finish within {NPM_INSTALL_TIMEOUT:?}"));
            }
            Ok(None) => std::thread::sleep(NPM_POLL),
            Err(e) => break Err(format!("waiting for npm: {e}")),
        }
    };
    let stderr_tail = stderr_reader
        .join()
        .map(|buf| {
            let from = buf.len().saturating_sub(STDIO_STDERR_TAIL_BYTES);
            String::from_utf8_lossy(&buf[from..]).trim().to_string()
        })
        .unwrap_or_default();

    match status? {
        s if s.success() => Ok(()),
        s => Err(match stderr_tail.is_empty() {
            true => format!("npm install exited {s}"),
            false => format!("npm install exited {s}: {}", last_line(&stderr_tail)),
        }),
    }
}

/// `bin/pi -> node_modules/.bin/pi`, replacing whatever was there.
#[cfg(unix)]
fn link_binary(target: &Path, link: &Path) -> std::io::Result<()> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::symlink_metadata(link) {
        Ok(_) => std::fs::remove_file(link)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn link_binary(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "auto-provisioning pi is unix-only",
    ))
}

fn attempted_recently(root: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(last_attempt_file(root)) else {
        return false;
    };
    let Ok(at) = text.trim().parse::<u64>() else {
        return false;
    };
    let now = unix_now_secs();
    now.saturating_sub(at) < PROVISION_RETRY_BACKOFF.as_secs()
}

fn record_attempt(root: &Path) {
    let _ = std::fs::write(last_attempt_file(root), format!("{}\n", unix_now_secs()));
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Windows runtime dependencies survive `env_clear`, including with an older
/// explicit passthrough list. Values come from this machine, never fixed paths.
/// ProgramFiles lets pi locate Git Bash; SystemRoot is needed by Node's DNS.
fn child_environment(
    configured: &[String],
    lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Vec<(String, std::ffi::OsString)> {
    #[cfg(windows)]
    const SYSTEM_KEYS: &[&str] = &[
        "SystemRoot", "SystemDrive", "ProgramFiles", "ProgramFiles(x86)",
        "USERPROFILE", "APPDATA", "LOCALAPPDATA", "TEMP", "TMP", "COMSPEC",
    ];
    #[cfg(not(windows))]
    const SYSTEM_KEYS: &[&str] = &[];
    configured.iter().map(String::as_str).chain(SYSTEM_KEYS.iter().copied())
        .filter_map(|key| lookup(key).map(|value| (key.to_string(), value)))
        .collect()
}

/// One task to run as its own pi process ([`PiDaemon::run_stdio_task`]).
#[derive(Debug, Clone)]
pub struct StdioTask {
    /// The full prompt, exactly as a daemon session would have received it.
    pub prompt: String,
    /// cwd for the process; the sub-agent's working directory.
    pub workdir: PathBuf,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Passed as `--api-key`. A daemon inherits keys from the environment
    /// instead, which is why this is only needed here.
    pub api_key: Option<String>,
    pub thinking: Option<String>,
    /// Wall clock for the whole run; past it pi is terminated.
    pub timeout: Duration,
    /// Rendered transcript lines, pushed as pi emits them. The run dropping
    /// the sender is the reader's signal that the task is over.
    pub transcript: Option<mpsc::UnboundedSender<String>>,
}

/// What one `pi --print --mode json` run produced.
#[derive(Debug, Clone)]
pub struct StdioRun {
    pub result: StdioResult,
    /// `None` when pi was killed (timeout) or never reported a code.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Tail of pi's stderr, for explaining a failure in the callback.
    pub stderr_tail: String,
}

impl StdioRun {
    /// Why this run should be reported as a failure, if it should. A non-zero
    /// exit with a clean JSON stream still counts: pi said it went wrong.
    pub fn error(&self) -> Option<String> {
        if let Some(message) = self.result.error_message.clone() {
            return Some(message);
        }
        if self.result.failed() {
            return Some(format!(
                "pi stopped with '{}'",
                self.result.stop_reason.as_deref().unwrap_or("error")
            ));
        }
        match self.exit_code {
            Some(0) => None,
            // A kill is the caller's own doing; it reports the timeout itself.
            _ if self.timed_out => None,
            Some(code) => Some(match self.stderr_tail.is_empty() {
                true => format!("pi exited {code}"),
                false => format!("pi exited {code}: {}", last_line(&self.stderr_tail)),
            }),
            None => Some(match self.stderr_tail.is_empty() {
                true => "pi exited without a status".to_string(),
                false => format!("pi exited without a status: {}", last_line(&self.stderr_tail)),
            }),
        }
    }
}

/// Fold pi's stdout JSONL, mirroring anything worth showing into the
/// transcript as it arrives so `portal_subagent_log` still works.
async fn fold_stdout(
    stdout: tokio::process::ChildStdout,
    transcript: Option<mpsc::UnboundedSender<String>>,
) -> StdioResult {
    let mut result = StdioResult::default();
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        result.fold_line(&line);
        if let Some(tx) = transcript.as_ref() {
            if let Some(rendered) = render_stdio_line(&line) {
                let _ = tx.send(rendered);
            }
        }
    }
    result
}

/// pi's `--mode json` lines are shaped like daemon session events, so the
/// transcript renderer works on them verbatim.
fn render_stdio_line(line: &str) -> Option<String> {
    let body = serde_json::from_str(line.trim()).ok()?;
    transcript::render(&body)
}

/// Last `max` bytes of a child's stderr.
async fn read_stderr_tail(mut stderr: tokio::process::ChildStderr, max: usize) -> String {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > max * 2 {
            buf.drain(..buf.len() - max);
        }
    }
    if buf.len() > max {
        buf.drain(..buf.len() - max);
    }
    String::from_utf8_lossy(&buf).trim().to_string()
}

fn append_log(path: &Path, text: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{text}");
    }
}

/// The last non-blank line — where a CLI usually puts the actual complaint.
fn last_line(text: &str) -> String {
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Copy the daemon's stderr into `<state>/pi/daemon.log`, truncating at 5 MiB.
/// The daemon is not detached, so without this its diagnostics would vanish.
fn spawn_log_capture(mut stderr: tokio::process::ChildStderr, path: PathBuf) {
    tokio::spawn(async move {
        use std::io::Write;
        let mut buf = [0u8; 8192];
        loop {
            let n = match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let rotate = std::fs::metadata(&path)
                .map(|m| m.len() > DAEMON_LOG_MAX_BYTES)
                .unwrap_or(false);
            let mut opts = std::fs::OpenOptions::new();
            opts.create(true);
            if rotate {
                opts.write(true).truncate(true);
            } else {
                opts.append(true);
            }
            match opts.open(&path) {
                Ok(mut f) => {
                    let _ = f.write_all(&buf[..n]);
                }
                Err(e) => {
                    debug!("cannot write {}: {e}", path.display());
                    break;
                }
            }
        }
    });
}

fn read_log_tail(path: &Path, max: usize) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    let from = data.len().saturating_sub(max);
    Some(String::from_utf8_lossy(&data[from..]).to_string())
}

#[cfg(unix)]
async fn wait_for_socket_gone(socket: &Path, timeout: Duration) {
    let deadline = time::Instant::now() + timeout;
    while socket.exists() && time::Instant::now() < deadline {
        time::sleep(READINESS_POLL).await;
    }
}

#[cfg(unix)]
fn restrict_permissions(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    // The state dir holds auth.json and transcripts (PRD §9 risk 10).
    if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
        debug!("could not chmod 0700 {}: {e}", dir.display());
    }
}

#[cfg(not(unix))]
fn restrict_permissions(_dir: &Path) {}

/// `~` / `~/...` → the user's home, else unchanged.
pub fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .filter(|p| !p.as_os_str().is_empty())
}

/// Minimal `which`: first executable match in `PATH`.
pub fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Short root: the daemon socket must fit in `sun_path` (~104 bytes) and
    /// macOS `$TMPDIR` alone is ~50.
    fn temp_dir(tag: &str) -> PathBuf {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let dir = PathBuf::from("/tmp").join(format!("pd-{tag}-{}", &id[..8]));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn daemon_for(state_dir: PathBuf) -> PiDaemon {
        PiDaemon::new(PiDaemonConfig {
            command: vec!["/bin/false".to_string()],
            state_dir,
            workspace_root: PathBuf::from("/tmp"),
            env_passthrough: vec!["PATH".to_string()],
            api_key: None, provider: None,
        })
    }

    #[test]
    fn paths_follow_the_documented_layout() {
        let d = daemon_for(PathBuf::from("/state/subagent"));
        assert_eq!(d.pi_dir(), PathBuf::from("/state/subagent/pi"));
        assert_eq!(d.agent_dir(), PathBuf::from("/state/subagent/pi/agent"));
        assert_eq!(d.sessions_dir(), PathBuf::from("/state/subagent/pi/sessions"));
        assert_eq!(d.socket_path(), PathBuf::from("/state/subagent/pi/daemon.sock"));
        assert_eq!(d.log_path(), PathBuf::from("/state/subagent/pi/daemon.log"));
    }

    #[test]
    fn prepare_dirs_creates_the_tree_privately() {
        let root = temp_dir("dirs");
        let d = daemon_for(root.join("subagent"));
        d.prepare_dirs().unwrap();
        assert!(d.agent_dir().is_dir());
        assert!(d.sessions_dir().is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.pi_dir()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "state dir must not be world-readable");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_command_honors_an_explicit_absolute_path() {
        let resolved = PiDaemon::resolve_command(Some(&[
            "/bin/sh".to_string(),
            "--flag".to_string(),
        ]))
        .unwrap();
        assert_eq!(resolved, vec!["/bin/sh".to_string(), "--flag".to_string()]);
    }

    #[test]
    fn resolve_command_rejects_a_missing_explicit_path() {
        assert!(PiDaemon::resolve_command(Some(&[
            "/nonexistent/pi-binary".to_string()
        ]))
        .is_none());
    }

    #[test]
    fn resolve_command_searches_path_for_a_bare_name() {
        let resolved = PiDaemon::resolve_command(Some(&["sh".to_string()])).unwrap();
        assert!(resolved[0].ends_with("/sh"), "{resolved:?}");
    }

    #[test]
    fn resolve_command_is_none_when_pi_is_absent() {
        // An empty PATH and no bundled copy ⇒ the sub-agent is unavailable.
        let empty = temp_dir("nopath");
        let resolved = PiDaemon::resolve_command(Some(&[
            empty.join("pi").display().to_string()
        ]));
        assert!(resolved.is_none());
        let _ = std::fs::remove_dir_all(empty);
    }

    #[test]
    fn expand_home_expands_only_a_leading_tilde() {
        let home = home_dir().unwrap();
        assert_eq!(expand_home("~/x/y"), home.join("x/y"));
        assert_eq!(expand_home("/abs/~/x"), PathBuf::from("/abs/~/x"));
        assert_eq!(expand_home("rel"), PathBuf::from("rel"));
    }

    #[cfg(unix)]
    #[test]
    fn an_overlong_state_dir_is_refused_with_an_actionable_message() {
        // Binding would fail with an opaque EINVAL; say what to change instead.
        let deep = PathBuf::from("/tmp").join("d".repeat(120));
        let d = daemon_for(deep);
        let err = d.prepare_dirs().unwrap_err().to_string();
        assert!(err.contains("unix sockets"), "{err}");
        assert!(err.contains("state_dir"), "{err}");

        // A sane path passes the check.
        let root = temp_dir("shortpath");
        assert!(daemon_for(root.join("s")).prepare_dirs().is_ok());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn health_reports_a_stopped_daemon() {
        let d = daemon_for(PathBuf::from("/state/subagent"));
        let h = d.health().await;
        assert!(!h.running);
        assert!(h.pid.is_none());
        assert!(!h.adopted);
        let json = h.to_json();
        assert_eq!(json["running"], false);
        assert!(json.get("token").is_none(), "health must never carry secrets");
    }

    #[tokio::test]
    async fn spawn_failure_reports_the_log_tail_and_leaves_no_child() {
        let root = temp_dir("failstart");
        let d = PiDaemon::new(PiDaemonConfig {
            command: vec!["/bin/sh".to_string(), "-c".to_string(),
                          "echo 'pi: no provider configured' >&2; exit 3".to_string()],
            state_dir: root.join("subagent"),
            workspace_root: PathBuf::from("/tmp"),
            env_passthrough: vec!["PATH".to_string()],
            api_key: None, provider: None,
        });

        let err = match d.ensure_running().await {
            Ok(_) => panic!("a daemon that exits immediately must not be reported ready"),
            Err(e) => format!("{e:#}"),
        };
        assert!(err.contains("pi daemon exited during startup"), "{err}");

        let health = d.health().await;
        assert!(!health.running);
        assert!(health.pid.is_none(), "a failed start must not leave a pid");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn shutdown_on_a_stopped_daemon_is_a_noop() {
        let d = daemon_for(PathBuf::from("/state/subagent"));
        d.shutdown(Duration::from_millis(50)).await;
        assert!(!d.health().await.running);
    }

    // ── auto-provisioning ───────────────────────────────────────────

    /// A stand-in `npm` that does what a real `npm install --prefix <root>
    /// <pkg>@<ver>` would leave behind: an executable `node_modules/.bin/pi`
    /// under the prefix. Records its argv so the test can check the spec.
    /// `extra` shell runs first (to fail, hang, …).
    #[cfg(unix)]
    fn fake_npm(root: &Path, extra: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join("fake-npm");
        let script = format!(
            r###"#!/bin/sh
echo "argv: $*" >> '{log}'
{extra}
prefix=""
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) prefix="$2"; shift ;;
  esac
  shift
done
mkdir -p "$prefix/node_modules/.bin"
printf '#!/bin/sh\necho fake-pi\n' > "$prefix/node_modules/.bin/pi"
chmod 755 "$prefix/node_modules/.bin/pi"
"###,
            log = root.join("npm-argv.log").display(),
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn provisioning_builds_the_documented_layout_and_pins_the_version() {
        let root = temp_dir("prov");
        let npm = fake_npm(&root, "");
        let install = root.join("pi");

        let outcome = provision_pi_into(&install, Some(&npm), false);
        let binary = bundled_pi_binary(&install);
        assert_eq!(outcome, Provision::Ready(binary.clone()));
        assert_eq!(outcome.binary(), Some(binary.as_path()));

        // bin/pi is a symlink onto npm's launcher, and it runs.
        let link = std::fs::read_link(&binary).expect("bin/pi must be a symlink");
        assert_eq!(link, install.join("node_modules/.bin/pi"));
        assert!(is_executable(&binary), "{}", binary.display());
        assert_eq!(
            std::fs::read_to_string(bundled_pi_version_file(&install)).unwrap().trim(),
            PI_PINNED_VERSION
        );
        assert!(bundled_pi_is_current(&install));
        assert!(
            !last_attempt_file(&install).exists(),
            "a successful install clears the backoff marker"
        );

        // npm was asked for exactly the pinned package into exactly that prefix.
        let argv = std::fs::read_to_string(root.join("npm-argv.log")).unwrap();
        assert!(argv.contains("install"), "{argv}");
        assert!(argv.contains(&format!("--prefix {}", install.display())), "{argv}");
        assert!(argv.contains(&format!("{PI_NPM_PACKAGE}@{PI_PINNED_VERSION}")), "{argv}");

        // Current ⇒ a second call is a no-op: npm is not run again.
        let again = provision_pi_into(&install, Some(&npm), false);
        assert_eq!(again, Provision::Ready(binary));
        assert_eq!(
            std::fs::read_to_string(root.join("npm-argv.log")).unwrap().matches("install").count(),
            1,
            "npm must not run when the pinned version is already installed"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_command_finds_the_bundled_binary_after_provisioning() {
        let root = temp_dir("provres");
        let install = root.join("pi");

        // Nothing installed under the bundled root yet.
        assert!(!bundled_pi_binary(&install).exists());
        assert!(bundled_pi_wants_provisioning(None, &install));

        let npm = fake_npm(&root, "");
        assert!(matches!(provision_pi_into(&install, Some(&npm), false), Provision::Ready(_)));

        let resolved = PiDaemon::resolve_command_with_bundled(None, Some(&install)).unwrap();
        assert_eq!(resolved, vec![bundled_pi_binary(&install).display().to_string()]);
        // The bundled copy at the pinned version does not want re-installing…
        assert!(!bundled_pi_wants_provisioning(Some(&resolved), &install));
        // …but the same copy at another version does.
        std::fs::write(bundled_pi_version_file(&install), "0.0.1\n").unwrap();
        assert!(bundled_pi_wants_provisioning(Some(&resolved), &install));
        // A pi that is not ours is never re-installed, whatever VERSION says.
        assert!(!bundled_pi_wants_provisioning(Some(&["/usr/local/bin/pi".to_string()]), &install));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn a_version_mismatch_reinstalls_over_the_old_link() {
        let root = temp_dir("provver");
        let install = root.join("pi");
        let npm = fake_npm(&root, "");

        // An older install: a stale link and an old VERSION.
        let stale_target = root.join("old-pi");
        std::fs::write(&stale_target, "#!/bin/sh\necho old\n").unwrap();
        std::fs::create_dir_all(install.join("bin")).unwrap();
        std::os::unix::fs::symlink(&stale_target, bundled_pi_binary(&install)).unwrap();
        std::fs::write(bundled_pi_version_file(&install), "0.60.0\n").unwrap();
        assert!(!bundled_pi_is_current(&install));

        assert!(matches!(provision_pi_into(&install, Some(&npm), false), Provision::Ready(_)));
        assert_eq!(
            std::fs::read_link(bundled_pi_binary(&install)).unwrap(),
            install.join("node_modules/.bin/pi"),
            "the link must be replaced, not left pointing at the old binary"
        );
        assert!(bundled_pi_is_current(&install));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn without_npm_provisioning_degrades_gracefully() {
        let root = temp_dir("provnonpm");
        let install = root.join("pi");
        assert_eq!(provision_pi_into(&install, None, false), Provision::NoNpm);
        assert_eq!(provision_pi_into(&install, None, true), Provision::NoNpm);
        assert!(!bundled_pi_binary(&install).exists());
        assert!(!bundled_pi_version_file(&install).exists());
        // Nothing was even attempted, so nothing is on disk to back off from.
        assert!(!install.exists(), "no npm ⇒ no directory churn");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_npm_is_reported_and_backed_off_until_forced() {
        let root = temp_dir("provfail");
        let install = root.join("pi");
        let npm = fake_npm(&root, "echo 'npm ERR! network unreachable' >&2; exit 1");

        match provision_pi_into(&install, Some(&npm), false) {
            Provision::Failed(reason) => {
                assert!(reason.contains("network unreachable"), "{reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(!bundled_pi_binary(&install).exists());
        assert!(!bundled_pi_version_file(&install).exists(), "no pin without a binary");
        assert!(last_attempt_file(&install).exists());

        // Startup does not hammer npm after a fresh failure…
        assert_eq!(provision_pi_into(&install, Some(&npm), false), Provision::Deferred);
        let runs = || std::fs::read_to_string(root.join("npm-argv.log")).unwrap().matches("install").count();
        assert_eq!(runs(), 1);
        // …but an explicit request (portal_subagent_setup) tries again now.
        assert!(matches!(provision_pi_into(&install, Some(&npm), true), Provision::Failed(_)));
        assert_eq!(runs(), 2);

        // An old marker no longer defers.
        std::fs::write(last_attempt_file(&install), "1\n").unwrap();
        assert!(matches!(provision_pi_into(&install, Some(&npm), false), Provision::Failed(_)));
        assert_eq!(runs(), 3);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn npm_that_leaves_no_binary_is_a_failure_not_a_dangling_link() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_dir("provempty");
        let install = root.join("pi");
        // Exits 0 without producing node_modules/.bin/pi.
        let npm = root.join("silent-npm");
        std::fs::write(&npm, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755)).unwrap();

        match provision_pi_into(&install, Some(&npm), true) {
            Provision::Failed(reason) => assert!(reason.contains("does not exist"), "{reason}"),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(std::fs::symlink_metadata(bundled_pi_binary(&install)).is_err());
        assert!(!bundled_pi_version_file(&install).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    // ── stdio fallback ──────────────────────────────────────────────

    /// A stand-in pi that behaves like 0.73.1: no daemon mode at all, but a
    /// working `--print --mode json`. Every invocation appends its argv and
    /// cwd to `<root>/argv.log`.
    #[cfg(unix)]
    fn fake_pi(root: &Path) -> PathBuf {
        fake_pi_with(root, "")
    }

    /// As [`fake_pi`], with `extra` shell run before the JSON is printed.
    #[cfg(unix)]
    fn fake_pi_with(root: &Path, extra: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let log = root.join("argv.log");
        let path = root.join("fake-pi");
        let script = format!(
            r###"#!/bin/sh
echo "argv: $* | cwd: $(pwd)" >> '{log}'
case "$*" in
  *--daemon-socket*) echo 'Error: Unknown option: --daemon-socket' >&2; exit 1 ;;
esac
{extra}
cat <<'JSON'
{{"type":"session","version":3,"id":"fake-1","cwd":"."}}
{{"type":"agent_start"}}
{{"type":"message_end","message":{{"role":"assistant","content":[{{"type":"text","text":"## Result\nthe fake pi did the work."}}],"usage":{{"input":11,"output":7}},"stopReason":"stop"}}}}
{{"type":"turn_end"}}
{{"type":"agent_end","messages":[]}}
JSON
"###,
            log = log.display(),
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    fn argv_log(root: &Path) -> String {
        std::fs::read_to_string(root.join("argv.log")).unwrap_or_default()
    }

    #[cfg(unix)]
    fn daemon_running(root: &Path, command: Vec<String>) -> PiDaemon {
        PiDaemon::new(PiDaemonConfig {
            command,
            state_dir: root.join("subagent"),
            workspace_root: root.to_path_buf(),
            env_passthrough: vec!["PATH".to_string(), "HOME".to_string()],
            api_key: None, provider: None,
        })
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_pi_without_daemon_mode_falls_back_to_the_stdio_transport() {
        let root = temp_dir("fallback");
        let pi = fake_pi(&root);
        let d = daemon_running(&root, vec![pi.display().to_string()]);

        assert_eq!(d.transport_mode().await, TransportMode::Daemon, "daemon first");

        let transport = d.ensure_transport().await.unwrap();
        assert_eq!(transport.mode(), TransportMode::Stdio);
        assert_eq!(d.transport_mode().await, TransportMode::Stdio);
        assert!(
            argv_log(&root).contains("--daemon-socket"),
            "the daemon way must be tried before it is written off"
        );

        // The decision is cached: a pi that cannot daemonise will not learn
        // to, and a doomed spawn per task is pure latency.
        let again = d.ensure_transport().await.unwrap();
        assert_eq!(again.mode(), TransportMode::Stdio);
        assert_eq!(
            argv_log(&root).matches("--daemon-socket").count(),
            1,
            "daemon mode must not be retried"
        );

        // The daemon-only entry point now says why, rather than respawning.
        let err = match d.ensure_running().await {
            Ok(_) => panic!("a pi with no daemon mode must not hand back a client"),
            Err(e) => format!("{e:#}"),
        };
        assert!(err.contains("no daemon mode"), "{err}");
        assert_eq!(d.health().await.to_json()["transport"], "stdio");

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_stdio_task_runs_a_fresh_pi_and_reports_its_result() {
        let root = temp_dir("stdiorun");
        let pi = fake_pi(&root);
        let d = daemon_running(&root, vec![pi.display().to_string()]);
        let workdir = root.join("work");
        std::fs::create_dir_all(&workdir).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let run = d
            .run_stdio_task(StdioTask {
                prompt: "# Task sub_1\nrefactor the thing".to_string(),
                workdir: workdir.clone(),
                provider: Some("openrouter".to_string()),
                model: Some("deepseek/deepseek-chat-v4-0324".to_string()),
                api_key: Some("sk-or-test".to_string()),
                thinking: None,
                timeout: Duration::from_secs(30),
                transcript: Some(tx),
            })
            .await
            .unwrap();

        assert_eq!(run.exit_code, Some(0));
        assert!(!run.timed_out);
        assert_eq!(run.error(), None);
        assert_eq!(
            run.result.last_assistant_text.as_deref(),
            Some("## Result\nthe fake pi did the work.")
        );
        assert_eq!(run.result.turns, 1);
        assert_eq!(run.result.usage.total, 18);

        let argv = argv_log(&root);
        assert!(argv.contains("--print --mode json"), "{argv}");
        assert!(argv.contains("--provider openrouter"), "{argv}");
        assert!(argv.contains("--model deepseek/deepseek-chat-v4-0324"), "{argv}");
        assert!(argv.contains("--api-key sk-or-test"), "{argv}");
        assert!(argv.contains("refactor the thing"), "the prompt is the last argument: {argv}");
        // `pwd` resolves /tmp → /private/tmp on macOS, so compare realpaths.
        let real_workdir = workdir.canonicalize().unwrap();
        assert!(
            argv.contains(&format!("cwd: {}", real_workdir.display())),
            "the task runs in its own workdir: {argv}"
        );

        // Progress was mirrored while it ran, not only at the end.
        drop(d);
        let mut lines = Vec::new();
        while let Ok(line) = rx.try_recv() {
            lines.push(line);
        }
        assert!(
            lines.iter().any(|l| l.contains("the fake pi did the work")),
            "{lines:?}"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_stdio_task_that_overruns_its_budget_is_killed() {
        let root = temp_dir("stdiokill");
        let pi = fake_pi_with(&root, "sleep 30");
        let d = daemon_running(&root, vec![pi.display().to_string()]);

        let started = time::Instant::now();
        let run = d
            .run_stdio_task(StdioTask {
                prompt: "take too long".to_string(),
                workdir: root.clone(),
                provider: None,
                model: None,
                api_key: None,
                thinking: None,
                timeout: Duration::from_millis(300),
                transcript: None,
            })
            .await
            .unwrap();

        assert!(run.timed_out);
        assert_eq!(run.exit_code, None);
        assert_eq!(run.error(), None, "the timeout is the caller's to report");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "a timed-out task must not wait for the child to finish on its own"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_stdio_task_explains_itself_from_stderr() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_dir("stdiofail");
        let pi = root.join("broken-pi");
        std::fs::write(
            &pi,
            "#!/bin/sh\necho 'pi: no provider configured' >&2\nexit 2\n",
        )
        .unwrap();
        std::fs::set_permissions(&pi, std::fs::Permissions::from_mode(0o755)).unwrap();
        let d = daemon_running(&root, vec![pi.display().to_string()]);

        let run = d
            .run_stdio_task(StdioTask {
                prompt: "anything".to_string(),
                workdir: root.clone(),
                provider: None,
                model: None,
                api_key: None,
                thinking: None,
                timeout: Duration::from_secs(10),
                transcript: None,
            })
            .await
            .unwrap();

        assert_eq!(run.exit_code, Some(2));
        let error = run.error().unwrap();
        assert!(error.contains("no provider configured"), "{error}");
        assert!(
            read_log_tail(&d.log_path(), 4096)
                .unwrap_or_default()
                .contains("no provider configured"),
            "stderr belongs in daemon.log too"
        );

        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(all(test, windows))]
mod windows_env_tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;

    #[test]
    fn runtime_environment_survives_default_and_legacy_configs() {
        // Simulate different installation drives and Unicode paths without
        // mutating process-wide environment variables in parallel tests.
        let machine = HashMap::from([
            ("SystemRoot", OsString::from(r"D:\Windows")),
            ("ProgramFiles", OsString::from(r"E:\Applications 空格")),
            ("ProgramFiles(x86)", OsString::from(r"F:\Legacy Apps")),
            ("PATH", OsString::from(r"E:\Portable Git\bin")),
            ("MY_PROVIDER_TOKEN", OsString::from("explicit-credential")),
            ("UNRELATED_SECRET", OsString::from("do-not-inherit")),
        ]);
        for configured in [
            crate::config::SubagentConfig::default().env_passthrough,
            vec!["PATH".to_string(), "MY_PROVIDER_TOKEN".to_string()],
            vec![],
        ] {
            let child: HashMap<_, _> = child_environment(&configured, |key| machine.get(key).cloned())
                .into_iter().collect();
            for key in ["SystemRoot", "ProgramFiles", "ProgramFiles(x86)"] {
                assert_eq!(child.get(key), machine.get(key), "lost machine path {key}");
            }
            assert_eq!(child.contains_key("MY_PROVIDER_TOKEN"), configured.iter().any(|k| k == "MY_PROVIDER_TOKEN"));
            assert_eq!(child.contains_key("PATH"), configured.iter().any(|k| k == "PATH"));
            assert!(!child.contains_key("UNRELATED_SECRET"));
            assert!(!child.contains_key("LOCALAPPDATA"), "do not invent missing paths");
        }
    }

    #[test]
    fn child_process_can_start_with_an_empty_legacy_allowlist() {
        let root = std::env::var_os("SystemRoot").unwrap();
        let mut cmd = std::process::Command::new(PathBuf::from(root).join("System32").join("cmd.exe"));
        let output = cmd.env_clear()
            .envs(child_environment(&[], |key| std::env::var_os(key)))
            .args(["/d", "/c", "if defined SystemRoot (echo WINDOWS_RUNTIME_OK) else (exit /b 1)"])
            .output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "WINDOWS_RUNTIME_OK");
    }
}
