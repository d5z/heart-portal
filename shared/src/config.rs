//! Shared configuration types.

use serde::{Deserialize, Serialize};

// ── Near Field (context management) ────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NearFieldConfig {
    /// Max ratio of context window to use (default: 0.85)
    pub max_context_ratio: f32,
    /// Number of recent tool results to protect from compaction (default: 10)
    pub protected_recent_results: usize,
    /// Context fullness that triggers preemptive compaction (default: 0.95)
    pub full_threshold: f32,
}

impl Default for NearFieldConfig {
    fn default() -> Self {
        Self {
            max_context_ratio: 0.85,
            protected_recent_results: 10,
            full_threshold: 0.95,
        }
    }
}
