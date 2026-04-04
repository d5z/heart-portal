//! Background process manager for portal_exec (background) + portal_process tools.

use crate::config::PortalConfig;
use crate::exec_policy::{configure_shell_command, validate_exec_allowlist};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Mutex, Notify};
use tokio::time;
use tracing::debug;

const DEFAULT_MAX_SESSIONS: usize = 10;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const KILL_GRACE: Duration = Duration::from_secs(5);
const EXIT_RETENTION: Duration = Duration::from_secs(5 * 60);
/// Long-poll cap: prevents MCP clients from holding connections indefinitely.
pub const MAX_POLL_TIMEOUT_MS: u64 = 300_000;
/// Single stdin write cap (interactive prompts).
pub const MAX_STDIN_WRITE_BYTES: usize = 256 * 1024;
/// Session ids are `sess_` + UUID; reject oversized / odd keys.
pub const MAX_SESSION_ID_BYTES: usize = 128;

pub struct ProcessManager {
    sessions: Arc<Mutex<HashMap<String, ManagedProcess>>>,
    max_sessions: usize,
    max_output_bytes: usize,
}

pub struct ManagedProcess {
    pub session_id: String,
    pub pid: u32,
    pub command: String,
    pub started_at: tokio::time::Instant,
    pub stdin: Option<ChildStdin>,
    pub output: Arc<Mutex<OutputBuffer>>,
    pub status: Arc<Mutex<ProcessStatus>>,
    pub(crate) notify: Arc<Notify>,
    pub(crate) exited_at: Option<tokio::time::Instant>,
}

pub struct OutputBuffer {
    pub data: Vec<u8>,
    pub max_bytes: usize,
    total_written: u64,
    /// Last time stdout/stderr delivered bytes (spawn time until first byte).
    last_output_at: time::Instant,
}

