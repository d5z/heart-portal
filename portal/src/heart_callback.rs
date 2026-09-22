//! Shared Heart callback pipeline.
//!
//! One place for the "Portal finished something the being asked for, wake them
//! up" path: caps, retry policy, URL redaction and the HTTP client. Both
//! [`ProcessManager`](crate::process_manager::ProcessManager) (background
//! `portal_exec`) and [`SubagentManager`](crate::subagent::SubagentManager)
//! deliver through it so Heart sees one envelope shape and one retry story.

use crate::tools::text::OutputEncoding;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time;
use tracing::{debug, info, warn};

/// Tail of the output ring buffer attached to a callback payload.
/// 200KB leaves ~56KB of headroom for the other payload fields (cap 256KB).
pub const CALLBACK_OUTPUT_MAX_BYTES: usize = 200 * 1024;
/// Smaller tail used when the JSON-escaped payload still exceeds the cap.
pub const CALLBACK_OUTPUT_FALLBACK_BYTES: usize = 128 * 1024;
/// Hard cap on the serialized callback payload.
pub const CALLBACK_PAYLOAD_MAX_BYTES: usize = 256 * 1024;
/// The command/brief is being-supplied and otherwise unbounded; keep it from
/// eating the payload budget that the output tail is sized against.
pub const CALLBACK_COMMAND_MAX_BYTES: usize = 4096;
/// 1 initial attempt + 2 retries.
const CALLBACK_ATTEMPTS: usize = 3;
/// Backoff before retry N (index 0 = before the 2nd attempt).
const CALLBACK_BACKOFF: [Duration; CALLBACK_ATTEMPTS - 1] =
    [Duration::from_secs(2), Duration::from_secs(4)];
const CALLBACK_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const CALLBACK_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Where finished work is reported (Heart's `POST /api/callback`).
/// Present only in `--connect` mode; `None` means standalone (no callback).
#[derive(Clone)]
pub struct CallbackConfig {
    pub url: String,
    pub token: String,
    pub portal_name: String,
    pub client: reqwest::Client,
}

/// Shared, cloneable handle to the callback configuration.
///
/// Constructed once in `main`/`ToolHost` and handed to every manager that can
/// finish work asynchronously. `set()` is called after the Loom link is parsed;
/// before that every `deliver_detached` is a no-op (standalone mode).
#[derive(Clone, Default)]
pub struct HeartCallback {
    config: Arc<Mutex<Option<CallbackConfig>>>,
}

impl HeartCallback {
    pub fn new() -> Self {
        Self {
            config: Arc::new(Mutex::new(None)),
        }
    }

    /// Enable async callbacks: finished work POSTs its result to `url`.
    /// Called from `--connect` mode before the relay handshake. Without it,
    /// tasks finish silently.
    pub fn set(&self, url: String, token: String, portal_name: String) {
        let client = match reqwest::Client::builder()
            .timeout(CALLBACK_HTTP_TIMEOUT)
            .connect_timeout(CALLBACK_CONNECT_TIMEOUT)
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                warn!("failed to build callback HTTP client: {e}; async callbacks disabled");
                return;
            }
        };
        info!("async callback enabled → {}", redact_url(&url));
        match self.config.lock() {
            Ok(mut guard) => {
                *guard = Some(CallbackConfig {
                    url,
                    token,
                    portal_name,
                    client,
                });
            }
            Err(e) => warn!("callback config lock poisoned: {e}; async callbacks disabled"),
        }
    }

    pub fn config(&self) -> Option<CallbackConfig> {
        self.config.lock().ok().and_then(|g| g.clone())
    }

    /// Portal name as reported to Heart; `None` until `set()` has run.
    pub fn portal_name(&self) -> Option<String> {
        self.config().map(|c| c.portal_name)
    }

    pub fn is_configured(&self) -> bool {
        self.config().is_some()
    }

    /// Fire-and-forget delivery. Returns immediately; a lost callback is a
    /// WARN, never a Portal failure. No-op when unconfigured.
    pub fn deliver_detached(&self, task_id: String, payload: Value) {
        let Some(cfg) = self.config() else {
            debug!("no callback configured; dropping result for {task_id}");
            return;
        };
        tokio::spawn(async move {
            deliver_callback(cfg, task_id, payload).await;
        });
    }
}

/// Last `max` bytes of `data` (tail — the interesting end of a build/test log).
pub fn tail(data: &[u8], max: usize) -> &[u8] {
    let mut start = data.len().saturating_sub(max);
    while start < data.len() && data[start] & 0xc0 == 0x80 {
        start += 1;
    }
    &data[start..]
}

/// Head of `s` capped at `max` bytes, never splitting a UTF-8 char.
pub fn clamp_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &s[..end])
}

