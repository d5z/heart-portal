//! LandscapeAccessor — landscape table (Cortex-woven long-term memory slots).

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::Connection;
use std::sync::{Arc, Mutex};

/// Accessor for the landscape table in a .being file.
pub struct LandscapeAccessor {
    conn: Arc<Mutex<Connection>>,
}

impl LandscapeAccessor {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Create the landscape table if it doesn't exist.
    pub fn init_tables(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS landscape (
                slot       TEXT PRIMARY KEY,
                text       TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );",
        )
        .context("failed to create landscape table")?;
        Ok(())
    }

    /// Read a single landscape slot. Returns (text, updated_at) if found.
    pub fn read_slot(&self, slot: &str) -> Result<Option<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT text, updated_at FROM landscape WHERE slot = ?1")?;
        let mut rows = stmt.query(rusqlite::params![slot])?;
        match rows.next()? {
            Some(row) => Ok(Some((row.get(0)?, row.get(1)?))),
            None => Ok(None),
        }
    }

    /// Read all landscape slots. Returns Vec of (slot, text, updated_at).
    pub fn read_all(&self) -> Result<Vec<(String, String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT slot, text, updated_at FROM landscape ORDER BY slot")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut result = Vec::new();
        for r in rows {
            result.push(r?);
        }
        Ok(result)
    }

    /// Write (upsert) a landscape slot.
    pub fn write_slot(&self, slot: &str, text: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO landscape (slot, text, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(slot) DO UPDATE SET text = excluded.text, updated_at = excluded.updated_at",
            rusqlite::params![slot, text, now],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_accessor() -> LandscapeAccessor {
        let conn = Connection::open_in_memory().unwrap();
        LandscapeAccessor::init_tables(&conn).unwrap();
        LandscapeAccessor::new(Arc::new(Mutex::new(conn)))
    }

    #[test]
    fn test_read_empty() {
        let acc = test_accessor();
        assert!(acc.read_slot("mood").unwrap().is_none());
        assert!(acc.read_all().unwrap().is_empty());
    }

    #[test]
    fn test_write_and_read() {
        let acc = test_accessor();
        acc.write_slot("mood", "calm and focused").unwrap();

        let (text, updated_at) = acc.read_slot("mood").unwrap().unwrap();
        assert_eq!(text, "calm and focused");
        assert!(!updated_at.is_empty());
    }

    #[test]
    fn test_upsert_overwrites() {
        let acc = test_accessor();
        acc.write_slot("mood", "calm").unwrap();
        acc.write_slot("mood", "excited").unwrap();

        let (text, _) = acc.read_slot("mood").unwrap().unwrap();
        assert_eq!(text, "excited");
    }

    #[test]
    fn test_read_all() {
        let acc = test_accessor();
        acc.write_slot("mood", "calm").unwrap();
        acc.write_slot("context", "building heart").unwrap();

        let all = acc.read_all().unwrap();
        assert_eq!(all.len(), 2);
        // Sorted by slot name
        assert_eq!(all[0].0, "context");
        assert_eq!(all[1].0, "mood");
    }
}
