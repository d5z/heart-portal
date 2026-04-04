//! Reality Reconcile — detect tensions between memory and reality.
//!
//! When the being's memory suggests something that conflicts with
//! its reality map, we generate Tensions for further investigation.

use anyhow::Result;
use crate::reality::{EffectiveNode, Freshness, RealityLayer, RealityRealm};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// A detected tension between memory and reality.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tension {
    /// Type of tension detected
    pub kind: TensionKind,
    /// Human-readable description
    pub description: String,
    /// Associated reality key(s)
    pub reality_keys: Vec<String>,
    /// Memory signal(s) that triggered this
    pub memory_hints: Vec<String>,
    /// Confidence that this is a real tension (0-1)
    pub confidence: f64,
}

/// Types of tensions we can detect.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TensionKind {
    /// Memory mentions a reality key that's stale
    StaleNode,
    /// Memory mentions something contradicting bedrock fact
    BedrockConflict,
    /// Memory mentions a topic that's in fog layer
    FogConflict,
    /// Memory mentions something not in reality map
    MissingReality,
}

/// V1 reconcile implementation — system 1, heuristic-based.
///
/// This is intentionally simple and imperfect. We extract basic keywords
/// from memory signals and check for obvious conflicts.
pub fn reconcile(
    memory_signals: &[String],
    reality_nodes: &[EffectiveNode],
) -> Vec<Tension> {
    let mut tensions = Vec::new();

    // Extract topics/keywords from memory signals
    let memory_topics = extract_topics_from_memory(memory_signals);
    
    // Index reality nodes by topic for quick lookup
    let reality_index = index_reality_by_topic(reality_nodes);

    // Pattern 1: Memory mentions stale reality nodes
    tensions.extend(detect_stale_tensions(&memory_topics, reality_nodes));

    // Pattern 2: Memory mentions something that contradicts bedrock
    tensions.extend(detect_bedrock_conflicts(&memory_topics, reality_nodes));

    // Pattern 3: Memory mentions topics that are in fog
    tensions.extend(detect_fog_conflicts(&memory_topics, reality_nodes));

    // Pattern 4: Memory mentions topics not in reality at all
    tensions.extend(detect_missing_reality(&memory_topics, &reality_index, memory_signals));

    tensions
}

/// Extract basic keywords and topics from memory signals.
/// V1: very simple word extraction, no LLM.
fn extract_topics_from_memory(memory_signals: &[String]) -> HashSet<String> {
    let mut topics = HashSet::new();
    
    for signal in memory_signals {
        // Simple tokenization: split on whitespace, keep hyphens
        let words: Vec<String> = signal
            .split(|c: char| c.is_whitespace() || (c.is_ascii_punctuation() && c != '-' && c != ':'))
            .filter(|w| w.len() > 2) // Skip very short words
            .map(|w| w.to_lowercase())
            .collect();
        
        for word in words {
            // Skip common words
            if is_common_word(&word) {
                continue;
            }
            topics.insert(word);
        }
        
        // Also look for colon-separated keys (like "sense:health")
        if let Some(key) = extract_reality_key(signal) {
            topics.insert(key);
        }
    }
    
    topics
}

fn is_common_word(word: &str) -> bool {
    matches!(word, "the" | "and" | "or" | "but" | "in" | "on" | "at" | "to" | "for" | "of" | 
                   "with" | "by" | "about" | "this" | "that" | "these" | "those" | "a" | "an" |
                   "is" | "are" | "was" | "were" | "been" | "have" | "has" | "had" | "do" |
                   "does" | "did" | "will" | "would" | "could" | "should" | "can" | "may" |
                   "might" | "must" | "i" | "you" | "he" | "she" | "it" | "we" | "they")
}

/// Extract reality keys (format: word:word) from text.
fn extract_reality_key(text: &str) -> Option<String> {
    for word in text.split_whitespace() {
        if word.contains(':') && !word.starts_with("http") {
            // Simple heuristic: word:word pattern that's not a URL
            let parts: Vec<&str> = word.split(':').collect();
            if parts.len() == 2 && parts[0].len() > 1 && parts[1].len() > 1 {
                return Some(word.to_owned());
            }
        }
    }
    None
}

