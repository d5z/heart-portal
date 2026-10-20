//! Exec tool — run shell commands.

use crate::config::PortalConfig;
use crate::exec_policy::{
    configure_shell_command, validate_exec_allowlist, validate_shell_command, ExecShell,
};
use crate::process_manager::ProcessManager;
use crate::tools::text::{OutputDecoder, OutputEncoding};
use anyhow::Result;
use serde_json::Value;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tracing::debug;

pub async fn execute(
    config: &PortalConfig,
    process_manager: &Arc<ProcessManager>,
    arguments: Value,
) -> Result<Value> {
    let command = arguments
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'command' argument"))?;
    let shell = ExecShell::parse(arguments.get("shell"))?;
    validate_shell_command(shell, command)?;
    let output_encoding = OutputEncoding::parse(arguments.get("output_encoding"))?.for_shell(shell);

    let workdir = arguments
        .get("workdir")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| config.security.workspace_root.to_string_lossy().to_string());

    let timeout_secs = arguments
        .get("timeout_secs")
        .and_then(super::value_as_u64)
        .unwrap_or_else(|| {
            debug!("Missing timeout_secs argument, using default 30 seconds");
            30
        })
        .min(300);

    let max_output_bytes = arguments
        .get("max_output_bytes")
        .and_then(super::value_as_u64)
        .unwrap_or(100_000)
        .clamp(1, 100_000) as usize;
    let started = std::time::Instant::now();
    let background = arguments
        .get("background")
        .and_then(super::value_as_bool)
        .unwrap_or_else(|| {
            debug!("Missing background argument, using synchronous execution");
            false
        });

    validate_exec_allowlist(command, &config.security.exec_allowlist)?;

    if background {
        let info = process_manager
            .spawn_with_shell(config, command, &workdir, &[], shell, output_encoding)
            .await?;
        return Ok(serde_json::json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&serde_json::json!({
                    "session_id": info.session_id,
                    "pid": info.pid,
                    "status": "running",
                    "output_encoding": info.output_encoding.as_str()
                }))?
            }],
            "isError": false
        }));
    }

    debug!(
        "exec: {} (workdir: {}, timeout: {}s)",
        command, workdir, timeout_secs
    );

    let mut cmd = Command::new(shell.program());
    configure_shell_command(&mut cmd, command, config, &workdir, shell);

    // Drain both pipes concurrently, retaining bounded UTF-8 output while counting all bytes.
    // kill_on_drop also covers cancellation of the tool call or its timeout future.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let (mut child, owner) = crate::mcp::ProcessOwner::spawn_exec(&mut cmd)
        .map_err(|e| anyhow::anyhow!("Failed to execute: {}", e))?;
    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");
    let (status, stdout_capture, stderr_capture) =
        tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
            let wait = async {
                #[cfg(unix)]
                {
                    owner.wait_unreaped().await?;
                    owner.terminate();
                }
                let status = child.wait().await?;
                #[cfg(windows)]
                owner.terminate();
                Ok(status)
            };
            tokio::try_join!(
                wait,
                capture(stdout_pipe, output_encoding, max_output_bytes),
                capture(stderr_pipe, output_encoding, max_output_bytes)
            )
        })
        .await
        .map_err(|_| anyhow::anyhow!("Command timed out after {}s", timeout_secs))?
        .map_err(|e| anyhow::anyhow!("Failed to execute: {}", e))?;
    let exit_code = status.code().unwrap_or(-1);
    let stdout_bytes = stdout_capture.total;
    let stderr_bytes = stderr_capture.total;
    let stdout = stdout_capture.render();
    let stderr = stderr_capture.render();

    let mut text = format!(
        "{}{}",
        stdout,
        if !stderr.is_empty() {
            format!("\n--- stderr ---\n{}", stderr)
        } else {
            String::new()
        },
    );
    if exit_code != 0 {
        text.push_str(&format!("\n(exit code: {})", exit_code));
    }

    // Truncate large outputs to avoid flooding the being's context
    let truncated = text.len() > max_output_bytes
        || stdout_capture.omitted() > 0
        || stderr_capture.omitted() > 0;
    if text.len() > max_output_bytes {
        let mut summary = OutputCapture::new(max_output_bytes);
        summary.push(&text);
        text = summary.render();
    }

    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "isError": exit_code != 0,
        "truncated": truncated,
        "exit_code": exit_code,
        "duration_ms": started.elapsed().as_millis(),
        "total_output_bytes": stdout_bytes.saturating_add(stderr_bytes),
        "output_encoding": output_encoding.as_str()
    }))
}

