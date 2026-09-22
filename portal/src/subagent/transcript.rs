//! Session events → readable transcript lines (PRD §4.4 / §4.6).
//!
//! Progress is *pulled*, never pushed: pi's `session_event` stream is rendered
//! to one line per event and appended to the same [`OutputBuffer`] ring that
//! backs `portal_exec --background`, so `portal_subagent_log` can reuse
//! `portal_process poll|log`'s offset/limit/poll contract verbatim. The being
//! already knows how to read it.
//!
//! ```text
//! [12:01:03] agent_start
//! [12:01:07] tool ▶ bash {"command":"cargo test"}
//! [12:01:19] tool ◀ bash ok
//! [12:01:24] assistant: Tests pass. Now editing src/lib.rs…
//! [12:03:01] agent_end
//! ```

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio::time;

use crate::process_manager::OutputBuffer;

use super::protocol::{assistant_text, SessionEvent, SessionEventBody};

/// Ring size per session — same 1 MiB budget as a background shell session.
pub const TRANSCRIPT_MAX_BYTES: usize = 1024 * 1024;
/// Head of an assistant message kept in the transcript.
const ASSISTANT_PREVIEW_BYTES: usize = 400;
/// Head of a tool argument blob kept in the transcript.
const TOOL_ARGS_PREVIEW_BYTES: usize = 160;
/// Long-poll cap, matching `portal_process`.
pub const MAX_POLL_TIMEOUT_MS: u64 = 300_000;

/// What a rendered event told us about the task's progress. The manager folds
/// this into `TaskState` so `portal_subagent_status` can show turns/last tool
/// without parsing the transcript back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventDigest {
    /// A turn completed.
    pub turn_completed: bool,
    /// A tool started; the name.
    pub tool_started: Option<String>,
    /// The agent finished a run (observability only — `prompt_and_wait` is the
    /// authoritative completion signal, PRD §3.3).
    pub agent_ended: bool,
}

