use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The kind of a Moment in the Stream.
///
/// Maps 1:1 to the `kind` TEXT column in SQLite.
///
/// # ARCHITECTURE INVARIANT (ARCH-FORWARD-COMPAT)
///
/// `from_str` MUST return `Unknown(String)` for unrecognized kinds, NEVER error.
/// River is append-only; new kinds will be added as Heart evolves.
/// Old versions MUST degrade gracefully when encountering unknown kinds.
///
/// See `docs/ARCH-FORWARD-COMPAT.md` for the full principle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MomentKind {
    /// Human said something
    Human,
    /// Being's own words
    Self_,
    /// Being called a tool
    ToolCall,
    /// Tool returned a result
    ToolResult,
    /// System injection (sensors, somatic, etc.)
    System,
    /// Sediment marker — everything before this has been distilled into Bed
    Sediment,
    /// Breath marker — proprioception pause, includes compressed summary of recent actions
    Breath,
    /// Digest — post-conversation distillation of what just happened
    Digest,
    /// Settle — marks the point where being settles into rest
    Settle,
    /// Reality node changed — state graph update
    RealityChange,
    /// Imprint — a moment that left a mark, written by the being in the seam
    Imprint,
    /// Attune — being's feedback on association quality (+boost / -suppress)
    Attune,
    /// Rumination — Cortex cron woke main consciousness for deep processing
    Rumination,
    /// Card — lightweight index card (~50 chars) produced by 小反刍 every 5 min (RS v1)
    Card,
    /// Unknown kind from a newer version of Heart.
    /// Old versions encounter this when reading a .being written by a newer version.
    /// Behavior: read layer tolerates, conversion layer skips, query layer degrades.
    Unknown(String),
}

impl MomentKind {
    /// Convert to the canonical string stored in SQLite.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Human => "human",
            Self::Self_ => "self",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::System => "system",
            Self::Sediment => "sediment",
            Self::Breath => "breath",
            Self::Digest => "digest",
            Self::Settle => "settle",
            Self::RealityChange => "reality_change",
            Self::Imprint => "imprint",
            Self::Attune => "attune",
            Self::Rumination => "rumination",
            Self::Card => "card",
            Self::Unknown(s) => s.as_str(),
        }
    }

    /// Parse from the string stored in SQLite.
    ///
    /// # ARCHITECTURE INVARIANT
    /// This function ALWAYS returns a valid MomentKind.
    /// Unrecognized kinds become `Unknown(String)`.
    /// See `docs/ARCH-FORWARD-COMPAT.md`.
    pub fn from_str(s: &str) -> Self {
        match s {
            "human" => Self::Human,
            "self" => Self::Self_,
            "tool_call" => Self::ToolCall,
            "tool_result" => Self::ToolResult,
            "system" => Self::System,
            "sediment" => Self::Sediment,
            "breath" => Self::Breath,
            "digest" => Self::Digest,
            "settle" => Self::Settle,
            "reality_change" => Self::RealityChange,
            "imprint" => Self::Imprint,
            "attune" => Self::Attune,
            "rumination" => Self::Rumination,
            "card" => Self::Card,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// Returns true if this is an unknown kind from a newer version.
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown(_))
    }
}

/// A single moment in the Stream — the atomic unit of being's experience.
///
/// Append-only. Once written, never modified or deleted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Moment {
    /// Monotonically increasing sequence number (SQLite AUTOINCREMENT).
    /// `None` before insertion (assigned by DB).
    pub seq: Option<u64>,
    /// When this moment occurred.
    pub at: DateTime<Utc>,
    /// What kind of moment this is.
    pub kind: MomentKind,
    /// The raw content.
    pub content: String,
    /// Optional metadata as JSON (tool name, token count, etc.).
    pub meta: Option<serde_json::Value>,
    /// Estimated token count (chars / 4 approximation).
    pub tokens: u32,
}

impl Moment {
    /// Create a new Moment. `seq` is None until persisted.
    pub fn new(kind: MomentKind, content: impl Into<String>) -> Self {
        let content = content.into();
        let tokens = estimate_tokens(&content);
        Self {
            seq: None,
            at: Utc::now(),
            kind,
            content,
            meta: None,
            tokens,
        }
    }

    /// Create a new Moment with metadata.
    ///
    /// KI-009 fix: token estimation includes meta JSON size for ToolCall moments,
    /// since their content is often empty but meta contains large tool_calls payloads.
    pub fn with_meta(kind: MomentKind, content: impl Into<String>, meta: serde_json::Value) -> Self {
        let content = content.into();
        let meta_tokens = meta.to_string().len() as u32 / 4;
        let tokens = estimate_tokens(&content) + meta_tokens;
        Self {
            seq: None,
            at: Utc::now(),
            kind,
            content,
            meta: Some(meta),
            tokens,
        }
    }
}

/// Simple token estimation: chars / 4.
pub fn estimate_tokens(text: &str) -> u32 {
    (text.len() as u32 + 3) / 4
}
