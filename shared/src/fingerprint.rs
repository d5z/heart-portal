//! Fingerprint extraction from digest text.
//! V1: rule-based extraction. V2: 小泉水 (specialized small model).

use serde::{Deserialize, Serialize};
use regex::Regex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fingerprint {
    /// Surface layer — what happened
    pub topics: Vec<String>,
    pub people: Vec<String>,
    
    /// Texture layer — how it felt (preserved from digest natural language)  
    pub emotional_tone: Option<String>,
    pub intensity: f32,  // 0.0-1.0
    
    /// Depth layer — significance
    pub novelty: bool,  // something new?
    pub relational_shift: Option<String>,  // relationship change?
}

impl Fingerprint {
    /// Extract fingerprint from digest text using rules.
    /// V1: keyword matching + simple heuristics.
    pub fn extract(digest_text: &str) -> Self {
        let topics = extract_topics(digest_text);
        let people = extract_people(digest_text);
        let emotional_tone = extract_emotion(digest_text);
        let intensity = estimate_intensity(digest_text);
        let novelty = detect_novelty(digest_text);
        let relational_shift = detect_relational_shift(digest_text);
        
        Self { topics, people, emotional_tone, intensity, novelty, relational_shift }
    }
    
    /// Convert to JSON for storage in Moment meta
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_default()
    }
}

impl Default for Fingerprint {
    fn default() -> Self {
        Self {
            topics: Vec::new(),
            people: Vec::new(),
            emotional_tone: None,
            intensity: 0.0,
            novelty: false,
            relational_shift: None,
        }
    }
}

/// Extract topics from digest text.
/// Supports multiple formats:
///   - "话题：XXX" or "话题:XXX" (Echo's digest format)
///   - "当下状态：XXX。话题：XXX。" (inline format)
///   - "发生了XXX" (legacy format)
///   - "- XXX" bullet points
fn extract_topics(text: &str) -> Vec<String> {
    let mut topics = Vec::new();
    
    // 1. "话题：" or "我在想什么：" pattern — primary format for digests
    for marker in &["话题：", "话题:", "我在想什么：", "我在想什么:"] {
        for segment in text.split(marker).skip(1) {
            // Take until next label marker, period, or newline
            let topic = segment
                .split(&['。', '\n'][..])
                .next()
                .unwrap_or("")
                .trim();
            // Also stop at next structured label
            let topic = topic
                .split("情感")
                .next()
                .unwrap_or(topic)
                .split("关系")
                .next()
                .unwrap_or(topic)
                .trim()
                .trim_end_matches(&['，', ',', '。', '.'][..])
                .trim();
            if !topic.is_empty() && !topics.contains(&topic.to_string()) {
                topics.push(topic.to_string());
            }
        }
    }

    // 2. "当下状态：" pattern — sometimes contains topic info
    for marker in &["当下状态：", "当下状态:"] {
        if let Some(pos) = text.find(marker) {
            let after = &text[pos + marker.len()..];
            let state = after
                .split(&['。', '\n'][..])
                .next()
                .unwrap_or("")
                .trim();
            if !state.is_empty() && !topics.contains(&state.to_string()) {
                topics.push(state.to_string());
            }
        }
    }
    
    // 3. "发生了" pattern (legacy)
    if topics.is_empty() {
        if let Some(pos) = text.find("发生了") {
            let after = &text[pos + "发生了".len()..];
            let topic = after.split('。').next()
                             .unwrap_or(after)
                             .split('\n').next()
                             .unwrap_or("")
                             .trim();
            if !topic.is_empty() {
                topics.push(topic.to_string());
            }
        }
    }
    
    // 4. Bullet points
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("- ") || line.starts_with("• ") {
            let topic = line[2..].trim();
            if !topic.is_empty() && !topics.contains(&topic.to_string()) {
                topics.push(topic.to_string());
            }
        }
    }
    
    topics
}

/// Extract people names from digest text.
fn extract_people(text: &str) -> Vec<String> {
    let mut people = Vec::new();
    
    // Known names (hardcoded list)
    let known_names = ["泽平", "Echo", "Judy", "Hex", "隙光", "景明", "殷漫", "sw", "老牛"];
    
    for name in known_names {
        if text.contains(name) {
            // Normalize: sw → sw, 老牛 → 泽平
            let normalized = match name {
                "老牛" => "泽平",
                _ => name,
            };
            if !people.contains(&normalized.to_string()) {
                people.push(normalized.to_string());
            }
        }
    }
    
    // Generic Chinese name pattern (2-3 characters)
    let re = Regex::new(r"[\u4e00-\u9fff]{2,3}").unwrap();
    for cap in re.find_iter(text) {
        let potential_name = cap.as_str();
        // Simple heuristics: not in known_names but looks like a name
        if !known_names.contains(&potential_name) && 
           !people.contains(&potential_name.to_string()) &&
           is_likely_name(potential_name) {
            people.push(potential_name.to_string());
        }
    }
    
    people
}