/// Tail of `s` capped at `max` bytes, never splitting a UTF-8 char.
/// Used for partial results, where the end is the interesting part.
pub fn clamp_str_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…[truncated]{}", &s[start..])
}

/// Build a payload that fits the wire cap: try the full 200KB output budget,
/// fall back to 128KB when JSON escaping blows past [`CALLBACK_PAYLOAD_MAX_BYTES`].
pub fn fit_payload(build: impl Fn(usize) -> Value) -> Value {
    let payload = build(CALLBACK_OUTPUT_MAX_BYTES);
    let size = serde_json::to_vec(&payload)
        .map(|b| b.len())
        .unwrap_or(usize::MAX);
    if size <= CALLBACK_PAYLOAD_MAX_BYTES {
        return payload;
    }
    build(CALLBACK_OUTPUT_FALLBACK_BYTES)
}

/// Retry 5xx (Heart restarting / proxy hiccup); never retry 4xx (401, 413, …).
pub fn should_retry_status(status: u16) -> bool {
    status >= 500
}

/// Retry transport failures that a later attempt may survive.
pub fn should_retry_error(err: &reqwest::Error) -> bool {
    err.is_connect() || err.is_timeout()
}

/// Strip the query string so a token can never reach the logs.
pub fn redact_url(url: &str) -> String {
    match url.split_once('?') {
        Some((base, _)) => format!("{base}?<redacted>"),
        None => url.to_string(),
    }
}

/// POST the payload, retrying per PRD §3.
/// Never returns an error: a lost callback is a WARN, not a Portal failure
/// (the being can still poll).
pub async fn deliver_callback(cfg: CallbackConfig, task_id: String, payload: Value) {
    let mut last_err = String::from("no attempt made");
    for attempt in 0..CALLBACK_ATTEMPTS {
        if attempt > 0 {
            time::sleep(CALLBACK_BACKOFF[attempt - 1]).await;
        }
        // Heart checks `?token=` query param, not Authorization header.
        let url_with_token = format!("{}?token={}", cfg.url, cfg.token);
        match cfg
            .client
            .post(&url_with_token)
            .json(&payload)
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    debug!("callback delivered for task {task_id} ({status})");
                    return;
                }
                last_err = format!("HTTP {status}");
                if !should_retry_status(status.as_u16()) {
                    break;
                }
            }
            Err(e) => {
                let retry = should_retry_error(&e);
                last_err = e.without_url().to_string();
                if !retry {
                    break;
                }
            }
        }
    }
    warn!(
        "callback delivery failed for task {} to {}: {}",
        task_id,
        redact_url(&cfg.url),
        last_err
    );
}

// ── portal_exec (background) payload ────────────────────────────────

/// A finished background session, as reported to Heart.
#[derive(Clone, Debug)]
pub struct CallbackTask {
    pub session_id: String,
    pub command: String,
    pub workdir: String,
    pub exit_code: i32,
    pub elapsed_secs: u64,
    pub output_encoding: OutputEncoding,
}

pub(crate) fn payload_with_tail(
    task: &CallbackTask,
    portal_name: &str,
    data: &[u8],
    total_output_bytes: u64,
    max_output: usize,
) -> Value {
    let slice = tail(data, max_output);
    let truncated = (slice.len() as u64) < total_output_bytes;
    let command = clamp_str(&task.command, CALLBACK_COMMAND_MAX_BYTES);
    serde_json::json!({
        "source": "portal",
        "task_id": task.session_id,
        "summary": format!(
            "portal_exec completed: '{}' (exit {})",
            command, task.exit_code
        ),
        "result": {
            "session_id": task.session_id,
            "exit_code": task.exit_code,
            "output": String::from_utf8_lossy(slice),
            "output_encoding": task.output_encoding.as_str(),
            "command": command,
            "workdir": task.workdir,
            "elapsed_secs": task.elapsed_secs,
            "portal_name": portal_name,
            "truncated": truncated,
            "total_output_bytes": total_output_bytes,
        }
    })
}

