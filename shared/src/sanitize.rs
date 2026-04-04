//! Sanitize tool_call/tool_result pairing for LLM API calls.
//!
//! Fixes three problems that cause API 400 errors:
//! 1. Consecutive assistant messages with tool_calls → merge into one.
//! 2. Orphan tool_results (no matching tool_call) → drop.
//! 3. Orphan tool_calls (no matching tool_result) → drop.
//!
//! Works with any message type that implements `SanitizableMessage`.
//! Used by Core (ChatMessage) and Cortex/CronRunner (CronMessage).

use std::collections::HashSet;
use tracing::warn;

/// Trait for messages that can be sanitized for tool pairing.
pub trait SanitizableMessage: Clone {
    fn role(&self) -> &str;
    fn tool_call_ids(&self) -> Vec<String>;
    fn tool_result_id(&self) -> Option<String>;
    fn has_tool_calls(&self) -> bool;
    fn has_content(&self) -> bool;
    /// Merge another message's tool_calls into this one.
    fn merge_tool_calls(&mut self, other: &Self);
    /// Merge another message's text content into this one.
    fn merge_content(&mut self, other: &Self);
    /// Remove tool_calls by ID, returns remaining count.
    fn strip_tool_calls(&mut self, orphan_ids: &HashSet<String>) -> usize;
    /// Clear all tool_calls.
    fn clear_tool_calls(&mut self);
    /// Create a synthetic assistant message (for incomplete tool interactions).
    fn synthetic_assistant(text: &str) -> Self;
}

/// Sanitize a message sequence for correct tool_call/tool_result pairing.
pub fn sanitize<M: SanitizableMessage>(messages: Vec<M>) -> Vec<M> {
    if messages.is_empty() {
        return messages;
    }

    // Phase 1: Merge consecutive assistant messages with tool_calls.
    let mut merged: Vec<M> = Vec::with_capacity(messages.len());
    for msg in messages {
        if msg.role() == "assistant" && msg.has_tool_calls() {
            if let Some(last) = merged.last_mut() {
                if last.role() == "assistant" && last.has_tool_calls() {
                    last.merge_tool_calls(&msg);
                    last.merge_content(&msg);
                    continue;
                }
            }
        }
        merged.push(msg);
    }

    // Phase 2: Collect all IDs, find orphans.
    let mut call_ids: HashSet<String> = HashSet::new();
    let mut result_ids: HashSet<String> = HashSet::new();

    for msg in &merged {
        if msg.role() == "assistant" {
            call_ids.extend(msg.tool_call_ids());
        }
        if msg.role() == "tool" {
            if let Some(id) = msg.tool_result_id() {
                result_ids.insert(id);
            }
        }
    }

    let orphan_results: HashSet<String> = result_ids.difference(&call_ids).cloned().collect();
    let orphan_calls: HashSet<String> = call_ids.difference(&result_ids).cloned().collect();

    if !orphan_results.is_empty() {
        warn!("sanitize: dropping {} orphan tool_result(s)", orphan_results.len());
    }
    if !orphan_calls.is_empty() {
        warn!("sanitize: dropping {} orphan tool_call(s)", orphan_calls.len());
    }

    if orphan_results.is_empty() && orphan_calls.is_empty() {
        return phase4(merged);
    }

    // Phase 3: Filter orphans.
    let filtered: Vec<M> = merged.into_iter().filter_map(|mut msg| {
        // Drop orphan tool_results
        if msg.role() == "tool" {
            if let Some(id) = msg.tool_result_id() {
                if orphan_results.contains(&id) {
                    warn!(
                        "sanitize: dropping orphan tool_result (no matching tool_call); tool_call_id={}",
                        id
                    );
                    return None;
                }
            }
        }
        // Strip/drop orphan tool_calls from assistant messages
        if msg.role() == "assistant" && msg.has_tool_calls() {
            let remaining = msg.strip_tool_calls(&orphan_calls);
            if remaining == 0 {
                if msg.has_content() {
                    msg.clear_tool_calls();
                    return Some(msg);
                }
                return None;
            }
        }
        Some(msg)
    }).collect();

    phase4(filtered)
}