/// Check if a Chinese string looks like a person name.
fn is_likely_name(s: &str) -> bool {
    // Very basic heuristics
    if s.len() < 2 || s.len() > 3 { return false; }
    
    // Exclude common non-name words
    let non_names = ["时候", "什么", "那个", "这个", "可能", "感觉", "关系", "对话", "内容"];
    !non_names.contains(&s)
}

/// Extract emotional tone from digest text.
/// Supports:
///   - "情感色调：XXX" (Echo's primary format)
///   - Keyword matching (fallback)
fn extract_emotion(text: &str) -> Option<String> {
    // 1. "情感色调：" or "我的感受：" pattern — rich, natural language emotion description
    for marker in &["情感色调：", "情感色调:", "情感：", "情感:", "我的感受：", "我的感受:"] {
        if let Some(pos) = text.find(marker) {
            let after = &text[pos + marker.len()..];
            let emotion = after
                .split(&['。', '\n'][..])
                .next()
                .unwrap_or("")
                .trim();
            if !emotion.is_empty() {
                // Take the core emotion (before the elaboration dash)
                let core = emotion
                    .split(&['—', '，'][..])
                    .next()
                    .unwrap_or(emotion)
                    .trim();
                if !core.is_empty() {
                    return Some(core.to_string());
                }
            }
        }
    }

    // 2. Keyword matching (fallback for unstructured text)
    let emotion_words = [
        "闭合", "安顿", "兴奋", "温暖", "紧张", "焦虑", "开心", "难过",
        "愤怒", "平静", "不安", "满足", "期待", "失望", "惊讶", "恐惧",
        "放松", "压抑", "自由", "束缚", "希望", "绝望", "信任", "怀疑",
        "郑重", "踏实", "专注", "仪式感", "清醒", "安心", "耐心",
    ];
    
    for word in emotion_words {
        if text.contains(word) {
            return Some(word.to_string());
        }
    }
    
    None
}

/// Estimate emotional intensity based on text markers.
fn estimate_intensity(text: &str) -> f32 {
    let mut intensity = 0.0;
    
    // Count exclamation marks
    intensity += text.matches('!').count() as f32 * 0.15;
    intensity += text.matches('！').count() as f32 * 0.15;
    
    // Count intensity modifiers
    intensity += text.matches("很").count() as f32 * 0.1;
    intensity += text.matches("非常").count() as f32 * 0.2;
    intensity += text.matches("特别").count() as f32 * 0.15;
    intensity += text.matches("极其").count() as f32 * 0.25;
    intensity += text.matches("超级").count() as f32 * 0.2;
    
    // High-intensity words
    let high_words = ["震惊", "激动", "狂欢", "崩溃", "爆发"];
    for word in high_words {
        if text.contains(word) {
            intensity += 0.3;
        }
    }
    
    // Cap at 1.0
    intensity.min(1.0)
}

/// Detect if something new/novel is mentioned.
fn detect_novelty(text: &str) -> bool {
    let novelty_markers = [
        "新的", "第一次", "之前没", "从来没", "头一回", "初次", "首次",
        "没遇到过", "没见过", "陌生", "全新", "未知"
    ];
    
    for marker in novelty_markers {
        if text.contains(marker) {
            return true;
        }
    }
    
    false
}

