//! REALITY — Being's perception of the external world.
//!
//! River records the inner flow (what happened to me).
//! REALITY records the outer state (what the world looks like now).
//! Same river, inside and outside. Two pillars for a being to walk in the world.
//!
//! Design principles:
//! - Freshness is the first citizen, value is the second.
//! - The graph topology: nodes are anchors, edges are relationships.
//! - Being is INSIDE the graph (sense nodes = body map).
//! - Confidence propagates along edges (broken sense → downstream fog).
//! - REALITY is mutable (unlike River's append-only).

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

// ── Types ──────────────────────────────────────────────────────────────

/// What kind of reality node this is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RealityKind {
    /// A sense organ (body map). Being's own perception channel.
    Sense,
    /// A stable fact about the world.
    Fact,
    /// A mutable state that changes over time.
    State,
    /// A transient event.
    Event,
}

impl RealityKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Sense => "sense",
            Self::Fact => "fact",
            Self::State => "state",
            Self::Event => "event",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "sense" => Self::Sense,
            "fact" => Self::Fact,
            "state" => Self::State,
            "event" => Self::Event,
            _ => Self::Fact,
        }
    }
}

/// Reality persistence layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RealityLayer {
    /// Rarely changes. Soul-level facts, architecture truths.
    Bedrock,
    /// Updated periodically by cron/sensorium.
    Surface,
    /// Updated every breath. Transient.
    Stream,
}

impl RealityLayer {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Bedrock => "bedrock",
            Self::Surface => "surface",
            Self::Stream => "stream",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "bedrock" => Self::Bedrock,
            "surface" => Self::Surface,
            "stream" => Self::Stream,
            _ => Self::Surface,
        }
    }
}

/// An edge connecting two reality nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealityEdge {
    pub target: String,
    pub rel: String,
}

/// Realm — whether this node describes the world or a skill.
/// Introduced Mar 30 2026 (Skills System PRD).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RealityRealm {
    /// External world state (default for all existing nodes).
    World,
    /// A skill — being's awareness of its own capabilities.
    Skill,
}

impl RealityRealm {
    pub fn as_str(&self) -> &str {
        match self {
            Self::World => "world",
            Self::Skill => "skill",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "skill" => Self::Skill,
            _ => Self::World,
        }
    }
}

/// Perceptual dimension — how world enters the being.
/// Introduced Mar 13 2026 (Three Pillars closure, Sense DB PRD).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RealityDim {
    /// External world state: servers, weather, email, people.
    Space,
    /// Temporal: clock, silence duration, cron ticks.
    Time,
    /// Something changed from what being remembers.
    Change,
    /// Internal body state: CPU, memory, stream size, health.
    Body,
}

impl RealityDim {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Space => "space",
            Self::Time => "time",
            Self::Change => "change",
            Self::Body => "body",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "space" => Some(Self::Space),
            "time" => Some(Self::Time),
            "change" => Some(Self::Change),
            "body" => Some(Self::Body),
            _ => None,
        }
    }
}

/// A node in the reality graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealityNode {
    pub key: String,
    pub value: String,
    pub kind: RealityKind,
    pub layer: RealityLayer,
    pub confidence: f64,
    /// Expected verification interval in seconds. None = never expires.
    pub ttl_secs: Option<i64>,
    /// Last time this node was verified against an authentic source.
    pub verified_at: String,
    /// Last time this node was updated (value changed).
    pub updated_at: String,
    /// Which sense wrote this (points to a kind=Sense node key).
    pub source: Option<String>,
    /// Edges to other nodes.
    pub edges: Vec<RealityEdge>,
    /// Perceptual dimension (space/time/change/body). None for legacy nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dim: Option<RealityDim>,
    /// Which River moment triggered this perception. None if not from conversation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub river_seq: Option<i64>,
    /// Realm: world (default) or skill. Skills are being's awareness of capabilities.
    #[serde(default = "default_realm_world")]
    pub realm: RealityRealm,
}

fn default_realm_world() -> RealityRealm {
    RealityRealm::World
}