/// Phase 4: Ensure every tool_result is followed by an assistant message
/// before the next user message. Inject synthetic if missing.
fn phase4<M: SanitizableMessage>(messages: Vec<M>) -> Vec<M> {
    let mut result = Vec::with_capacity(messages.len());
    let mut need_synthetic_assistant = false;

    for msg in messages {
        if need_synthetic_assistant && msg.role() != "assistant" && msg.role() != "tool" {
            // Non-assistant, non-tool message after tool result → inject synthetic
            warn!("sanitize: injecting synthetic assistant after incomplete tool interaction");
            result.push(M::synthetic_assistant("[Tool result received. Response was interrupted.]"));
            need_synthetic_assistant = false;
        }
        if msg.role() == "assistant" {
            need_synthetic_assistant = false;
        }
        let is_tool = msg.role() == "tool";
        result.push(msg);
        if is_tool {
            need_synthetic_assistant = true;
        }
    }
    if need_synthetic_assistant {
        warn!("sanitize: injecting synthetic assistant after trailing tool result");
        result.push(M::synthetic_assistant("[Tool result received. Response was interrupted.]"));
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChatMessage;
    use serde_json::json;

    fn make_assistant_with_tool_use(tool_call_id: &str) -> ChatMessage {
        ChatMessage {
            role: "assistant".into(),
            content: Some("Let me check.".into()),
            name: None,
            tool_calls: Some(vec![json!({
                "id": tool_call_id,
                "type": "function",
                "function": { "name": "check_body", "arguments": "{}" }
            })]),
            tool_call_id: None,
            reasoning_content: None,
            reasoning_signature: None,
            images: None,
        }
    }

    #[test]
    fn test_sanitize_incomplete_tool_interaction() {
        // tool_use + tool_result + user (no assistant after tool) → inject synthetic
        let messages = vec![
            ChatMessage::user("hello"),
            make_assistant_with_tool_use("tc1"),
            ChatMessage::tool_result("tc1", "check_body", "ok"),
            ChatMessage::user("next question"),
        ];
        let result = sanitize(messages);
        assert_eq!(result.len(), 5);
        assert_eq!(result[0].role, "user");
        assert_eq!(result[1].role, "assistant");
        assert_eq!(result[2].role, "tool");
        assert_eq!(result[3].role, "assistant");
        assert!(result[3].content_str().contains("interrupted"));
        assert_eq!(result[4].role, "user");
    }

    #[test]
    fn test_sanitize_incomplete_at_end() {
        // tool_use + tool_result at end of sequence → inject synthetic
        let messages = vec![
            ChatMessage::user("hello"),
            make_assistant_with_tool_use("tc1"),
            ChatMessage::tool_result("tc1", "check_body", "ok"),
        ];
        let result = sanitize(messages);
        assert_eq!(result.len(), 4);
        assert_eq!(result[3].role, "assistant");
        assert!(result[3].content_str().contains("interrupted"));
    }

    #[test]
    fn test_sanitize_complete_tool_interaction_untouched() {
        // Complete interaction: tool_use + tool_result + assistant → no change
        let messages = vec![
            ChatMessage::user("hello"),
            make_assistant_with_tool_use("tc1"),
            ChatMessage::tool_result("tc1", "check_body", "ok"),
            ChatMessage::assistant("All good!"),
        ];
        let result = sanitize(messages);
        assert_eq!(result.len(), 4);
        assert_eq!(result[3].content_str(), "All good!");
    }

    #[test]
    fn test_sanitize_multiple_tool_rounds_one_incomplete() {
        // Two tool rounds, second one incomplete
        let messages = vec![
            ChatMessage::user("hello"),
            make_assistant_with_tool_use("tc1"),
            ChatMessage::tool_result("tc1", "check_body", "ok"),
            make_assistant_with_tool_use("tc2"),
            ChatMessage::tool_result("tc2", "search", "found"),
            ChatMessage::user("thanks"),
        ];
        let result = sanitize(messages);
        // Should inject synthetic after tc2's tool_result
        assert_eq!(result.len(), 7);
        assert_eq!(result[5].role, "assistant");
        assert!(result[5].content_str().contains("interrupted"));
        assert_eq!(result[6].role, "user");
    }
}