/// A session's transcript ring plus the notify that wakes long pollers.
#[derive(Clone)]
pub struct Transcript {
    buffer: Arc<AsyncMutex<OutputBuffer>>,
    notify: Arc<Notify>,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

/// One page of transcript, shaped like `process_manager::LogResult`.
#[derive(Debug, Clone)]
pub struct TranscriptPage {
    pub output: Vec<u8>,
    pub next_offset: u64,
    pub truncated: bool,
    pub idle_s: u64,
    pub total_output_bytes: u64,
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            buffer: Arc::new(AsyncMutex::new(OutputBuffer::new(TRANSCRIPT_MAX_BYTES))),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Append a pre-rendered line (used for Portal's own annotations, e.g.
    /// "task cancelled by the being").
    pub async fn append_line(&self, line: &str) {
        let mut buf = self.buffer.lock().await;
        buf.append(line);
        buf.append("\n");
        drop(buf);
        self.notify.notify_waiters();
    }

    /// Render one event into the ring. Returns what it implied for task state;
    /// events with no useful rendering are dropped silently.
    pub async fn record(&self, event: &SessionEvent) -> EventDigest {
        let digest = digest(&event.event);
        if let Some(line) = render(&event.event) {
            self.append_line(&line).await;
        }
        digest
    }

    /// Bytes from `offset`, waiting up to `timeout_ms` for something new —
    /// the same semantics as `portal_process poll`.
    pub async fn page(
        &self,
        offset: u64,
        limit: usize,
        timeout_ms: u64,
    ) -> anyhow::Result<TranscriptPage> {
        let timeout_ms = timeout_ms.min(MAX_POLL_TIMEOUT_MS);
        let deadline = (timeout_ms > 0)
            .then(|| time::Instant::now() + Duration::from_millis(timeout_ms));

        loop {
            let page = {
                let buf = self.buffer.lock().await;
                let (output, next_offset) = buf.bytes_range(offset, limit)?;
                let start_offset = buf.total_written().saturating_sub(buf.data.len() as u64);
                TranscriptPage {
                    output,
                    next_offset,
                    truncated: offset < start_offset,
                    idle_s: buf.idle_s(),
                    total_output_bytes: buf.total_written(),
                }
            };

            if !page.output.is_empty() {
                return Ok(page);
            }
            let Some(deadline) = deadline else {
                return Ok(page);
            };
            if time::Instant::now() >= deadline {
                return Ok(page);
            }
            tokio::select! {
                _ = self.notify.notified() => {}
                _ = time::sleep_until(deadline) => {}
            }
        }
    }

    /// Seconds since the last event — "is it thinking or is it stuck?".
    pub async fn idle_s(&self) -> u64 {
        self.buffer.lock().await.idle_s()
    }

    pub async fn total_bytes(&self) -> u64 {
        self.buffer.lock().await.total_written()
    }
}

/// What the event means for task bookkeeping.
fn digest(event: &SessionEventBody) -> EventDigest {
    match event.kind.as_str() {
        "turn_end" => EventDigest {
            turn_completed: true,
            ..Default::default()
        },
        "tool_execution_start" => EventDigest {
            tool_started: Some(tool_name(event).to_string()),
            ..Default::default()
        },
        "agent_end" => EventDigest {
            agent_ended: true,
            ..Default::default()
        },
        _ => EventDigest::default(),
    }
}

/// One transcript line, or `None` for events not worth showing.
pub fn render(event: &SessionEventBody) -> Option<String> {
    let ts = clock();
    let body = match event.kind.as_str() {
        "agent_start" => "agent_start".to_string(),
        "agent_end" => "agent_end".to_string(),
        "tool_execution_start" => {
            let args = event
                .rest
                .get("args")
                .or_else(|| event.rest.get("arguments"))
                .or_else(|| event.rest.get("input"))
                .map(|v| clamp(&compact(v), TOOL_ARGS_PREVIEW_BYTES))
                .unwrap_or_default();
            format!("tool ▶ {} {}", tool_name(event), args).trim_end().to_string()
        }
        "tool_execution_end" => {
            let outcome = match event.rest.get("error") {
                Some(e) if !e.is_null() => format!("error: {}", clamp(&compact(e), 200)),
                _ => event
                    .str_field("status")
                    .unwrap_or("ok")
                    .to_string(),
            };
            let secs = event
                .f64_field("durationMs")
                .or_else(|| event.f64_field("duration_ms"))
                .map(|ms| format!(" ({:.1}s)", ms / 1000.0))
                .unwrap_or_default();
            format!("tool ◀ {} {}{}", tool_name(event), outcome, secs)
        }
        "message_end" => {
            let owned = event
                .str_field("text")
                .or_else(|| event.str_field("content"))
                .or_else(|| {
                    event
                        .rest
                        .get("message")
                        .and_then(|m| m.get("text"))
                        .and_then(|t| t.as_str())
                })
                .map(str::to_string)
                // pi's `--mode json` shape: `message.content` is a block array,
                // and the user's own echoed message is not worth a line.
                .or_else(|| event.rest.get("message").and_then(assistant_text))?;
            let text = owned.trim();
            if text.is_empty() {
                return None;
            }
            format!("assistant: {}", clamp(&one_line(text), ASSISTANT_PREVIEW_BYTES))
        }
        "turn_end" => {
            let tokens = event
                .u64_field("totalTokens")
                .or_else(|| event.u64_field("tokens"))
                .map(|t| format!(" ({t} tok)"))
                .unwrap_or_default();
            format!("turn_end{tokens}")
        }
        "compaction_start" => "compaction_start (auto)".to_string(),
        "compaction_end" => "compaction_end".to_string(),
        "auto_retry_start" => {
            let reason = event.str_field("reason").unwrap_or("unspecified");
            format!("auto_retry ({reason})")
        }
        "refine_complete" => "harness refined".to_string(),
        "refine_failed" => {
            let reason = event.str_field("error").unwrap_or("unspecified");
            format!("harness refine failed ({reason})")
        }
        "error" => {
            let msg = event
                .str_field("message")
                .or_else(|| event.str_field("error"))
                .unwrap_or("unspecified");
            format!("error: {}", clamp(msg, 400))
        }
        // Token deltas, stream chunks, UI noise: not worth a line.
        _ => return None,
    };
    Some(format!("[{ts}] {body}"))
}

fn tool_name(event: &SessionEventBody) -> &str {
    event
        .str_field("toolName")
        .or_else(|| event.str_field("tool_name"))
        .or_else(|| event.str_field("name"))
        .unwrap_or("tool")
}

/// `HH:MM:SS` in UTC. Deliberately not a date: the transcript is read live and
/// pulling in a calendar dependency for a log prefix is not worth it.
fn clock() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day = secs % 86_400;
    format!("{:02}:{:02}:{:02}", day / 3600, (day % 3600) / 60, day % 60)
}

fn compact(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clamp(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(v: serde_json::Value) -> SessionEventBody {
        serde_json::from_value(v).unwrap()
    }

    fn event(v: serde_json::Value) -> SessionEvent {
        SessionEvent {
            active_session_id: "as_1".to_string(),
            event: body(v),
            meta: None,
        }
    }

    /// Strip the `[HH:MM:SS] ` prefix so assertions stay time-independent.
    fn rendered(v: serde_json::Value) -> Option<String> {
        render(&body(v)).map(|line| {
            assert!(line.starts_with('['), "{line}");
            line[11..].to_string()
        })
    }

    #[test]
    fn renders_the_documented_event_shapes() {
        assert_eq!(rendered(json!({"type":"agent_start"})).unwrap(), "agent_start");
        assert_eq!(
            rendered(json!({"type":"tool_execution_start","toolName":"bash",
                            "args":{"command":"cargo test"}}))
            .unwrap(),
            r#"tool ▶ bash {"command":"cargo test"}"#
        );
        assert_eq!(
            rendered(json!({"type":"tool_execution_end","toolName":"bash","durationMs":12100}))
                .unwrap(),
            "tool ◀ bash ok (12.1s)"
        );
        assert_eq!(
            rendered(json!({"type":"message_end","text":"Tests pass.\n  Now editing src/lib.rs…"}))
                .unwrap(),
            "assistant: Tests pass. Now editing src/lib.rs…"
        );
        assert_eq!(
            rendered(json!({"type":"compaction_start"})).unwrap(),
            "compaction_start (auto)"
        );
        assert_eq!(rendered(json!({"type":"agent_end"})).unwrap(), "agent_end");
    }

    #[test]
    fn unknown_and_empty_events_render_nothing() {
        assert!(rendered(json!({"type":"token_delta","delta":"a"})).is_none());
        assert!(rendered(json!({"type":"message_end","text":"   "})).is_none());
        assert!(rendered(json!({"type":"message_end"})).is_none());
    }

    #[test]
    fn the_stdio_message_shape_renders_only_what_the_assistant_said() {
        // `pi --print --mode json` puts the text in a content block array…
        assert_eq!(
            rendered(json!({"type":"message_end","message":{"role":"assistant",
                            "content":[{"type":"thinking","thinking":"hmm"},
                                       {"type":"text","text":"Tests pass."}]}}))
            .unwrap(),
            "assistant: Tests pass."
        );
        // …and echoes the being's own prompt back as an event, which is not
        // progress and must not be shown as if the sub-agent said it.
        assert!(rendered(json!({"type":"message_end","message":{"role":"user",
                                "content":[{"type":"text","text":"# Task sub_1"}]}}))
        .is_none());
        // A turn that only called tools says nothing.
        assert!(rendered(json!({"type":"message_end","message":{"role":"assistant",
                                "content":[]}}))
        .is_none());
    }

    #[test]
    fn tool_errors_and_alternate_field_names_are_handled() {
        let line = rendered(json!({"type":"tool_execution_end","tool_name":"edit",
                                   "error":"file not found"}))
        .unwrap();
        assert_eq!(line, "tool ◀ edit error: file not found");
        let line = rendered(json!({"type":"error","message":"provider refused"})).unwrap();
        assert_eq!(line, "error: provider refused");
    }

    #[test]
    fn long_previews_are_clamped_on_char_boundaries() {
        let text = "é".repeat(1000);
        let line = rendered(json!({"type":"message_end","text": text})).unwrap();
        assert!(line.ends_with('…'));
        assert!(line.len() < ASSISTANT_PREVIEW_BYTES + 32);
        assert!(std::str::from_utf8(line.as_bytes()).is_ok());
    }

    #[test]
    fn digest_tracks_turns_and_tools() {
        assert_eq!(
            digest(&body(json!({"type":"turn_end"}))),
            EventDigest {
                turn_completed: true,
                ..Default::default()
            }
        );
        assert_eq!(
            digest(&body(json!({"type":"tool_execution_start","toolName":"bash"}))).tool_started,
            Some("bash".to_string())
        );
        assert!(digest(&body(json!({"type":"agent_end"}))).agent_ended);
        assert_eq!(digest(&body(json!({"type":"noise"}))), EventDigest::default());
    }

    #[tokio::test]
    async fn recorded_events_are_readable_by_offset() {
        let t = Transcript::new();
        t.record(&event(json!({"type":"agent_start"}))).await;
        t.record(&event(json!({"type":"token_delta"}))).await; // rendered as nothing
        t.record(&event(json!({"type":"agent_end"}))).await;

        let page = t.page(0, 64 * 1024, 0).await.unwrap();
        let text = String::from_utf8(page.output).unwrap();
        assert!(text.contains("agent_start"), "{text}");
        assert!(text.contains("agent_end"), "{text}");
        assert_eq!(text.lines().count(), 2, "unrenderable events add no lines");
        assert_eq!(page.next_offset, page.total_output_bytes);
        assert!(!page.truncated);

        // Reading from the tip returns nothing, without blocking.
        let tip = t.page(page.next_offset, 64 * 1024, 0).await.unwrap();
        assert!(tip.output.is_empty());
        assert_eq!(tip.next_offset, page.next_offset);
    }

    #[tokio::test]
    async fn page_long_polls_until_a_new_line_arrives() {
        let t = Transcript::new();
        let writer = t.clone();
        tokio::spawn(async move {
            time::sleep(Duration::from_millis(60)).await;
            writer.append_line("[00:00:00] agent_start").await;
        });

        let page = t.page(0, 4096, 5_000).await.unwrap();
        assert!(
            !page.output.is_empty(),
            "the poll must wake on the new line instead of timing out"
        );
    }

    #[tokio::test]
    async fn page_returns_immediately_with_zero_timeout() {
        let t = Transcript::new();
        let started = time::Instant::now();
        let page = t.page(0, 4096, 0).await.unwrap();
        assert!(page.output.is_empty());
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn ring_overflow_is_reported_as_truncated() {
        let t = Transcript::new();
        let chunk = "x".repeat(4096);
        for _ in 0..300 {
            t.append_line(&chunk).await;
        }
        assert!(t.total_bytes().await > TRANSCRIPT_MAX_BYTES as u64);
        let page = t.page(0, 64 * 1024, 0).await.unwrap();
        assert!(page.truncated, "dropped bytes must be visible to the being");
    }
}
