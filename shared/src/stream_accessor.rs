//! StreamAccessor — read-mostly interface to a being's stream.
//!
//! Used by Cortex tools (remember, reflect, anchor, recall, search).
//! Extracted from Core's StreamStore as a shared-layer subset.

use crate::{estimate_tokens, Moment, MomentKind};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use std::sync::{Arc, Mutex};

/// Stream accessor — read-mostly interface to a being's stream.
pub struct StreamAccessor {
    conn: Arc<Mutex<Connection>>,
}

// ── internal helpers ────────────────────────────────────────────────────

struct RawRow {
    seq: u64,
    at: String,
    kind: String,
    content: String,
    meta: Option<String>,
    tokens: u32,
}

fn raw_to_moment(r: RawRow) -> Result<Moment> {
    let at: DateTime<Utc> = DateTime::parse_from_rfc3339(&r.at)
        .with_context(|| format!("invalid timestamp in stream row seq={}", r.seq))?
        .with_timezone(&Utc);

    let kind = MomentKind::from_str(&r.kind);
    if kind.is_unknown() {
        tracing::warn!("stream seq={}: unknown moment kind '{}' (newer Heart version?)", r.seq, r.kind);
    }

    let meta = match r.meta {
        Some(s) => Some(
            serde_json::from_str(&s)
                .with_context(|| format!("invalid meta JSON at seq={}", r.seq))?,
        ),
        None => None,
    };

    Ok(Moment {
        seq: Some(r.seq),
        at,
        kind,
        content: r.content,
        meta,
        tokens: r.tokens,
    })
}

impl StreamAccessor {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Search moments by text content (LIKE match, upgradeable to FTS).
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Moment>> {
        let conn = self.conn.lock().unwrap();
        let words: Vec<&str> = query.split_whitespace().filter(|w| !w.is_empty()).collect();
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let mut where_clauses: Vec<String> = Vec::new();
        let mut param_values: Vec<String> = Vec::new();
        for (i, word) in words.iter().enumerate() {
            where_clauses.push(format!("content LIKE ?{} COLLATE NOCASE", i + 1));
            param_values.push(format!("%{word}%"));
        }
        let sql = format!(
            "SELECT seq, at, kind, content, meta, tokens FROM stream WHERE {} ORDER BY seq DESC LIMIT ?{}",
            where_clauses.join(" AND "),
            words.len() + 1
        );
        let mut stmt = conn.prepare(&sql)?;
        let limit_val = limit as i64;
        let params_ref: Vec<&dyn rusqlite::types::ToSql> = param_values
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .chain(std::iter::once(&limit_val as &dyn rusqlite::types::ToSql))
            .collect();
        let rows = stmt.query_map(params_ref.as_slice(), |row| {
            Ok(RawRow {
                seq: row.get::<_, i64>(0)? as u64,
                at: row.get::<_, String>(1)?,
                kind: row.get::<_, String>(2)?,
                content: row.get::<_, String>(3)?,
                meta: row.get::<_, Option<String>>(4)?,
                tokens: row.get::<_, u32>(5)?,
            })
        })?;
        let mut moments = Vec::new();
        for row in rows {
            let r = row?;
            moments.push(raw_to_moment(r)?);
        }
        moments.reverse(); // chronological order
        Ok(moments)
    }