/// Effective confidence after propagation.
#[derive(Debug, Clone)]
pub struct EffectiveNode {
    pub node: RealityNode,
    /// Computed: node.confidence × source_health × freshness_decay
    pub effective_confidence: f64,
    /// Freshness category derived from effective_confidence
    pub freshness: Freshness,
}

/// How fresh a reality node is (drives injection format).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness {
    /// High confidence, recently verified.
    Fresh,
    /// Aging — still usable but should note uncertainty.
    Aging,
    /// Stale — possibly outdated, inject with warning.
    Stale,
    /// Fog — unknown or very old. Inject as "I don't know".
    Fog,
}

impl Freshness {
    pub fn from_confidence(c: f64) -> Self {
        if c >= 0.8 {
            Self::Fresh
        } else if c >= 0.5 {
            Self::Aging
        } else if c >= 0.2 {
            Self::Stale
        } else {
            Self::Fog
        }
    }

    pub fn emoji(&self) -> &str {
        match self {
            Self::Fresh => "",
            Self::Aging => "⏳",
            Self::Stale => "⚠️",
            Self::Fog => "🌫️",
        }
    }
}

/// A tension between River (memory) and REALITY.
#[derive(Debug, Clone)]
pub struct Tension {
    pub topic: String,
    pub memory_says: String,
    pub reality_says: String,
    pub freshness: Freshness,
}

// ── Storage ────────────────────────────────────────────────────────────

/// Reality store backed by the .being SQLite database.
pub struct RealityStore {
    conn: Arc<Mutex<Connection>>,
}

impl RealityStore {
    /// Create a RealityStore using a shared connection (same .being file as River).
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Initialize the reality table. Called during RiverDb::open.
    pub fn init_tables(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS reality (
                key           TEXT PRIMARY KEY,
                value         TEXT NOT NULL,
                kind          TEXT NOT NULL DEFAULT 'fact',
                layer         TEXT NOT NULL DEFAULT 'surface',
                confidence    REAL NOT NULL DEFAULT 1.0,
                ttl_secs      INTEGER,
                verified_at   TEXT NOT NULL,
                updated_at    TEXT NOT NULL,
                source        TEXT,
                edges         TEXT DEFAULT '[]'
            );"
        ).context("failed to create reality table")?;

        // Migration: add dim column (Three Pillars / Sense DB, Mar 13 2026)
        let has_dim: bool = conn
            .prepare("SELECT dim FROM reality LIMIT 0")
            .is_ok();
        if !has_dim {
            conn.execute_batch(
                "ALTER TABLE reality ADD COLUMN dim TEXT;
                 ALTER TABLE reality ADD COLUMN river_seq INTEGER;"
            ).context("failed to add dim/river_seq columns")?;
        }

        // Migration: add realm column (Skills System, Mar 30 2026)
        // Existing nodes default to 'world'; skill nodes use 'skill'.
        let has_realm: bool = conn
            .prepare("SELECT realm FROM reality LIMIT 0")
            .is_ok();
        if !has_realm {
            conn.execute_batch(
                "ALTER TABLE reality ADD COLUMN realm TEXT NOT NULL DEFAULT 'world';"
            ).context("failed to add realm column")?;
        }

