//! Exec tool — run shell commands.

use crate::config::PortalConfig;
use crate::exec_policy::{configure_shell_command, validate_exec_allowlist};
use crate::process_manager::ProcessManager;
use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;
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

    let workdir = arguments
        .get("workdir")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| config.security.workspace_root.to_string_lossy().to_string());

    let timeout_secs = arguments
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(30)
        .min(300);

    let background = arguments
        .get("background")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    validate_exec_allowlist(command, &config.security.exec_allowlist)?;

    if background {
        let info = process_manager
            .spawn(config, command, &workdir, &[])
            .await?;
        return Ok(serde_json::json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&serde_json::json!({
                    "session_id": info.session_id,
                    "pid": info.pid,
                    "status": "running"
                }))?
            }],
            "isError": false
        }));
    }

    debug!(
        "exec: {} (workdir: {}, timeout: {}s)",
        command, workdir, timeout_secs
    );

    let mut cmd = Command::new("sh");
    configure_shell_command(&mut cmd, command, config, &workdir);

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        cmd.output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Command timed out after {}s", timeout_secs))?
    .map_err(|e| anyhow::anyhow!("Failed to execute: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);

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
    const MAX_OUTPUT_CHARS: usize = 100_000;
    let truncated = text.len() > MAX_OUTPUT_CHARS;
    if truncated {
        text.truncate(MAX_OUTPUT_CHARS);
        text.push_str(&format!(
            "\n...\n(output truncated at {} chars)",
            MAX_OUTPUT_CHARS
        ));
    }

    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "isError": exit_code != 0,
        "truncated": truncated
    }))
}