impl OutputBuffer {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            data: Vec::new(),
            max_bytes,
            total_written: 0,
            last_output_at: time::Instant::now(),
        }
    }

    pub fn total_written(&self) -> u64 {
        self.total_written
    }

    /// Seconds since last captured output (or since buffer creation if none yet).
    pub fn idle_s(&self) -> u64 {
        self.last_output_at.elapsed().as_secs()
    }

    pub fn append(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.last_output_at = time::Instant::now();
        self.total_written += chunk.len() as u64;
        self.data.extend_from_slice(chunk);
        if self.data.len() > self.max_bytes {
            let drop = self.data.len() - self.max_bytes;
            self.data.drain(..drop);
        }
    }

    /// Returns bytes from logical `offset` to current end, whether data was dropped before `offset`, and `total_written`.
    pub fn bytes_since(&self, offset: u64) -> (Vec<u8>, bool, u64) {
        let start_offset = self
            .total_written
            .saturating_sub(self.data.len() as u64);
        let truncated = offset < start_offset;
        let from = offset.max(start_offset);
        if from >= self.total_written || self.data.is_empty() {
            return (vec![], truncated, self.total_written);
        }
        let start_idx = (from - start_offset) as usize;
        (self.data[start_idx..].to_vec(), truncated, self.total_written)
    }

    pub fn bytes_range(&self, offset: u64, limit: usize) -> (Vec<u8>, u64) {
        let start_offset = self
            .total_written
            .saturating_sub(self.data.len() as u64);
        let from = offset.max(start_offset);
        if from >= self.total_written || self.data.is_empty() {
            return (vec![], self.total_written);
        }
        let start_idx = (from - start_offset) as usize;
        let end = (start_idx + limit).min(self.data.len());
        (self.data[start_idx..end].to_vec(), self.total_written)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessStatus {
    Running,
    Exited(i32),
}

#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub session_id: String,
    pub pid: u32,
    pub command: String,
    pub status: ProcessStatus,
    pub uptime_s: u64,
    /// Seconds since last stdout/stderr chunk (sensory: silence vs progress).
    pub idle_s: u64,
    /// Total bytes captured (may exceed ring size; monotonic).
    pub total_output_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct PollResult {
    pub output: Vec<u8>,
    pub next_offset: u64,
    pub truncated: bool,
    pub status: ProcessStatus,
    pub idle_s: u64,
    pub total_output_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct LogResult {
    pub output: Vec<u8>,
    pub next_offset: u64,
    pub truncated: bool,
    pub status: ProcessStatus,
    pub idle_s: u64,
    pub total_output_bytes: u64,
}

pub fn validate_session_id(session_id: &str) -> Result<()> {
    if session_id.is_empty() || session_id.len() > MAX_SESSION_ID_BYTES {
        anyhow::bail!("Invalid session_id");
    }
    if !session_id.starts_with("sess_") {
        anyhow::bail!("Invalid session_id");
    }
    Ok(())
}

async fn read_into_buffer<R: tokio::io::AsyncRead + Unpin>(
    mut stream: R,
    output: Arc<Mutex<OutputBuffer>>,
    notify: Arc<Notify>,
) {
    let mut buf = [0u8; 8192];
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        let mut o = output.lock().await;
        o.append(&buf[..n]);
        drop(o);
        notify.notify_waiters();
    }
}

impl ProcessManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            max_sessions: DEFAULT_MAX_SESSIONS,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    pub async fn spawn(
        &self,
        config: &PortalConfig,
        command: &str,
        workdir: &str,
        extra_env: &[(String, String)],
    ) -> Result<SessionInfo> {
        validate_exec_allowlist(command, &config.security.exec_allowlist)?;

        let running = {
            let g = self.sessions.lock().await;
            let mut n = 0;
            for s in g.values() {
                if matches!(*s.status.lock().await, ProcessStatus::Running) {
                    n += 1;
                }
            }
            n
        };
        if running >= self.max_sessions {
            anyhow::bail!(
                "Maximum concurrent background sessions ({}) reached",
                self.max_sessions
            );
        }

        let session_id = format!("sess_{}", uuid::Uuid::new_v4());
        let output = Arc::new(Mutex::new(OutputBuffer::new(self.max_output_bytes)));
        let status = Arc::new(Mutex::new(ProcessStatus::Running));
        let notify = Arc::new(Notify::new());

        let mut cmd = Command::new("sh");
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        configure_shell_command(&mut cmd, command, config, workdir);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn: {}", e))?;
        let pid = child.id().unwrap_or(0);
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("stdout not piped"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("stderr not piped"))?;

        let out_a = Arc::clone(&output);
        let n_a = Arc::clone(&notify);
        tokio::spawn(async move {
            read_into_buffer(stdout, out_a, n_a).await;
        });
        let out_b = Arc::clone(&output);
        let n_b = Arc::clone(&notify);
        tokio::spawn(async move {
            read_into_buffer(stderr, out_b, n_b).await;
        });

        let st_b = Arc::clone(&status);
        let n_exit = Arc::clone(&notify);
        let sessions_wait = Arc::clone(&self.sessions);
        let sid_wait = session_id.clone();
        tokio::spawn(async move {
            let code = match child.wait().await {
                Ok(s) => s.code().unwrap_or(-1),
                Err(_) => -1,
            };
            let now = tokio::time::Instant::now();
            {
                let mut g = sessions_wait.lock().await;
                if let Some(p) = g.get_mut(&sid_wait) {
                    p.exited_at = Some(now);
                }
            }
            let mut st = st_b.lock().await;
            *st = ProcessStatus::Exited(code);
            n_exit.notify_waiters();
        });

        let started_at = tokio::time::Instant::now();
        let proc = ManagedProcess {
            session_id: session_id.clone(),
            pid,
            command: command.to_string(),
            started_at,
            stdin,
            output: Arc::clone(&output),
            status: Arc::clone(&status),
            notify: Arc::clone(&notify),
            exited_at: None,
        };

        self.sessions.lock().await.insert(session_id.clone(), proc);

        debug!(
            "spawned background session {} pid {} ({})",
            session_id, pid, command
        );

        Ok(SessionInfo {
            session_id,
            pid,
            command: command.to_string(),
            status: ProcessStatus::Running,
            uptime_s: 0,
            idle_s: 0,
            total_output_bytes: 0,
        })
    }

    pub async fn poll(
        &self,
        session_id: &str,
        offset: u64,
        timeout_ms: u64,
    ) -> Result<PollResult> {
        validate_session_id(session_id)?;
        let timeout_ms = timeout_ms.min(MAX_POLL_TIMEOUT_MS);
        let deadline = if timeout_ms > 0 {
            Some(time::Instant::now() + Duration::from_millis(timeout_ms))
        } else {
            None
        };

        loop {
            let (bytes, truncated, next, st, notify, idle_s, total_out) = {
                let guard = self.sessions.lock().await;
                let s = guard
                    .get(session_id)
                    .ok_or_else(|| anyhow::anyhow!("Unknown session: {}", session_id))?;
                let (bytes, truncated, next, idle_s, total_out) = {
                    let buf = s.output.lock().await;
                    let (bytes, truncated, next) = buf.bytes_since(offset);
                    let idle_s = buf.idle_s();
                    let total_out = buf.total_written();
                    (bytes, truncated, next, idle_s, total_out)
                };
                let st = s.status.lock().await.clone();
                let n = Arc::clone(&s.notify);
                (bytes, truncated, next, st, n, idle_s, total_out)
            };

            if !bytes.is_empty() || matches!(st, ProcessStatus::Exited(_)) {
                return Ok(PollResult {
                    output: bytes,
                    next_offset: next,
                    truncated,
                    status: st,
                    idle_s,
                    total_output_bytes: total_out,
                });
            }

            let Some(dl) = deadline else {
                return Ok(PollResult {
                    output: vec![],
                    next_offset: next,
                    truncated,
                    status: st,
                    idle_s,
                    total_output_bytes: total_out,
                });
            };

            if time::Instant::now() >= dl {
                return Ok(PollResult {
                    output: vec![],
                    next_offset: next,
                    truncated,
                    status: st,
                    idle_s,
                    total_output_bytes: total_out,
                });
            }

            let sleep = time::sleep_until(dl);
            tokio::select! {
                _ = notify.notified() => {}
                _ = sleep => {}
            }
        }
    }

    pub async fn log(&self, session_id: &str, offset: u64, limit: u64) -> Result<LogResult> {
        validate_session_id(session_id)?;
        let limit = limit.min(self.max_output_bytes as u64) as usize;
        let guard = self.sessions.lock().await;
        let s = guard
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown session: {}", session_id))?;
        let (output, next_offset, truncated, idle_s, total_out) = {
            let buf = s.output.lock().await;
            let idle_s = buf.idle_s();
            let total_out = buf.total_written();
            let (output, next_offset) = buf.bytes_range(offset, limit);
            let start_offset = buf
                .total_written()
                .saturating_sub(buf.data.len() as u64);
            let truncated = offset < start_offset;
            (output, next_offset, truncated, idle_s, total_out)
        };
        let st = s.status.lock().await.clone();
        Ok(LogResult {
            output,
            next_offset,
            truncated,
            status: st,
            idle_s,
            total_output_bytes: total_out,
        })
    }

    pub async fn write_stdin(&self, session_id: &str, data: &[u8]) -> Result<()> {
        validate_session_id(session_id)?;
        if data.len() > MAX_STDIN_WRITE_BYTES {
            anyhow::bail!(
                "stdin write exceeds max {} bytes",
                MAX_STDIN_WRITE_BYTES
            );
        }
        let mut guard = self.sessions.lock().await;
        let s = guard
            .get_mut(session_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown session: {}", session_id))?;
        if matches!(*s.status.lock().await, ProcessStatus::Exited(_)) {
            anyhow::bail!("Session has exited");
        }
        let stdin = s
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("stdin not available for this session"))?;
        stdin.write_all(data).await?;
        stdin.flush().await?;
        Ok(())
    }

    pub async fn kill(&self, session_id: &str) -> Result<()> {
        validate_session_id(session_id)?;
        let pid = {
            let guard = self.sessions.lock().await;
            let s = guard
                .get(session_id)
                .ok_or_else(|| anyhow::anyhow!("Unknown session: {}", session_id))?;
            s.pid
        };

        #[cfg(unix)]
        {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            time::sleep(KILL_GRACE).await;
            let still_running = {
                let guard = self.sessions.lock().await;
                if let Some(s) = guard.get(session_id) {
                    matches!(*s.status.lock().await, ProcessStatus::Running)
                } else {
                    false
                }
            };
            if still_running {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            anyhow::bail!("kill is only supported on Unix");
        }

        Ok(())
    }

    pub async fn list(&self) -> Vec<SessionInfo> {
        let guard = self.sessions.lock().await;
        let mut out = Vec::new();
        for s in guard.values() {
            let st = s.status.lock().await.clone();
            let uptime_s = match &st {
                ProcessStatus::Running => s.started_at.elapsed().as_secs(),
                ProcessStatus::Exited(_) => s
                    .exited_at
                    .map(|ex| ex.saturating_duration_since(s.started_at).as_secs())
                    .unwrap_or(0),
            };
            let (idle_s, total_output_bytes) = {
                let buf = s.output.lock().await;
                (buf.idle_s(), buf.total_written())
            };
            out.push(SessionInfo {
                session_id: s.session_id.clone(),
                pid: s.pid,
                command: s.command.clone(),
                status: st,
                uptime_s,
                idle_s,
                total_output_bytes,
            });
        }
        out
    }

    pub async fn cleanup(&self) {
        let now = tokio::time::Instant::now();
        let keys: Vec<String> = {
            let guard = self.sessions.lock().await;
            guard.keys().cloned().collect()
        };
        for k in keys {
            let remove = {
                let guard = self.sessions.lock().await;
                let Some(p) = guard.get(&k) else {
                    continue;
                };
                let st = p.status.lock().await.clone();
                match st {
                    ProcessStatus::Running => false,
                    ProcessStatus::Exited(_) => {
                        if let Some(ex) = p.exited_at {
                            now.duration_since(ex) >= EXIT_RETENTION
                        } else {
                            false
                        }
                    }
                }
            };
            if remove {
                let mut guard = self.sessions.lock().await;
                guard.remove(&k);
            }
        }
    }

    pub async fn kill_all(&self) {
        let ids: Vec<String> = {
            let g = self.sessions.lock().await;
            g.keys().cloned().collect()
        };
        for id in ids {
            let _ = self.kill(&id).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_buffer_ring_and_offsets() {
        let mut b = OutputBuffer::new(10);
        b.append(b"0123456789");
        let (chunk, trunc, n) = b.bytes_since(0);
        assert!(!trunc);
        assert_eq!(n, 10);
        assert_eq!(chunk, b"0123456789");

        b.append(b"ABCDE");
        assert_eq!(b.data.len(), 10);
        let (_, trunc, n2) = b.bytes_since(0);
        assert!(trunc);
        assert_eq!(n2, 15);
        let (chunk2, _, _) = b.bytes_since(10);
        assert_eq!(chunk2, b"ABCDE");
    }

    #[test]
    fn validate_session_id_accepts_spawn_ids() {
        assert!(validate_session_id("sess_550e8400-e29b-41d4-a716-446655440000").is_ok());
    }

    #[test]
    fn validate_session_id_rejects_bad() {
        assert!(validate_session_id("").is_err());
        assert!(validate_session_id("nope").is_err());
        assert!(validate_session_id(&format!("sess_{}", "x".repeat(200))).is_err());
    }
}