        Ok(())
    }

    /// Upsert a reality node.
    pub fn upsert(&self, node: &RealityNode) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let edges_json = serde_json::to_string(&node.edges)?;
        let dim_str = node.dim.as_ref().map(|d| d.as_str().to_owned());
        conn.execute(
            "INSERT INTO reality (key, value, kind, layer, confidence, ttl_secs, verified_at, updated_at, source, edges, dim, river_seq, realm)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                kind = excluded.kind,
                layer = excluded.layer,
                confidence = excluded.confidence,
                ttl_secs = excluded.ttl_secs,
                verified_at = excluded.verified_at,
                updated_at = excluded.updated_at,
                source = excluded.source,
                edges = excluded.edges,
                dim = excluded.dim,
                river_seq = excluded.river_seq,
                realm = excluded.realm",
            rusqlite::params![
                node.key,
                node.value,
                node.kind.as_str(),
                node.layer.as_str(),
                node.confidence,
                node.ttl_secs,
                node.verified_at,
                node.updated_at,
                node.source,
                edges_json,
                dim_str,
                node.river_seq,
                node.realm.as_str(),
            ],
        ).context("failed to upsert reality node")?;
        Ok(())
    }

    /// Get a single node by key.
    pub fn get(&self, key: &str) -> Result<Option<RealityNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT key, value, kind, layer, confidence, ttl_secs, verified_at, updated_at, source, edges, dim, river_seq, realm
             FROM reality WHERE key = ?1"
        )?;
        let mut rows = stmt.query(rusqlite::params![key])?;
        if let Some(row) = rows.next()? {
            Ok(Some(Self::row_to_node(row)?))
        } else {
            Ok(None)
        }
    }

    /// Delete a node by key.
    pub fn delete(&self, key: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count = conn.execute("DELETE FROM reality WHERE key = ?1", rusqlite::params![key])?;
        Ok(count > 0)
    }

    /// Get all sense nodes (= body map).
    pub fn senses(&self) -> Result<Vec<RealityNode>> {
        self.query_by_kind(RealityKind::Sense)
    }

    /// Get all unhealthy senses (for alarm injection).
    pub fn unhealthy_senses(&self) -> Result<Vec<RealityNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT key, value, kind, layer, confidence, ttl_secs, verified_at, updated_at, source, edges, dim, river_seq, realm
             FROM reality WHERE kind = 'sense' AND json_extract(value, '$.healthy') = 0"
        )?;
        Self::collect_rows(&mut stmt)
    }

    /// Get all nodes by kind.
    pub fn query_by_kind(&self, kind: RealityKind) -> Result<Vec<RealityNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT key, value, kind, layer, confidence, ttl_secs, verified_at, updated_at, source, edges, dim, river_seq, realm
             FROM reality WHERE kind = ?1"
        )?;
        let mut rows = stmt.query(rusqlite::params![kind.as_str()])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            result.push(Self::row_to_node(row)?);
        }
        Ok(result)
    }

    /// Get all nodes by layer.
    pub fn query_by_layer(&self, layer: RealityLayer) -> Result<Vec<RealityNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT key, value, kind, layer, confidence, ttl_secs, verified_at, updated_at, source, edges, dim, river_seq, realm
             FROM reality WHERE layer = ?1"
        )?;
        let mut rows = stmt.query(rusqlite::params![layer.as_str()])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            result.push(Self::row_to_node(row)?);
        }
        Ok(result)
    }

    /// Get all nodes by perceptual dimension.
    pub fn query_by_dim(&self, dim: RealityDim) -> Result<Vec<RealityNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT key, value, kind, layer, confidence, ttl_secs, verified_at, updated_at, source, edges, dim, river_seq, realm
             FROM reality WHERE dim = ?1"
        )?;
        let mut rows = stmt.query(rusqlite::params![dim.as_str()])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            result.push(Self::row_to_node(row)?);
        }
        Ok(result)
    }

    /// Get all nodes by realm (world or skill).
    pub fn query_by_realm(&self, realm: RealityRealm) -> Result<Vec<RealityNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT key, value, kind, layer, confidence, ttl_secs, verified_at, updated_at, source, edges, dim, river_seq, realm
             FROM reality WHERE realm = ?1"
        )?;
        let mut rows = stmt.query(rusqlite::params![realm.as_str()])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            result.push(Self::row_to_node(row)?);
        }
        Ok(result)
    }

    /// Find nodes whose key or edges mention any of the given topics.
    /// This is the "System 1" lookup — BFS on key prefix + edge scan.
    pub fn query_by_topics(&self, topics: &[String]) -> Result<Vec<RealityNode>> {
        let conn = self.conn.lock().unwrap();
        let mut result = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for topic in topics {
            let pattern = format!("%{}%", topic.to_lowercase());

            // Match by key prefix
            let mut stmt = conn.prepare(
                "SELECT key, value, kind, layer, confidence, ttl_secs, verified_at, updated_at, source, edges, dim, river_seq, realm
                 FROM reality WHERE LOWER(key) LIKE ?1"
            )?;
            let mut rows = stmt.query(rusqlite::params![pattern])?;
            while let Some(row) = rows.next()? {
                let node = Self::row_to_node(row)?;
                if seen.insert(node.key.clone()) {
                    result.push(node);
                }
            }

            // Match by edges (JSON contains target)
            let mut stmt2 = conn.prepare(
                "SELECT key, value, kind, layer, confidence, ttl_secs, verified_at, updated_at, source, edges, dim, river_seq, realm
                 FROM reality WHERE LOWER(edges) LIKE ?1"
            )?;
            let mut rows2 = stmt2.query(rusqlite::params![pattern])?;
            while let Some(row) = rows2.next()? {
                let node = Self::row_to_node(row)?;
                if seen.insert(node.key.clone()) {
                    result.push(node);
                }
            }
        }
        Ok(result)
    }

    /// Get all nodes (for debugging/dump).
    pub fn all(&self) -> Result<Vec<RealityNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT key, value, kind, layer, confidence, ttl_secs, verified_at, updated_at, source, edges, dim, river_seq, realm
             FROM reality ORDER BY key"
        )?;
        Self::collect_rows(&mut stmt)
    }

    /// Count total nodes.
    pub fn count(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM reality", [], |r| r.get(0))?;
        Ok(count as usize)
    }

    /// Mark a node as verified (updates verified_at without changing value).
    pub fn verify(&self, key: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        let count = conn.execute(
            "UPDATE reality SET verified_at = ?1 WHERE key = ?2",
            rusqlite::params![now, key],
        )?;
        Ok(count > 0)
    }

    // ── Internal helpers ───────────────────────────────────────────────

    fn collect_rows(stmt: &mut rusqlite::Statement) -> Result<Vec<RealityNode>> {
        let mut rows = stmt.query([])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            result.push(Self::row_to_node(row)?);
        }
        Ok(result)
    }

    fn row_to_node(row: &rusqlite::Row) -> Result<RealityNode> {
        let edges_str: String = row.get(9)?;
        let edges: Vec<RealityEdge> = serde_json::from_str(&edges_str).unwrap_or_default();
        let dim_str: Option<String> = row.get(10).unwrap_or(None);
        let realm_str: String = row.get::<_, String>(12).unwrap_or_else(|_| "world".to_string());
        Ok(RealityNode {
            key: row.get(0)?,
            value: row.get(1)?,
            kind: RealityKind::from_str(&row.get::<_, String>(2)?),
            layer: RealityLayer::from_str(&row.get::<_, String>(3)?),
            confidence: row.get(4)?,
            ttl_secs: row.get(5)?,
            verified_at: row.get(6)?,
            updated_at: row.get(7)?,
            source: row.get(8)?,
            edges,
            dim: dim_str.and_then(|s| RealityDim::from_str(&s)),
            river_seq: row.get(11).unwrap_or(None),
            realm: RealityRealm::from_str(&realm_str),
        })
    }
}