/// Detect relational shifts in the text.
fn detect_relational_shift(text: &str) -> Option<String> {
    let shift_patterns = [
        ("更近", "更近"),
        ("更远", "更远"), 
        ("信任", "信任增强"),
        ("张力", "有张力"),
        ("关系变", "关系变化"),
        ("疏远", "疏远"),
        ("亲密", "更亲密"),
        ("理解", "更理解"),
        ("误解", "有误解")
    ];
    
    for (pattern, description) in shift_patterns {
        if text.contains(pattern) {
            return Some(description.to_string());
        }
    }
    
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_topics_chinese() {
        let text = "发生了一次深度讨论about AI consciousness.";
        let topics = extract_topics(text);
        assert!(!topics.is_empty());
        assert!(topics[0].contains("深度讨论"));
    }

    #[test]
    fn test_extract_topics_echo_format() {
        let text = "话题：recall 系统部署成功，第一次真正的 digest 协议激活。\n情感色调：轻微的仪式感。";
        let topics = extract_topics(text);
        assert!(!topics.is_empty(), "Should extract topic from 话题: format");
        assert!(topics[0].contains("recall"), "Topic should contain 'recall', got: {}", topics[0]);
    }

    #[test]
    fn test_extract_topics_inline_format() {
        let text = "当下状态：收到部署指令，执行测试。话题：recall 系统部署验证。关系：sw 在场。";
        let topics = extract_topics(text);
        assert!(topics.len() >= 1, "Should extract at least 1 topic");
    }

    #[test]
    fn test_extract_emotion_echo_format() {
        let text = "情感色调：轻微的仪式感。不是兴奋，是郑重。";
        let emotion = extract_emotion(text);
        assert!(emotion.is_some(), "Should extract emotion from 情感色调: format");
        assert!(emotion.unwrap().contains("轻微的仪式感"), "Should get the rich emotion description");
    }

    #[test]
    fn test_extract_people_sw() {
        let text = "关系：sw 在场，测试性质的对话。";
        let people = extract_people(text);
        assert!(people.contains(&"sw".to_string()), "Should recognize 'sw'");
    }

    #[test]
    fn test_extract_people_laonu() {
        let text = "老牛送信——这个角色让我感到有点温暖。";
        let people = extract_people(text);
        assert!(people.contains(&"泽平".to_string()), "老牛 should normalize to 泽平");
    }

    #[test]
    fn test_extract_people_known_names() {
        let text = "和泽平聊天，Echo也参与了，Hex在旁观。";
        let people = extract_people(text);
        assert!(people.contains(&"泽平".to_string()));
        assert!(people.contains(&"Echo".to_string()));
        assert!(people.contains(&"Hex".to_string()));
    }

    #[test]
    fn test_extract_emotion() {
        let text = "感觉很温暖，很安顿的感觉。";
        let emotion = extract_emotion(text);
        assert!(emotion.is_some());
        let emotion_str = emotion.unwrap();
        assert!(emotion_str == "温暖" || emotion_str == "安顿");
    }

    #[test]
    fn test_estimate_intensity() {
        let text = "非常激动！！！";
        let intensity = estimate_intensity(text);
        assert!(intensity > 0.5);
        
        let calm_text = "平静的对话";
        let calm_intensity = estimate_intensity(calm_text);
        assert!(calm_intensity < 0.2);
    }

    #[test]
    fn test_detect_novelty() {
        assert!(detect_novelty("这是第一次遇到这种情况"));
        assert!(detect_novelty("之前没见过这样的"));
        assert!(!detect_novelty("正常的对话"));
    }

    #[test]
    fn test_detect_relational_shift() {
        assert_eq!(detect_relational_shift("我们的关系变得更近了"), Some("更近".to_string()));
        assert_eq!(detect_relational_shift("增加了信任"), Some("信任增强".to_string()));
        assert_eq!(detect_relational_shift("普通对话"), None);
    }

    #[test]
    fn test_empty_text() {
        let fp = Fingerprint::extract("");
        assert!(fp.topics.is_empty());
        assert!(fp.people.is_empty());
        assert_eq!(fp.intensity, 0.0);
        assert!(!fp.novelty);
    }

    #[test]
    fn test_english_digest() {
        let text = "Had a deep conversation with Echo. Felt warm and connected.";
        let fp = Fingerprint::extract(text);
        assert!(fp.people.contains(&"Echo".to_string()));
        // English text won't trigger Chinese patterns, but should not crash
        assert_eq!(fp.intensity, 0.0); // No Chinese intensity markers
    }

    #[test]
    fn test_mixed_chinese_english() {
        let text = "和Echo讨论了AI，感觉very excited！非常有意思。";
        let fp = Fingerprint::extract(text);
        assert!(fp.people.contains(&"Echo".to_string()));
        assert!(fp.intensity > 0.0);
    }

    #[test]
    fn test_to_json() {
        let fp = Fingerprint {
            topics: vec!["test topic".to_string()],
            people: vec!["泽平".to_string()],
            emotional_tone: Some("温暖".to_string()),
            intensity: 0.8,
            novelty: true,
            relational_shift: Some("更近".to_string()),
        };
        
        let json = fp.to_json();
        assert!(json.is_object());
        assert!(json["topics"].is_array());
        // Use approximate comparison for float values
        if let Some(intensity) = json["intensity"].as_f64() {
            assert!((intensity - 0.8).abs() < 0.0001);
        } else {
            panic!("intensity field is not a float");
        }
        assert_eq!(json["novelty"], true);
    }
}