    /// Get moments in a time range (RFC 3339 strings).
    pub fn time_range(&self, from: &str, to: &str) -> Result<Vec<Moment>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT seq, at, kind, content, meta, tokens FROM stream
             WHERE at >= ?1 AND at <= ?2
             ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![from, to], |row| {
            Ok(RawRow {
                seq: row.get::<_, i64>(0)? as u64,
                at: row.get::<_, String>(1)?,
                kind: row.get::<_, String>(2)?,
                content: row.get::<_, String>(3)?,
                meta: row.get::<_, Option<String>>(4)?,
                tokens: row.get::<_, u32>(5)?,
            })
        })?;
        let mut moments = Vec::new();
        for row in rows {
            let r = row?;
            moments.push(raw_to_moment(r)?);
        }
        Ok(moments)
    }

    /// Get moments by seq range (inclusive).
    pub fn seq_range(&self, from: i64, to: i64) -> Result<Vec<Moment>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT seq, at, kind, content, meta, tokens FROM stream
             WHERE seq >= ?1 AND seq <= ?2
             ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![from, to], |row| {
            Ok(RawRow {
                seq: row.get::<_, i64>(0)? as u64,
                at: row.get::<_, String>(1)?,
                kind: row.get::<_, String>(2)?,
                content: row.get::<_, String>(3)?,
                meta: row.get::<_, Option<String>>(4)?,
                tokens: row.get::<_, u32>(5)?,
            })
        })?;
        let mut moments = Vec::new();
        for row in rows {
            let r = row?;
            moments.push(raw_to_moment(r)?);
        }
        Ok(moments)
    }

    /// Append a moment to the stream. Returns the assigned seq.
    pub fn append(&self, kind: MomentKind, content: &str) -> Result<i64> {
        let tokens = estimate_tokens(content);
        let at = Utc::now().to_rfc3339();
        let kind_str = kind.as_str();

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO stream (at, kind, content, meta, tokens) VALUES (?1, ?2, ?3, NULL, ?4)",
            params![at, kind_str, content, tokens],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Get unsedimented tail (moments after last settle/sediment marker).
    pub fn unsedimented_tail(&self) -> Result<Vec<Moment>> {
        let conn = self.conn.lock().unwrap();

        let last_sediment_seq: i64 = {
            let mut stmt = conn.prepare(
                "SELECT seq FROM stream WHERE kind IN ('settle', 'sediment') ORDER BY seq DESC LIMIT 1",
            )?;
            let mut rows = stmt.query([])?;
            match rows.next()? {
                Some(row) => row.get::<_, i64>(0)?,
                None => 0,
            }
        };

        let mut stmt = conn.prepare(
            "SELECT seq, at, kind, content, meta, tokens FROM stream
             WHERE seq > ?1 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![last_sediment_seq], |row| {
            Ok(RawRow {
                seq: row.get::<_, i64>(0)? as u64,
                at: row.get::<_, String>(1)?,
                kind: row.get::<_, String>(2)?,
                content: row.get::<_, String>(3)?,
                meta: row.get::<_, Option<String>>(4)?,
                tokens: row.get::<_, u32>(5)?,
            })
        })?;
        let mut moments = Vec::new();
        for row in rows {
            let r = row?;
            moments.push(raw_to_moment(r)?);
        }
        Ok(moments)
    }

    /// Get moment count.
    pub fn count(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM stream", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Get total token count.
    pub fn total_tokens(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let sum: i64 =
            conn.query_row("SELECT COALESCE(SUM(tokens), 0) FROM stream", [], |row| {
                row.get(0)
            })?;
        Ok(sum as usize)
    }
}

// ── tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup() -> StreamAccessor {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS stream (
                 seq     INTEGER PRIMARY KEY AUTOINCREMENT,
                 at      TEXT    NOT NULL,
                 kind    TEXT    NOT NULL,
                 content TEXT    NOT NULL,
                 meta    TEXT,
                 tokens  INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_stream_at   ON stream(at);
             CREATE INDEX IF NOT EXISTS idx_stream_kind  ON stream(kind);",
        )
        .unwrap();
        StreamAccessor::new(Arc::new(Mutex::new(conn)))
    }

    #[test]
    fn test_append_and_search() {
        let sa = setup();
        sa.append(MomentKind::Human, "hello world").unwrap();
        sa.append(MomentKind::Self_, "goodbye moon").unwrap();
        sa.append(MomentKind::Human, "hello again").unwrap();

        let results = sa.search("hello", 10).unwrap();
        assert_eq!(results.len(), 2);
        assert!(results[0].content.contains("hello"));
        assert!(results[1].content.contains("hello"));
    }

    #[test]
    fn test_seq_range() {
        let sa = setup();
        for i in 1..=5 {
            sa.append(MomentKind::Human, &format!("msg {i}")).unwrap();
        }
        let results = sa.seq_range(2, 4).unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].content, "msg 2");
        assert_eq!(results[2].content, "msg 4");
    }

    #[test]
    fn test_time_range() {
        let sa = setup();
        sa.append(MomentKind::Human, "early").unwrap();
        sa.append(MomentKind::Human, "late").unwrap();

        // Query with a wide range that covers "now"
        let from = "2020-01-01T00:00:00+00:00";
        let to = "2099-01-01T00:00:00+00:00";
        let results = sa.time_range(from, to).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_unsedimented_tail() {
        let sa = setup();
        sa.append(MomentKind::Human, "old msg").unwrap();
        sa.append(MomentKind::Sediment, "sediment marker").unwrap();
        sa.append(MomentKind::Human, "new msg").unwrap();

        let tail = sa.unsedimented_tail().unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].content, "new msg");
    }

    #[test]
    fn test_count_and_tokens() {
        let sa = setup();
        sa.append(MomentKind::Human, "aaaa").unwrap(); // 4 chars → (4+3)/4 = 1
        sa.append(MomentKind::Self_, "bbbbbbbb").unwrap(); // 8 chars → (8+3)/4 = 2

        assert_eq!(sa.count().unwrap(), 2);
        assert_eq!(sa.total_tokens().unwrap(), 3);
    }

    #[test]
    fn test_append_returns_seq() {
        let sa = setup();
        let s1 = sa.append(MomentKind::Human, "first").unwrap();
        let s2 = sa.append(MomentKind::Human, "second").unwrap();
        let s3 = sa.append(MomentKind::Human, "third").unwrap();
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);
        assert_eq!(s3, 3);
    }
}