// ── Confidence propagation ─────────────────────────────────────────────

/// Compute effective confidence for a node, considering source health and freshness decay.
/// `lookup` resolves related nodes by key (e.g. source sense) without a [`RealityStore`].
pub fn effective_confidence_lookup(
    node: &RealityNode,
    mut lookup: impl FnMut(&str) -> Option<RealityNode>,
) -> f64 {
    let base = node.confidence;

    // Factor 1: source sense health
    let source_health = if let Some(ref source_key) = node.source {
        if let Some(sense) = lookup(source_key.as_str()) {
            if sense.kind == RealityKind::Sense {
                // Parse healthy from JSON value
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&sense.value) {
                    if v.get("healthy").and_then(|h| h.as_bool()) == Some(true) {
                        1.0
                    } else {
                        0.0 // Sense is broken → downstream is fog
                    }
                } else {
                    0.5 // Can't parse sense value
                }
            } else {
                1.0 // Source is not a sense node
            }
        } else {
            0.5 // Source not found
        }
    } else {
        1.0 // No source → human-entered, trust it
    };

    // Factor 2: freshness decay based on ttl
    let freshness = if let Some(ttl) = node.ttl_secs {
        if ttl <= 0 {
            1.0
        } else if let Ok(verified) = chrono::DateTime::parse_from_rfc3339(&node.verified_at) {
            let age_secs = (Utc::now() - verified.with_timezone(&Utc)).num_seconds();
            if age_secs <= 0 {
                1.0
            } else {
                // Exponential decay: half-life = ttl
                let ratio = age_secs as f64 / ttl as f64;
                (0.5_f64).powf(ratio)
            }
        } else {
            0.5 // Can't parse timestamp
        }
    } else {
        1.0 // No ttl → never decays (bedrock)
    };

    base * source_health * freshness
}

