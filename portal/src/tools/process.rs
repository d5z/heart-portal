//! MCP tool `portal_process` — list / poll / log / write / kill for background exec sessions.

use crate::process_manager::{ProcessManager, ProcessStatus, SessionInfo};
use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

fn status_json(st: &ProcessStatus) -> Value {
    match st {
        ProcessStatus::Running => serde_json::json!({ "kind": "running" }),
        ProcessStatus::Exited(code) => serde_json::json!({ "kind": "exited", "code": code }),
    }
}

fn session_row(s: SessionInfo) -> Value {
    serde_json::json!({
        "session_id": s.session_id,
        "pid": s.pid,
        "command": s.command,
        "status": status_json(&s.status),
        "uptime_s": s.uptime_s,
        "idle_s": s.idle_s,
        "total_output_bytes": s.total_output_bytes,
    })
}

pub async fn handle(process_manager: &Arc<ProcessManager>, arguments: Value) -> Result<Value> {
    let action = arguments
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'action'"))?;

    match action {
        "list" => {
            let sessions = process_manager.list().await;
            let rows: Vec<Value> = sessions.into_iter().map(session_row).collect();
            Ok(serde_json::json!({
                "content": [{
                    "type": "text",
                    "text": serde_json::to_string_pretty(&serde_json::json!({ "sessions": rows }))?
                }],
                "isError": false
            }))
        }
        "poll" => {
            let session_id = arguments
                .get("session_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("Missing 'session_id' for poll"))?;
            let offset = arguments.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
            let timeout_ms = arguments
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(5000);
            let r = process_manager
                .poll(session_id, offset, timeout_ms)
                .await?;
            let text = String::from_utf8_lossy(&r.output);
            Ok(serde_json::json!({
                "content": [{
                    "type": "text",
                    "text": serde_json::to_string(&serde_json::json!({
                        "output": text,
                        "next_offset": r.next_offset,
                        "truncated": r.truncated,
                        "status": status_json(&r.status),
                        "idle_s": r.idle_s,
                        "total_output_bytes": r.total_output_bytes,
                    }))?
                }],
                "isError": false
            }))
        }
        "log" => {
            let session_id = arguments
                .get("session_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("Missing 'session_id' for log"))?;
            let offset = arguments.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
            let limit = arguments.get("limit").and_then(|v| v.as_u64()).unwrap_or(64 * 1024);
            let r = process_manager.log(session_id, offset, limit).await?;
            let text = String::from_utf8_lossy(&r.output);
            Ok(serde_json::json!({
                "content": [{
                    "type": "text",
                    "text": serde_json::to_string(&serde_json::json!({
                        "output": text,
                        "next_offset": r.next_offset,
                        "truncated": r.truncated,
                        "status": status_json(&r.status),
                        "idle_s": r.idle_s,
                        "total_output_bytes": r.total_output_bytes,
                    }))?
                }],
                "isError": false
            }))
        }
        "write" => {
            let session_id = arguments
                .get("session_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("Missing 'session_id' for write"))?;
            let data = arguments
                .get("data")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("Missing 'data' for write"))?;
            process_manager
                .write_stdin(session_id, data.as_bytes())
                .await?;
            Ok(serde_json::json!({
                "content": [{ "type": "text", "text": "ok" }],
                "isError": false
            }))
        }
        "kill" => {
            let session_id = arguments
                .get("session_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("Missing 'session_id' for kill"))?;
            process_manager.kill(session_id).await?;
            Ok(serde_json::json!({
                "content": [{ "type": "text", "text": "ok" }],
                "isError": false
            }))
        }
        _ => anyhow::bail!("Unknown action: {}", action),
    }
}