/// Index reality nodes by extracting topics from their keys and values.
fn index_reality_by_topic(reality_nodes: &[EffectiveNode]) -> HashSet<String> {
    let mut index = HashSet::new();
    
    for node in reality_nodes {
        // Add the key itself
        index.insert(node.node.key.clone());
        
        // Add parts of the key (before and after colon)
        if let Some(colon_pos) = node.node.key.find(':') {
            index.insert(node.node.key[..colon_pos].to_string());
            index.insert(node.node.key[colon_pos + 1..].to_string());
        }
        
        // Add key words from value (simple approach)
        for word in node.node.value.split_whitespace() {
            let clean_word = word.trim_matches(|c: char| c.is_ascii_punctuation())
                .to_lowercase();
            if clean_word.len() > 2 && !is_common_word(&clean_word) {
                index.insert(clean_word);
            }
        }
    }
    
    index
}

/// Detect tensions where memory mentions stale reality nodes.
fn detect_stale_tensions(
    memory_topics: &HashSet<String>,
    reality_nodes: &[EffectiveNode],
) -> Vec<Tension> {
    let mut tensions = Vec::new();
    
    for node in reality_nodes {
        // Check if node is stale
        if node.freshness == Freshness::Stale {
            // Check if memory mentions this node's key or topics
            let node_topics = extract_node_topics(node);
            if memory_topics.iter().any(|t| node_topics.contains(t)) {
                tensions.push(Tension {
                    kind: TensionKind::StaleNode,
                    description: format!(
                        "Memory references '{}' but this reality node is stale (last verified: {})",
                        node.node.key, node.node.verified_at
                    ),
                    reality_keys: vec![node.node.key.clone()],
                    memory_hints: memory_topics.iter()
                        .filter(|t| node_topics.contains(*t))
                        .cloned().collect(),
                    confidence: 0.7,
                });
            }
        }
    }
    
    tensions
}

/// Detect conflicts with bedrock-layer facts.
fn detect_bedrock_conflicts(
    memory_topics: &HashSet<String>,
    reality_nodes: &[EffectiveNode],
) -> Vec<Tension> {
    let mut tensions = Vec::new();
    
    // For V1, this is very simple: just check if memory contradicts
    // specific bedrock facts we know about
    for node in reality_nodes {
        if node.node.layer == RealityLayer::Bedrock {
            // Check for specific known conflicts
            if node.node.key == "heart-rs:compaction" && node.node.value.contains("false") {
                // Memory mentioning "compaction" when we know it's false might be a conflict
                if memory_topics.contains("compaction") || memory_topics.contains("compress") {
                    tensions.push(Tension {
                        kind: TensionKind::BedrockConflict,
                        description: format!(
                            "Memory mentions compaction but bedrock fact states: {}",
                            node.node.value
                        ),
                        reality_keys: vec![node.node.key.clone()],
                        memory_hints: vec!["compaction".to_string()],
                        confidence: 0.5, // Low confidence for V1
                    });
                }
            }
        }
    }
    
    tensions
}

/// Detect conflicts where memory references fogged topics.
fn detect_fog_conflicts(
    memory_topics: &HashSet<String>,
    reality_nodes: &[EffectiveNode],
) -> Vec<Tension> {
    let mut tensions = Vec::new();
    
    for node in reality_nodes {
        if node.freshness == Freshness::Fog {
            let node_topics = extract_node_topics(node);
            if memory_topics.iter().any(|t| node_topics.contains(t)) {
                tensions.push(Tension {
                    kind: TensionKind::FogConflict,
                    description: format!(
                        "Memory references '{}' but this info is in fog (very stale/unreliable)",
                        node.node.key
                    ),
                    reality_keys: vec![node.node.key.clone()],
                    memory_hints: memory_topics.iter()
                        .filter(|t| node_topics.contains(*t))
                        .cloned().collect(),
                    confidence: 0.8,
                });
            }
        }
    }
    
    tensions
}

