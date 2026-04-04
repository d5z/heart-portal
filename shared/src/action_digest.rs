//! Action Digest — anomaly-driven causal distillation of action_log.
//!
//! Core insight: only intent ≠ feedback pairs carry cognitive value.
//! "You don't remember every step. You remember where you fell."
//!
//! Takes a round's tool_call/tool_result pairs from action_log,
//! sends them to an LLM for anomaly detection, and writes results
//! to action_digest table + pattern library.

use crate::{Moment, MomentKind};
use serde::{Deserialize, Serialize};

/// Maximum tokens per tool_result content before truncation.
const MAX_RESULT_TOKENS: usize = 500;

/// Maximum total tokens to send to LLM for a single round's digest.
const MAX_DIGEST_INPUT_TOKENS: usize = 20_000;

/// A paired atom: one tool_call + its matching tool_result.
#[derive(Debug, Clone, Serialize)]
pub struct ActionAtom {
    pub seq_call: u64,
    pub seq_result: u64,
    pub intent: String,       // tool_call content (LLM's thinking)
    pub feedback: String,     // tool_result content (reality's response)
    pub tool_name: Option<String>,
    pub exit_code: Option<i32>,
    pub tokens: u32,
}

/// Result of digesting one round's action_log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DigestResult {
    pub round_seq: u64,
    pub clean: bool,           // true if no anomalies detected
    pub anomalies: Vec<Anomaly>,
    pub chain: Option<String>, // causal chain description
    pub shape: Option<String>, // one-sentence collision shape
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Anomaly {
    pub seq_call: u64,
    pub seq_result: u64,
    pub shape: String,
}

