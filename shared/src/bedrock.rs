//! Bedrock types — shared between Core and Cortex.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A single bedrock entry (soul fragment, anchor, identity, or commitment).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BedrockEntry {
    pub id: i64,
    pub kind: BedrockKind,
    pub key: String,
    pub content: String,
    pub updated: String,
    pub source_seq: Option<u64>,
}

/// The kind of bedrock entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BedrockKind {
    Soul,
    Anchor,
    Identity,
    Commitment,
    /// V11: Relationships — people and beings this being knows
    Relationship,
    /// Stable world knowledge — repos, SSH keys, email, infra facts.
    /// Migrated from reality table (was kind=Fact for non-sense entries).
    Knowledge,
}

impl fmt::Display for BedrockKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BedrockKind::Soul => write!(f, "Soul"),
            BedrockKind::Anchor => write!(f, "Anchor"),
            BedrockKind::Identity => write!(f, "Identity"),
            BedrockKind::Commitment => write!(f, "Commitment"),
            BedrockKind::Relationship => write!(f, "Relationship"),
            BedrockKind::Knowledge => write!(f, "Knowledge"),
        }
    }
}

impl BedrockKind {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "Soul" => Some(Self::Soul),
            "Anchor" => Some(Self::Anchor),
            "Identity" => Some(Self::Identity),
            "Commitment" => Some(Self::Commitment),
            "Relationship" => Some(Self::Relationship),
            "Knowledge" => Some(Self::Knowledge),
            _ => None,
        }
    }
}