/// Build the `POST /api/callback` body. Output is the *tail* of the ring buffer;
/// if JSON escaping still blows past the payload cap, fall back to a shorter tail.
pub fn build_callback_payload(
    task: &CallbackTask,
    portal_name: &str,
    data: &[u8],
    total_output_bytes: u64,
) -> Value {
    fit_payload(|max_output| payload_with_tail(task, portal_name, data, total_output_bytes, max_output))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_task() -> CallbackTask {
        CallbackTask {
            session_id: "sess_abc".to_string(),
            command: "make test".to_string(),
            workdir: "/home/alice/project".to_string(),
            exit_code: 0,
            elapsed_secs: 42,
            output_encoding: OutputEncoding::Utf8,
        }
    }

    #[test]
    fn callback_payload_has_prd_shape() {
        let task = sample_task();
        let v = build_callback_payload(&task, "alice-laptop", b"hello", 5);

        assert_eq!(v["source"], "portal");
        assert_eq!(v["task_id"], "sess_abc");
        assert_eq!(v["summary"], "portal_exec completed: 'make test' (exit 0)");
        assert_eq!(v["result"]["session_id"], "sess_abc");
        assert_eq!(v["result"]["exit_code"], 0);
        assert_eq!(v["result"]["output"], "hello");
        assert_eq!(v["result"]["output_encoding"], "utf8");
        assert_eq!(v["result"]["command"], "make test");
        assert_eq!(v["result"]["workdir"], "/home/alice/project");
        assert_eq!(v["result"]["elapsed_secs"], 42);
        assert_eq!(v["result"]["portal_name"], "alice-laptop");
        assert_eq!(v["result"]["truncated"], false);
        assert_eq!(v["result"]["total_output_bytes"], 5);
    }

    #[test]
    fn callback_payload_takes_tail_and_marks_truncated() {
        let task = sample_task();
        let mut data = vec![b'a'; CALLBACK_OUTPUT_MAX_BYTES];
        data.extend_from_slice(b"THE_END");
        let total = data.len() as u64;

        let v = build_callback_payload(&task, "p", &data, total);
        let out = v["result"]["output"].as_str().unwrap();

        assert_eq!(out.len(), CALLBACK_OUTPUT_MAX_BYTES);
        assert!(out.ends_with("THE_END"), "tail must be kept, not the head");
        assert_eq!(v["result"]["truncated"], true);
        assert_eq!(v["result"]["total_output_bytes"], total);
    }

    #[test]
    fn callback_payload_truncated_when_ring_dropped_bytes() {
        let task = sample_task();
        // Ring buffer holds 5 bytes but 1000 were written overall.
        let v = build_callback_payload(&task, "p", b"tail!", 1000);
        assert_eq!(v["result"]["truncated"], true);
        assert_eq!(v["result"]["total_output_bytes"], 1000);
    }

    #[test]
    fn callback_payload_falls_back_when_escaping_blows_the_cap() {
        let task = sample_task();
        // Every byte escapes to 6 chars (\u00XX) — 200KB tail would be ~1.2MB.
        let data = vec![0x01u8; CALLBACK_OUTPUT_MAX_BYTES + 10];
        let v = build_callback_payload(&task, "p", &data, data.len() as u64);
        let out = v["result"]["output"].as_str().unwrap();
        assert_eq!(out.len(), CALLBACK_OUTPUT_FALLBACK_BYTES);
    }

    #[test]
    fn callback_payload_clamps_a_huge_command() {
        let mut task = sample_task();
        task.command = "é".repeat(10_000); // multi-byte: must not split a char
        let v = build_callback_payload(&task, "p", b"", 0);
        let cmd = v["result"]["command"].as_str().unwrap();
        assert!(cmd.len() < CALLBACK_COMMAND_MAX_BYTES + 32);
        assert!(cmd.ends_with("…[truncated]"));
        assert!(serde_json::to_vec(&v).unwrap().len() <= CALLBACK_PAYLOAD_MAX_BYTES);
    }

    #[test]
    fn clamp_str_tail_keeps_the_end_on_char_boundary() {
        let s = "é".repeat(100); // 200 bytes
        let out = clamp_str_tail(&s, 51);
        assert!(out.starts_with("…[truncated]"));
        assert!(out.len() <= "…[truncated]".len() + 51);
        assert!(out.ends_with('é'));
        assert_eq!(clamp_str_tail("short", 100), "short");
    }

    #[test]
    fn retry_only_on_5xx() {
        assert!(should_retry_status(500));
        assert!(should_retry_status(503));
        assert!(!should_retry_status(200));
        assert!(!should_retry_status(401));
        assert!(!should_retry_status(413));
        assert!(!should_retry_status(404));
    }

    #[test]
    fn redact_url_strips_query() {
        assert_eq!(
            redact_url("https://echo.beings.town/alice/api/callback?token=secret"),
            "https://echo.beings.town/alice/api/callback?<redacted>"
        );
        assert_eq!(
            redact_url("https://echo.beings.town/alice/api/callback"),
            "https://echo.beings.town/alice/api/callback"
        );
    }

    #[test]
    fn unconfigured_callback_is_a_noop() {
        let cb = HeartCallback::new();
        assert!(!cb.is_configured());
        assert!(cb.config().is_none());
        assert!(cb.portal_name().is_none());
    }
}