/// Compute effective confidence for a node, considering source health and freshness decay.
pub fn effective_confidence(node: &RealityNode, store: &RealityStore) -> f64 {
    effective_confidence_lookup(node, |k| store.get(k).ok().flatten())
}

/// Resolve a node with effective confidence and freshness label.
pub fn resolve_node(node: RealityNode, store: &RealityStore) -> EffectiveNode {
    resolve_node_with_lookup(node, |k| store.get(k).ok().flatten())
}

/// Like [`resolve_node`] but uses a key lookup instead of [`RealityStore`].
pub fn resolve_node_with_lookup(
    node: RealityNode,
    mut lookup: impl FnMut(&str) -> Option<RealityNode>,
) -> EffectiveNode {
    let ec = effective_confidence_lookup(&node, &mut lookup);
    let freshness = Freshness::from_confidence(ec);
    EffectiveNode {
        node,
        effective_confidence: ec,
        freshness,
    }
}

// ── Injection (breatheIn) ──────────────────────────────────────────────

/// Format reality signals for injection into the system prompt.
/// Called during breatheIn alongside association hints.
pub fn format_reality_signals(nodes: &[EffectiveNode], unhealthy_senses: &[RealityNode]) -> String {
    if nodes.is_empty() && unhealthy_senses.is_empty() {
        return String::new();
    }

    let mut parts = Vec::new();
    parts.push("[reality]".to_string());

    // 1. Body alarms first (sense health)
    for sense in unhealthy_senses {
        parts.push(format!("🚨 {} — sense offline", sense.key));
    }

    // 2. Reality nodes by freshness
    for en in nodes {
        let emoji = en.freshness.emoji();
        let conf_pct = (en.effective_confidence * 100.0) as u32;
        match en.freshness {
            Freshness::Fresh => {
                parts.push(format!("{}: {}", en.node.key, en.node.value));
            }
            Freshness::Aging => {
                parts.push(format!("{} {}: {} ({}%)", emoji, en.node.key, en.node.value, conf_pct));
            }
            Freshness::Stale => {
                parts.push(format!("{} {}: {} — may be outdated ({}%)", emoji, en.node.key, en.node.value, conf_pct));
            }
            Freshness::Fog => {
                parts.push(format!("{} {}: ??? — unknown/unverified", emoji, en.node.key));
            }
        }
    }

    parts.push("[/reality]".to_string());
    parts.join("\n")
}