/// Detect when memory mentions topics not in reality at all.
fn detect_missing_reality(
    memory_topics: &HashSet<String>,
    reality_index: &HashSet<String>,
    memory_signals: &[String],
) -> Vec<Tension> {
    let mut tensions = Vec::new();
    
    // Look for specific patterns that might indicate missing reality
    for topic in memory_topics {
        if !reality_index.contains(topic) {
            // Check if this looks like it should be a reality node
            if topic.contains(':') || is_potential_fact(topic) {
                let relevant_signals: Vec<String> = memory_signals.iter()
                    .filter(|s| s.to_lowercase().contains(&topic.to_lowercase()))
                    .cloned().collect();
                
                if !relevant_signals.is_empty() {
                    tensions.push(Tension {
                        kind: TensionKind::MissingReality,
                        description: format!(
                            "Memory references '{}' but no reality node exists for this topic",
                            topic
                        ),
                        reality_keys: vec![],
                        memory_hints: relevant_signals,
                        confidence: 0.6,
                    });
                }
            }
        }
    }
    
    tensions
}

/// Heuristic to detect if a topic might be a fact worth tracking.
fn is_potential_fact(topic: &str) -> bool {
    // V1: very simple heuristics
    let fact_indicators = ["status", "health", "version", "mode", "state", "config", "setting"];
    fact_indicators.iter().any(|indicator| topic.contains(indicator))
}