/// Pair action_log entries into atoms.
pub fn pair_atoms(moments: &[Moment]) -> Vec<ActionAtom> {
    let mut atoms = Vec::new();
    let mut i = 0;
    while i < moments.len() {
        if moments[i].kind == MomentKind::ToolCall {
            // Look for the next tool_result
            if i + 1 < moments.len() && moments[i + 1].kind == MomentKind::ToolResult {
                let call = &moments[i];
                let result = &moments[i + 1];

                // Extract exit code from meta if available
                let exit_code = result.meta.as_ref()
                    .and_then(|m| m.get("exit_code"))
                    .and_then(|v| v.as_i64())
                    .map(|v| v as i32);

                // Extract tool name from meta
                let tool_name = call.meta.as_ref()
                    .and_then(|m| m.get("name"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                atoms.push(ActionAtom {
                    seq_call: call.seq.unwrap_or(0) as u64,
                    seq_result: result.seq.unwrap_or(0) as u64,
                    intent: call.content.clone(),
                    feedback: truncate_feedback(&result.content, MAX_RESULT_TOKENS),
                    tool_name,
                    exit_code,
                    tokens: call.tokens + result.tokens,
                });
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    atoms
}

/// Truncate feedback to max tokens (approximate: 4 chars per token).
/// Keeps head and tail for context.
fn truncate_feedback(content: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens * 4;
    if content.len() <= max_chars {
        return content.to_string();
    }
    let head_len = max_chars * 3 / 4;
    let tail_len = max_chars / 4;

    // Find safe UTF-8 boundaries
    let head_end = content.char_indices()
        .take_while(|(i, _)| *i < head_len)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let tail_start = content.char_indices()
        .rev()
        .take_while(|(i, _)| content.len() - *i < tail_len)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(content.len());

    format!("{}...[truncated]...{}", &content[..head_end], &content[tail_start..])
}

/// Build the LLM prompt for anomaly detection.
pub fn build_prompt(atoms: &[ActionAtom]) -> (String, String) {
    let system = r#"你是一个因果异常检测器。

下面是一组 tool_call/tool_result pairs，来自一个 being 的一轮行动。
每个 tool_call 是行动的意图，每个 tool_result 是现实的反馈。

你的任务：
1. 识别哪些 pairs 中"意图 ≠ 反馈"（现实和预期不匹配）
2. 用一句话描述每个异常的形状（不匹配的本质是什么）
3. 找出因果链——后一个意图是否被前一个反馈改变了
4. 用一句话总结整个 round 的碰壁形状（如果有的话）
5. 如果整个 round 都符合预期（无异常），只输出：CLEAN

忽略符合预期的 pairs。它们不产生新认知。

输出格式（严格遵守）：
ANOMALY: seq <call_seq>-<result_seq> | <一句话描述>
CHAIN: <seq> → <seq> → <seq>
SHAPE: <一句话碰壁形状>
或：
CLEAN"#.to_string();

    let mut user = String::new();
    let mut total_tokens = 0u32;
    for (i, atom) in atoms.iter().enumerate() {
        if total_tokens as usize > MAX_DIGEST_INPUT_TOKENS {
            user.push_str(&format!("\n[... {} more pairs truncated ...]\n", atoms.len() - i));
            break;
        }
        let exit_info = atom.exit_code
            .map(|c| if c != 0 { format!(" [EXIT {}]", c) } else { String::new() })
            .unwrap_or_default();
        user.push_str(&format!(
            "Pair {} (seq {}-{}):\ntool_call: {}\ntool_result: {}{}\n\n",
            i + 1, atom.seq_call, atom.seq_result,
            atom.intent, atom.feedback, exit_info
        ));
        total_tokens += atom.tokens;
    }

    (system, user)
}

/// Parse LLM response into DigestResult.
pub fn parse_response(response: &str, round_seq: u64) -> DigestResult {
    let trimmed = response.trim();

    // Check for CLEAN
    if trimmed == "CLEAN" || trimmed.starts_with("CLEAN") {
        return DigestResult {
            round_seq,
            clean: true,
            anomalies: Vec::new(),
            chain: None,
            shape: None,
        };
    }

    let mut anomalies = Vec::new();
    let mut chain = None;
    let mut shape = None;

    for line in trimmed.lines() {
        let line = line.trim();
        if line.starts_with("ANOMALY:") {
            // Parse: ANOMALY: seq 38-39 | description
            if let Some(rest) = line.strip_prefix("ANOMALY:") {
                let rest = rest.trim();
                if let Some(pipe_pos) = rest.find('|') {
                    let seq_part = rest[..pipe_pos].trim();
                    let desc = rest[pipe_pos + 1..].trim().to_string();

                    // Parse seq X-Y
                    let seq_str = seq_part.strip_prefix("seq ").unwrap_or(seq_part);
                    let parts: Vec<&str> = seq_str.split('-').collect();
                    if parts.len() == 2 {
                        if let (Ok(sc), Ok(sr)) = (parts[0].trim().parse::<u64>(), parts[1].trim().parse::<u64>()) {
                            anomalies.push(Anomaly {
                                seq_call: sc,
                                seq_result: sr,
                                shape: desc,
                            });
                        }
                    }
                }
            }
        } else if line.starts_with("CHAIN:") {
            chain = Some(line.strip_prefix("CHAIN:").unwrap_or(line).trim().to_string());
        } else if line.starts_with("SHAPE:") {
            shape = Some(line.strip_prefix("SHAPE:").unwrap_or(line).trim().to_string());
        }
    }

    DigestResult {
        round_seq,
        clean: anomalies.is_empty(),
        anomalies,
        chain,
        shape,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_clean() {
        let result = parse_response("CLEAN", 100);
        assert!(result.clean);
        assert!(result.anomalies.is_empty());
        assert!(result.shape.is_none());
    }

    #[test]
    fn test_parse_anomalies() {
        let response = r#"ANOMALY: seq 38-39 | 路径不完整，SQLite找不到stream表
ANOMALY: seq 42-43 | 路径修正后仍报错，文件无内容
CHAIN: 38 → 42 → 50 → 52
SHAPE: 根目录同名空壳文件形成影子陷阱"#;

        let result = parse_response(response, 7129);
        assert!(!result.clean);
        assert_eq!(result.anomalies.len(), 2);
        assert_eq!(result.anomalies[0].seq_call, 38);
        assert_eq!(result.anomalies[0].seq_result, 39);
        assert!(result.anomalies[0].shape.contains("路径"));
        assert!(result.chain.as_ref().unwrap().contains("38"));
        assert!(result.shape.as_ref().unwrap().contains("影子陷阱"));
    }

    #[test]
    fn test_truncate_feedback_short() {
        let short = "hello world";
        assert_eq!(truncate_feedback(short, 500), short);
    }

    #[test]
    fn test_truncate_feedback_long() {
        let long = "x".repeat(10000);
        let truncated = truncate_feedback(&long, 100);
        assert!(truncated.contains("[truncated]"));
        assert!(truncated.len() < 2000);
    }

    #[test]
    fn test_truncate_feedback_utf8_safe() {
        // Chinese characters: 3 bytes each
        let chinese = "你好世界".repeat(500);
        let truncated = truncate_feedback(&chinese, 100);
        // Should not panic on char boundary
        assert!(truncated.contains("[truncated]"));
    }

    #[test]
    fn test_pair_atoms() {
        let moments = vec![
            Moment::new(MomentKind::ToolCall, "check file"),
            Moment::new(MomentKind::ToolResult, "file not found"),
            Moment::new(MomentKind::ToolCall, "try other path"),
            Moment::new(MomentKind::ToolResult, "found it"),
        ];
        let atoms = pair_atoms(&moments);
        assert_eq!(atoms.len(), 2);
        assert_eq!(atoms[0].intent, "check file");
        assert_eq!(atoms[0].feedback, "file not found");
        assert_eq!(atoms[1].intent, "try other path");
        assert_eq!(atoms[1].feedback, "found it");
    }

    #[test]
    fn test_build_prompt() {
        let atoms = vec![ActionAtom {
            seq_call: 38,
            seq_result: 39,
            intent: "look at file".to_string(),
            feedback: "not found".to_string(),
            tool_name: Some("exec".to_string()),
            exit_code: Some(1),
            tokens: 50,
        }];
        let (system, user) = build_prompt(&atoms);
        assert!(system.contains("因果异常检测器"));
        assert!(user.contains("seq 38-39"));
        assert!(user.contains("[EXIT 1]"));
    }
}
