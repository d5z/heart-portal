//! Task-local execution records for the desktop sidebar, independent of sessions.
use std::{io::Write, path::Path, time::{SystemTime, UNIX_EPOCH}};
use serde_json::{json, Value};
use super::protocol::{assistant_text, SessionEventBody, Usage};

pub fn append(file: &Path, mut value: Value) {
    value["at"] = json!(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64);
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
    if let Ok(mut out) = options.open(file) { let _ = writeln!(out, "{value}"); }
}

fn text(value: &Value) -> String {
    if let Some(content) = value.get("content").and_then(Value::as_array) {
        content.iter().filter_map(|block| block.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n")
    } else if let Some(value) = value.as_str() { value.to_string() }
    else { serde_json::to_string_pretty(value).unwrap_or_default() }
}

pub fn record(file: &Path, event: &SessionEventBody) {
    let value = match event.kind.as_str() {
        "agent_start" | "turn_start" => json!({"kind":"thinking"}),
        "tool_execution_start" => json!({"kind":"tool", "callId":event.str_field("toolCallId"),
            "name":event.str_field("toolName").unwrap_or("tool"),
            "input":event.rest.get("args").or_else(|| event.rest.get("arguments")).map(text).unwrap_or_default()}),
        "tool_execution_end" => json!({"kind":"tool-result", "callId":event.str_field("toolCallId"),
            "name":event.str_field("toolName").unwrap_or("tool"),
            "failed":event.rest.get("isError").and_then(Value::as_bool).unwrap_or(false) || event.rest.get("error").is_some_and(|v| !v.is_null()),
            "duration_ms":event.rest.get("result").and_then(|result| result.pointer("/details/duration_ms")),
            "total_output_bytes":event.rest.get("result").and_then(|result| result.pointer("/details/total_output_bytes")),
            "truncated":event.rest.get("result").and_then(|result| result.pointer("/details/truncated")),
            "output":event.rest.get("result").or_else(|| event.rest.get("error")).map(text).unwrap_or_default()}),
        "message_update" => {
            let Some(update) = event.rest.get("assistantMessageEvent") else { return };
            if update.get("type").and_then(Value::as_str) != Some("text_delta") { return }
            let Some(delta) = update.get("delta").and_then(Value::as_str) else { return };
            json!({"kind":"message-delta", "output":delta})
        },
        "message_end" => {
            let Some(message) = event.rest.get("message").filter(|message| message["role"] == "assistant") else { return };
            let usage = message.get("usage")
                .map(Usage::from_value)
                .map(|usage| json!({
                    "input": usage.input,
                    "output": usage.output,
                    "cache_read": usage.cache_read,
                    "cache_write": usage.cache_write,
                    "total": usage.total,
                    "cost_usd": usage.cost_usd,
                }));
            let stop_reason = message.get("stopReason").or_else(|| message.get("stop_reason"))
                .and_then(Value::as_str);
            let mut value = match assistant_text(message) {
                Some(text) => json!({"kind":"message", "output":text}),
                None if usage.is_some() || stop_reason.is_some() => json!({"kind":"usage"}),
                None => return,
            };
            if let Some(usage) = usage { value["usage"] = usage; }
            if let Some(stop_reason) = stop_reason { value["stop_reason"] = json!(stop_reason); }
            value
        },
        "turn_end" => json!({"kind":"turn-end"}),
        "compaction_start" | "compaction_end" => json!({
            "kind":event.kind.replace('_', "-"),
            "reason":event.str_field("reason"),
            "tokens_before":event.rest.get("result").and_then(|result| result.get("tokensBefore")),
            "aborted":event.rest.get("aborted"),
        }),
        _ => return,
    };
    append(file, value);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn records_visible_text_deltas_without_private_thinking() {
        let file = std::env::temp_dir().join(format!("subagent-stream-{}.jsonl", uuid::Uuid::new_v4()));
        for event in [
            json!({"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","delta":"private thought"}}),
            json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"Hello "}}),
            json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"world"}}),
            json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"Hello world"}]}}),
        ] { record(&file, &serde_json::from_value(event).unwrap()); }
        let output = std::fs::read_to_string(&file).unwrap();
        let rows: Vec<Value> = output.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["kind"], "message-delta");
        assert_eq!(rows[1]["output"], "world");
        assert_eq!(rows[2]["kind"], "message");
        assert!(!output.contains("private thought"));
        let _ = std::fs::remove_file(file);
    }
    #[test]
    fn records_tool_steps_and_results_without_private_thinking() {
        let file = std::env::temp_dir().join(format!("subagent-activity-{}.jsonl", uuid::Uuid::new_v4()));
        for event in [
            json!({"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","delta":"private thought"}}),
            json!({"type":"tool_execution_start","toolName":"bash","toolCallId":"a","args":{"command":"python3 -V"}}),
            json!({"type":"tool_execution_end","toolName":"bash","toolCallId":"a","isError":false,"result":{"content":[{"type":"text","text":"Python 3.12"}],"details":{"duration_ms":12,"total_output_bytes":11,"truncated":false}}}),
            json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"private thought"},{"type":"text","text":"All done"}],"usage":{"input":10,"output":4,"cacheRead":2},"stopReason":"stop"}}),
        ] { record(&file, &serde_json::from_value(event).unwrap()); }
        let output = std::fs::read_to_string(&file).unwrap();
        assert_eq!(output.lines().count(), 3);
        assert!(output.contains("python3 -V") && output.contains("Python 3.12") && output.contains("All done"));
        assert!(output.contains("\"cache_read\":2") && output.contains("\"stop_reason\":\"stop\""));
        assert!(output.contains("\"duration_ms\":12") && output.contains("\"total_output_bytes\":11"));
        assert!(!output.contains("private thought"));
        let long = "文".repeat(70000);
        record(&file, &serde_json::from_value(json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":long}]}})).unwrap());
        let updated = std::fs::read_to_string(&file).unwrap();
        let last: Value = serde_json::from_str(updated.lines().last().unwrap()).unwrap();
        assert_eq!(last["output"].as_str().unwrap(), long);
        for event in [
            json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"toolCall","name":"portal_file_read"}],"usage":{"input":10,"output":4,"cacheRead":2},"stopReason":"toolUse"}}),
            json!({"type":"compaction_start","reason":"threshold"}),
            json!({"type":"compaction_end","reason":"threshold","aborted":false,"result":{"tokensBefore":123,"summary":"private summary"}}),
        ] { record(&file, &serde_json::from_value(event).unwrap()); }
        let updated = std::fs::read_to_string(&file).unwrap();
        let entries: Vec<Value> = updated.lines().skip(4).map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(entries[0]["kind"], "usage");
        assert_eq!(entries[0]["usage"]["cache_read"], 2);
        assert_eq!(entries[1]["kind"], "compaction-start");
        assert_eq!(entries[2]["kind"], "compaction-end");
        assert_eq!(entries[2]["tokens_before"], 123);
        assert!(!updated.contains("private summary"));
        let _ = std::fs::remove_file(file);
    }
}