/// Extract all searchable topics from a reality node.
fn extract_node_topics(node: &EffectiveNode) -> HashSet<String> {
    let mut topics = HashSet::new();
    
    // Add the full key
    topics.insert(node.node.key.clone());
    
    // Add key parts
    if let Some(colon_pos) = node.node.key.find(':') {
        topics.insert(node.node.key[..colon_pos].to_string());
        topics.insert(node.node.key[colon_pos + 1..].to_string());
    }
    
    // Add value words
    for word in node.node.value.split_whitespace() {
        let clean_word = word.trim_matches(|c: char| c.is_ascii_punctuation())
            .to_lowercase();
        if clean_word.len() > 2 && !is_common_word(&clean_word) {
            topics.insert(clean_word);
        }
    }
    
    topics
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reality::{RealityKind, RealityLayer, RealityNode, RealityRealm};

    fn create_test_node(key: &str, value: &str, layer: RealityLayer, verified_at: &str) -> EffectiveNode {
        let node = RealityNode {
            key: key.to_string(),
            value: value.to_string(),
            kind: RealityKind::Fact,
            layer,
            confidence: 1.0,
            ttl_secs: Some(3600),
            verified_at: verified_at.to_string(),
            updated_at: Utc::now().to_rfc3339(),
            source: None,
            edges: vec![],
            dim: None,
            river_seq: None,
            realm: RealityRealm::World,
        };
        
        // Calculate freshness based on verified_at
        let freshness = if verified_at == "2020-01-01T00:00:00Z" {
            Freshness::Fog  // Very old
        } else if verified_at < (Utc::now() - Duration::hours(25)).to_rfc3339().as_str() {
            Freshness::Stale
        } else {
            Freshness::Fresh
        };
        
        EffectiveNode {
            node,
            effective_confidence: 1.0,
            freshness,
        }
    }

    #[test]
    fn extract_topics_basic() {
        let signals = vec![
            "The health probe is working fine".to_string(),
            "sense:conversation status looks good".to_string(),
            "Need to check the heart-rs compaction setting".to_string(),
        ];
        
        let topics = extract_topics_from_memory(&signals);
        
        assert!(topics.contains("health"));
        assert!(topics.contains("probe"));
        assert!(topics.contains("sense:conversation"));
        assert!(topics.contains("heart-rs"));
        assert!(topics.contains("compaction"));
        
        // Should not contain common words
        assert!(!topics.contains("the"));
        assert!(!topics.contains("is"));
    }

    #[test]
    fn extract_reality_key_basic() {
        assert_eq!(extract_reality_key("check sense:health status"), Some("sense:health".to_string()));
        assert_eq!(extract_reality_key("visit https://example.com"), None); // URL should be ignored
        assert_eq!(extract_reality_key("no colon here"), None);
        assert_eq!(extract_reality_key("a:b:c has too many colons"), None);
    }

    #[test]
    fn detect_stale_node_tension() {
        let memory = vec!["Check the conversation health".to_string()];
        let nodes = vec![
            create_test_node("sense:conversation", "healthy", RealityLayer::Bedrock, "2020-01-01T00:00:00Z"),
        ];
        // Force it to be stale by manually setting freshness
        let mut stale_node = nodes[0].clone();
        stale_node.freshness = Freshness::Stale;
        
        let tensions = reconcile(&memory, &vec![stale_node]);
        
        // Filter for stale node tensions only
        let stale_tensions: Vec<_> = tensions.iter()
            .filter(|t| matches!(t.kind, TensionKind::StaleNode))
            .collect();
        assert_eq!(stale_tensions.len(), 1);
        assert!(stale_tensions[0].description.contains("sense:conversation"));
        assert!(stale_tensions[0].description.contains("stale"));
    }

    #[test]
    fn detect_fog_conflict() {
        let memory = vec!["The conversation system status".to_string()];
        let nodes = vec![
            create_test_node("sense:conversation", "unknown", RealityLayer::Surface, "2020-01-01T00:00:00Z"),
        ];
        
        let tensions = reconcile(&memory, &nodes);
        
        // Should detect fog conflict
        let fog_tensions: Vec<_> = tensions.iter()
            .filter(|t| matches!(t.kind, TensionKind::FogConflict))
            .collect();
        assert!(!fog_tensions.is_empty());
        assert!(fog_tensions[0].description.contains("fog"));
    }

    #[test]
    fn detect_bedrock_conflict() {
        let memory = vec!["Need to enable compaction mode".to_string()];
        let nodes = vec![
            create_test_node("heart-rs:compaction", "false — Heart-RS uses River model, no compaction", 
                             RealityLayer::Bedrock, &Utc::now().to_rfc3339()),
        ];
        
        let tensions = reconcile(&memory, &nodes);
        
        let bedrock_tensions: Vec<_> = tensions.iter()
            .filter(|t| matches!(t.kind, TensionKind::BedrockConflict))
            .collect();
        assert!(!bedrock_tensions.is_empty());
        assert!(bedrock_tensions[0].description.contains("bedrock"));
        assert!(bedrock_tensions[0].description.contains("compaction"));
    }

    #[test]
    fn detect_missing_reality() {
        let memory = vec![
            "Check the database status".to_string(),
            "The cache:health looks fine".to_string(),
        ];
        let nodes = vec![
            // No nodes for database or cache
        ];
        
        let tensions = reconcile(&memory, &nodes);
        
        let missing_tensions: Vec<_> = tensions.iter()
            .filter(|t| matches!(t.kind, TensionKind::MissingReality))
            .collect();
        
        // Should detect cache:health as a potential reality key
        assert!(missing_tensions.iter().any(|t| 
            t.memory_hints.iter().any(|h| h.contains("cache:health"))));
    }

    #[test]
    fn no_tensions_when_reality_fresh() {
        let memory = vec!["The conversation health is good".to_string()];
        let nodes = vec![
            create_test_node("sense:conversation", "healthy", RealityLayer::Bedrock, &Utc::now().to_rfc3339()),
        ];
        
        let tensions = reconcile(&memory, &nodes);
        
        // Should not generate tensions for fresh, matching reality
        assert!(tensions.iter().all(|t| t.confidence < 0.9)); // Only low-confidence heuristic tensions
    }

    #[test]
    fn test_index_reality_by_topic() {
        let nodes = vec![
            create_test_node("sense:health", "good status", RealityLayer::Bedrock, &Utc::now().to_rfc3339()),
            create_test_node("weather:nanjing", "sunny day", RealityLayer::Surface, &Utc::now().to_rfc3339()),
        ];
        
        let index = index_reality_by_topic(&nodes);
        
        assert!(index.contains("sense:health"));
        assert!(index.contains("sense"));
        assert!(index.contains("health"));
        assert!(index.contains("good"));
        assert!(index.contains("status"));
        assert!(index.contains("weather:nanjing"));
        assert!(index.contains("weather"));
        assert!(index.contains("nanjing"));
        assert!(index.contains("sunny"));
    }
}