// Independently implemented from the head/tail retention design in openai/codex
// unified_exec/head_tail_buffer.rs (e18a20d6). Preserve final errors as well as startup output.
struct OutputCapture {
    head: String,
    tail: String,
    total: usize,
    limit: usize,
    head_done: bool,
}
impl OutputCapture {
    fn new(limit: usize) -> Self {
        Self {
            head: String::new(),
            tail: String::new(),
            total: 0,
            limit,
            head_done: false,
        }
    }
    fn push(&mut self, text: &str) {
        self.total = self.total.saturating_add(text.len());
        let mut remaining = text;
        if !self.head_done {
            let prefix =
                super::text::byte_prefix(text, (self.limit / 2).saturating_sub(self.head.len()));
            self.head.push_str(prefix);
            remaining = &text[prefix.len()..];
            self.head_done = !remaining.is_empty() || self.head.len() == self.limit / 2;
        }
        self.tail.push_str(remaining);
        let tail_budget = self.limit.saturating_sub(self.head.len());
        if self.tail.len() > tail_budget {
            let mut drop = self.tail.len() - tail_budget;
            while !self.tail.is_char_boundary(drop) {
                drop += 1;
            }
            self.tail.drain(..drop);
        }
    }
    fn omitted(&self) -> usize {
        self.total.saturating_sub(self.head.len() + self.tail.len())
    }
    fn render(&self) -> String {
        if self.omitted() == 0 {
            return format!("{}{}", self.head, self.tail);
        }
        format!(
            "{}\n... (output truncated: {} UTF-8 bytes omitted) ...\n{}",
            self.head,
            self.omitted(),
            self.tail
        )
    }
}

async fn capture(
    mut pipe: impl AsyncRead + Unpin,
    encoding: OutputEncoding,
    limit: usize,
) -> std::io::Result<OutputCapture> {
    let mut decoder = OutputDecoder::new(encoding);
    let mut output = OutputCapture::new(limit);
    let mut buffer = [0u8; 8192];
    loop {
        let count = pipe.read(&mut buffer).await?;
        output.push(&decoder.push(&buffer[..count], count == 0));
        if count == 0 {
            break;
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn drains_large_output_preserving_both_ends_without_splitting_unicode() {
        let input = format!("start{}final error", "中🙂".repeat(200_000));
        let output = capture(input.as_bytes(), OutputEncoding::Utf8, 1024)
            .await
            .unwrap();
        assert!(output.head.len() + output.tail.len() <= 1024);
        assert!(output.render().starts_with("start"));
        assert!(output.render().ends_with("final error"));
        assert_eq!(output.total, input.len());
        assert!(!output.render().contains('�'));
    }
    #[test]
    fn small_output_is_exact_and_tiny_budgets_do_not_break_unicode() {
        for limit in 1..20 {
            let mut output = OutputCapture::new(limit);
            for part in ["a", "中", "🙂", "last"] {
                output.push(part);
            }
            assert!(output.head.len() + output.tail.len() <= limit);
            assert!(!output.render().contains('�'));
            if limit >= "a中🙂last".len() {
                assert_eq!(output.render(), "a中🙂last");
            }
        }
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_call_cleans_up_its_process_tree() {
        let workspace =
            std::env::temp_dir().join(format!("portal-exec-cancel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let mut config = PortalConfig::default();
        config.security.workspace_root = workspace.clone();
        let manager = Arc::new(ProcessManager::new(
            crate::heart_callback::HeartCallback::new(),
        ));
        let task = tokio::spawn(async move {
            execute(&config, &manager, serde_json::json!({"command":"echo ready > ready.txt; (sleep 2; echo leaked > leaked.txt) & wait"})).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !workspace.join("ready.txt").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        let _ = task.await;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        assert!(!workspace.join("leaked.txt").exists());
        std::fs::remove_dir_all(workspace).unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_result_keeps_final_stderr_and_structured_exit_status() {
        let manager = Arc::new(ProcessManager::new(
            crate::heart_callback::HeartCallback::new(),
        ));
        let mut config = PortalConfig::default();
        config.security.workspace_root = std::env::temp_dir();
        let result = execute(&config, &manager, serde_json::json!({"command":"printf 'START'; i=0; while [ $i -lt 1000 ]; do printf 'noise'; i=$((i+1)); done; printf 'FINAL ERROR' >&2; exit 7", "max_output_bytes":200})).await.unwrap();
        let output = result["content"][0]["text"].as_str().unwrap();
        assert!(output.starts_with("START"));
        assert!(output.contains("FINAL ERROR"));
        assert_eq!(result["exit_code"], 7);
        assert_eq!(result["truncated"], true);
        assert_eq!(result["total_output_bytes"], 5016);
        assert!(output.len() < 400);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_terminates_descendants_before_they_can_modify_files() {
        let workspace =
            std::env::temp_dir().join(format!("portal-exec-timeout-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let mut config = PortalConfig::default();
        config.security.workspace_root = workspace.clone();
        let manager = Arc::new(ProcessManager::new(
            crate::heart_callback::HeartCallback::new(),
        ));
        let result = execute(&config, &manager, serde_json::json!({"command":"(sleep 2; echo leaked > leaked.txt) & wait", "timeout_secs":1})).await;
        assert!(result.unwrap_err().to_string().contains("timed out"));
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        assert!(!workspace.join("leaked.txt").exists());
        std::fs::remove_dir_all(workspace).unwrap();
    }
}