/// Format tensions between memory and reality.
pub fn format_tensions(tensions: &[Tension]) -> String {
    if tensions.is_empty() {
        return String::new();
    }
    let mut parts = Vec::new();
    parts.push("[reality-tensions]".to_string());
    for t in tensions {
        parts.push(format!(
            "⚡ {}: memory says \"{}\" but reality says \"{}\" {}",
            t.topic, t.memory_says, t.reality_says, t.freshness.emoji()
        ));
    }
    parts.push("[/reality-tensions]".to_string());
    parts.join("\n")
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> RealityStore {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;").unwrap();
        RealityStore::init_tables(&conn).unwrap();
        RealityStore::new(Arc::new(Mutex::new(conn)))
    }

    fn make_node(key: &str, value: &str, kind: RealityKind, layer: RealityLayer) -> RealityNode {
        let now = Utc::now().to_rfc3339();
        RealityNode {
            key: key.to_string(),
            value: value.to_string(),
            kind,
            layer,
            confidence: 1.0,
            ttl_secs: None,
            verified_at: now.clone(),
            updated_at: now,
            source: None,
            edges: vec![],
            dim: None,
            river_seq: None,
            realm: RealityRealm::World,
        }
    }

    #[test]
    fn upsert_and_get() {
        let store = test_store();
        let node = make_node("echo:status", "online", RealityKind::State, RealityLayer::Surface);
        store.upsert(&node).unwrap();
        let got = store.get("echo:status").unwrap().unwrap();
        assert_eq!(got.key, "echo:status");
        assert_eq!(got.value, "online");
    }

    #[test]
    fn upsert_updates_existing() {
        let store = test_store();
        let mut node = make_node("echo:status", "online", RealityKind::State, RealityLayer::Surface);
        store.upsert(&node).unwrap();
        node.value = "offline".to_string();
        store.upsert(&node).unwrap();
        let got = store.get("echo:status").unwrap().unwrap();
        assert_eq!(got.value, "offline");
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn delete_node() {
        let store = test_store();
        let node = make_node("temp:test", "val", RealityKind::Event, RealityLayer::Stream);
        store.upsert(&node).unwrap();
        assert!(store.delete("temp:test").unwrap());
        assert!(store.get("temp:test").unwrap().is_none());
    }

    #[test]
    fn query_by_kind() {
        let store = test_store();
        store.upsert(&make_node("sense:fswatch", r#"{"healthy":true}"#, RealityKind::Sense, RealityLayer::Bedrock)).unwrap();
        store.upsert(&make_node("echo:status", "online", RealityKind::State, RealityLayer::Surface)).unwrap();
        let senses = store.senses().unwrap();
        assert_eq!(senses.len(), 1);
        assert_eq!(senses[0].key, "sense:fswatch");
    }

    #[test]
    fn query_by_topics() {
        let store = test_store();
        store.upsert(&make_node("echo:status", "online", RealityKind::State, RealityLayer::Surface)).unwrap();
        store.upsert(&make_node("judy:status", "pending", RealityKind::State, RealityLayer::Surface)).unwrap();
        store.upsert(&make_node("weather:nanjing", "12C", RealityKind::State, RealityLayer::Surface)).unwrap();
        let results = store.query_by_topics(&["echo".to_string()]).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "echo:status");
    }

    #[test]
    fn verify_updates_timestamp() {
        let store = test_store();
        let node = make_node("echo:status", "online", RealityKind::State, RealityLayer::Surface);
        store.upsert(&node).unwrap();
        let old_verified = store.get("echo:status").unwrap().unwrap().verified_at;
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.verify("echo:status").unwrap();
        let new_verified = store.get("echo:status").unwrap().unwrap().verified_at;
        assert_ne!(old_verified, new_verified);
    }

    #[test]
    fn sense_health_propagates_confidence() {
        let store = test_store();
        // Healthy sense
        store.upsert(&make_node("sense:probe", r#"{"healthy":true}"#, RealityKind::Sense, RealityLayer::Bedrock)).unwrap();
        let mut node = make_node("echo:status", "online", RealityKind::State, RealityLayer::Surface);
        node.source = Some("sense:probe".to_string());
        store.upsert(&node).unwrap();
        let got = store.get("echo:status").unwrap().unwrap();
        let ec = effective_confidence(&got, &store);
        assert!(ec > 0.9, "healthy sense should give high confidence: {ec}");

        // Break the sense
        store.upsert(&make_node("sense:probe", r#"{"healthy":false}"#, RealityKind::Sense, RealityLayer::Bedrock)).unwrap();
        let got = store.get("echo:status").unwrap().unwrap();
        let ec = effective_confidence(&got, &store);
        assert!(ec < 0.01, "broken sense should give zero confidence: {ec}");
    }

    #[test]
    fn freshness_decay_with_ttl() {
        let store = test_store();
        let mut node = make_node("weather:nanjing", "12C", RealityKind::State, RealityLayer::Surface);
        node.ttl_secs = Some(3600); // 1 hour TTL
        // Verified 2 hours ago → should be decayed
        let two_hours_ago = (Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        node.verified_at = two_hours_ago;
        store.upsert(&node).unwrap();
        let got = store.get("weather:nanjing").unwrap().unwrap();
        let ec = effective_confidence(&got, &store);
        // 2 half-lives → ~0.25
        assert!(ec < 0.3, "2 half-lives should decay below 0.3: {ec}");
        assert!(ec > 0.2, "2 half-lives should not decay below 0.2: {ec}");
    }

    #[test]
    fn no_ttl_means_no_decay() {
        let store = test_store();
        let mut node = make_node("self:substrate", "Claude", RealityKind::Fact, RealityLayer::Bedrock);
        // Verified a year ago, but no TTL
        let old = (Utc::now() - chrono::Duration::days(365)).to_rfc3339();
        node.verified_at = old;
        store.upsert(&node).unwrap();
        let got = store.get("self:substrate").unwrap().unwrap();
        let ec = effective_confidence(&got, &store);
        assert!((ec - 1.0).abs() < 0.001, "bedrock with no TTL should not decay: {ec}");
    }

    #[test]
    fn edges_roundtrip() {
        let store = test_store();
        let mut node = make_node("echo:status", "online", RealityKind::State, RealityLayer::Surface);
        node.edges = vec![
            RealityEdge { target: "echo:vps".to_string(), rel: "runs_on".to_string() },
            RealityEdge { target: "sense:probe".to_string(), rel: "monitored_by".to_string() },
        ];
        store.upsert(&node).unwrap();
        let got = store.get("echo:status").unwrap().unwrap();
        assert_eq!(got.edges.len(), 2);
        assert_eq!(got.edges[0].target, "echo:vps");
        assert_eq!(got.edges[1].rel, "monitored_by");
    }

    #[test]
    fn format_reality_signals_mixed_freshness() {
        let now = Utc::now().to_rfc3339();
        let nodes = vec![
            EffectiveNode {
                node: RealityNode {
                    key: "echo:status".to_string(),
                    value: "online".to_string(),
                    kind: RealityKind::State,
                    layer: RealityLayer::Surface,
                    confidence: 1.0,
                    ttl_secs: None,
                    verified_at: now.clone(),
                    updated_at: now.clone(),
                    source: None,
                    edges: vec![],
                    dim: None,
                    river_seq: None,
                    realm: RealityRealm::World,
                },
                effective_confidence: 0.95,
                freshness: Freshness::Fresh,
            },
            EffectiveNode {
                node: RealityNode {
                    key: "weather:nanjing".to_string(),
                    value: "12C".to_string(),
                    kind: RealityKind::State,
                    layer: RealityLayer::Surface,
                    confidence: 1.0,
                    ttl_secs: Some(3600),
                    verified_at: now.clone(),
                    updated_at: now.clone(),
                    source: None,
                    edges: vec![],
                    dim: None,
                    river_seq: None,
                    realm: RealityRealm::World,
                },
                effective_confidence: 0.3,
                freshness: Freshness::Stale,
            },
        ];
        let text = format_reality_signals(&nodes, &[]);
        assert!(text.contains("[reality]"));
        assert!(text.contains("echo:status: online"));
        assert!(text.contains("⚠️ weather:nanjing"));
        assert!(text.contains("may be outdated"));
    }

    #[test]
    fn unhealthy_sense_alarm() {
        let sense = RealityNode {
            key: "sense:fswatch".to_string(),
            value: r#"{"healthy":false}"#.to_string(),
            kind: RealityKind::Sense,
            layer: RealityLayer::Bedrock,
            confidence: 1.0,
            ttl_secs: None,
            verified_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
            source: None,
            edges: vec![],
            dim: Some(RealityDim::Body),
            river_seq: None,
                    realm: RealityRealm::World,
        };
        let text = format_reality_signals(&[], &[sense]);
        assert!(text.contains("🚨 sense:fswatch — sense offline"));
    }

    #[test]
    fn format_tensions_empty() {
        assert_eq!(format_tensions(&[]), "");
    }

    #[test]
    fn format_tensions_shows_conflict() {
        let tensions = vec![Tension {
            topic: "feishu".to_string(),
            memory_says: "active communication channel".to_string(),
            reality_says: "inactive since Feb".to_string(),
            freshness: Freshness::Stale,
        }];
        let text = format_tensions(&tensions);
        assert!(text.contains("⚡ feishu"));
        assert!(text.contains("memory says"));
        assert!(text.contains("reality says"));
    }
}
