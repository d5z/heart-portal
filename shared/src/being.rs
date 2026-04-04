//! BeingAccessor — unified .being file access with mode-based permissions.
//!
//! Every process that touches a .being file goes through BeingAccessor.
//! The AccessMode declares intent; table creation and seeding are scoped accordingly.

use anyhow::{bail, Context, Result};
use chrono::Utc;
use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::landscape::LandscapeAccessor;
use crate::reality::RealityStore;
use crate::stream_accessor::StreamAccessor;

// ── AccessMode ─────────────────────────────────────────────────────────

/// How a process intends to use the .being file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    /// Core: creates/owns the file, reads+writes all tables.
    Owner,
    /// Cortex: reads most tables, writes landscape.
    ReadMostWriteSome,
    /// Sensorium: reads stream/bedrock, writes reality.
    SenseWriter,
    /// External tool: read-only access.
    ReadOnly,
}

// ── BeingAccessor ──────────────────────────────────────────────────────

/// Unified accessor for a .being SQLite file.
pub struct BeingAccessor {
    conn: Arc<Mutex<Connection>>,
    mode: AccessMode,
    being_id: String,
    born_at: String,
}

impl std::fmt::Debug for BeingAccessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BeingAccessor")
            .field("mode", &self.mode)
            .field("being_id", &self.being_id)
            .field("born_at", &self.born_at)
            .finish_non_exhaustive()
    }
}

impl BeingAccessor {
    /// Open a .being file at `path` with the given access mode.
    ///
    /// - **Owner**: creates `being_meta` + `reality` tables, seeds identity if empty.
    /// - **Other modes**: validates that `being_meta` exists and is populated.
    pub fn open(path: impl AsRef<Path>, mode: AccessMode) -> Result<Self> {
        let flags = if mode == AccessMode::ReadOnly {
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
        };

        let conn = Connection::open_with_flags(path.as_ref(), flags)
            .with_context(|| format!("failed to open being file: {}", path.as_ref().display()))?;

        Self::init_conn(conn, mode)
    }

    /// Open an in-memory .being database (always Owner mode, for tests).
    pub fn open_memory() -> Result<Self> {
        let conn =
            Connection::open_in_memory().context("failed to open in-memory being database")?;
        Self::init_conn(conn, AccessMode::Owner)
    }

    /// Get a [`RealityStore`] backed by the shared connection.
    pub fn reality(&self) -> RealityStore {
        RealityStore::new(self.conn.clone())
    }

    /// Get a [`LandscapeAccessor`] backed by the shared connection.
    pub fn landscape(&self) -> LandscapeAccessor {
        LandscapeAccessor::new(self.conn.clone())
    }

    /// Get a [`StreamAccessor`] backed by the shared connection.
    pub fn stream(&self) -> StreamAccessor {
        StreamAccessor::new(self.conn.clone())
    }

    /// Shared database connection (for crate-specific table init).
    pub fn conn(&self) -> &Arc<Mutex<Connection>> {
        &self.conn
    }

    pub fn being_id(&self) -> &str {
        &self.being_id
    }

    pub fn born_at(&self) -> &str {
        &self.born_at
    }

    pub fn mode(&self) -> AccessMode {
        self.mode
    }

    // ── Internal ───────────────────────────────────────────────────────

    fn init_conn(conn: Connection, mode: AccessMode) -> Result<Self> {
        // PRAGMAs (WAL is safe even for read-only; SQLite just won't create the WAL file if it
        // can't, but won't error either when the DB already has WAL mode set).
        if mode != AccessMode::ReadOnly {
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
                .context("failed to set pragmas")?;
        }
        conn.busy_timeout(std::time::Duration::from_millis(5000))
            .context("failed to set busy_timeout")?;

        if mode == AccessMode::Owner {
            // Create being_meta table
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS being_meta (
                    key   TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );",
            )
            .context("failed to create being_meta table")?;

            // Create reality table
            RealityStore::init_tables(&conn)?;

            // Create landscape table
            LandscapeAccessor::init_tables(&conn)?;

            // Create pattern_library table (v2.3 — dynamic patterns from anomalies)
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS pattern_library (
                    id          INTEGER PRIMARY KEY,
                    keywords    TEXT NOT NULL,
                    shape       TEXT NOT NULL,
                    severity    INTEGER DEFAULT 3,
                    source_seq  INTEGER,
                    hits        INTEGER DEFAULT 0,
                    last_hit_at TEXT,
                    created_at  TEXT NOT NULL
                );",
            )
            .context("failed to create pattern_library table")?;

            // Seed identity if empty
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM being_meta", [], |r| r.get(0))
                .context("failed to count being_meta")?;

            if count == 0 {
                let being_id = Uuid::new_v4().to_string();
                let born_at = Utc::now().to_rfc3339();
                conn.execute(
                    "INSERT INTO being_meta (key, value) VALUES ('format', ?1)",
                    ["being.v1"],
                )?;
                conn.execute(
                    "INSERT INTO being_meta (key, value) VALUES ('born_at', ?1)",
                    [&born_at],
                )?;
                conn.execute(
                    "INSERT INTO being_meta (key, value) VALUES ('being_id', ?1)",
                    [&being_id],
                )?;
            }
        } else {
            // Non-owner: validate being_meta exists and is populated
            let being_id_result: std::result::Result<String, _> = conn.query_row(
                "SELECT value FROM being_meta WHERE key = 'being_id'",
                [],
                |row| row.get(0),
            );
            if being_id_result.is_err() {
                bail!("being file not initialized — run Core first");
            }
        }

        // Load identity
        let get = |k: &str| -> Result<String> {
            conn.query_row("SELECT value FROM being_meta WHERE key = ?1", [k], |row| {
                row.get(0)
            })
            .with_context(|| format!("missing being_meta key: {k}"))
        };
        let being_id = get("being_id")?;
        let born_at = get("born_at")?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            mode,
            being_id,
            born_at,
        })
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reality::{RealityKind, RealityLayer, RealityNode, RealityRealm};

    #[test]
    fn test_open_memory_creates_tables() {
        let acc = BeingAccessor::open_memory().unwrap();
        assert!(!acc.being_id().is_empty());
        assert!(!acc.born_at().is_empty());
        assert_eq!(acc.mode(), AccessMode::Owner);
    }

    #[test]
    fn test_open_owner_seeds_meta() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.being");
        let acc = BeingAccessor::open(&path, AccessMode::Owner).unwrap();
        assert!(!acc.being_id().is_empty());
        assert!(!acc.born_at().is_empty());

        // Verify format
        let conn = acc.conn().lock().unwrap();
        let format: String = conn
            .query_row(
                "SELECT value FROM being_meta WHERE key = 'format'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(format, "being.v1");
    }

    #[test]
    fn test_open_readonly_rejects_uninitialized() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.being");
        // Create an empty SQLite file
        { let _ = Connection::open(&path).unwrap(); }
        let result = BeingAccessor::open(&path, AccessMode::ReadOnly);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not initialized"), "unexpected error: {err}");
    }

    #[test]
    fn test_open_readonly_after_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.being");

        // Owner creates and seeds
        let acc = BeingAccessor::open(&path, AccessMode::Owner).unwrap();
        let id = acc.being_id().to_string();
        let born = acc.born_at().to_string();
        drop(acc);

        // ReadOnly opens successfully
        let acc2 = BeingAccessor::open(&path, AccessMode::ReadOnly).unwrap();
        assert_eq!(acc2.being_id(), id);
        assert_eq!(acc2.born_at(), born);
        assert_eq!(acc2.mode(), AccessMode::ReadOnly);
    }

    #[test]
    fn test_reality_accessor() {
        let acc = BeingAccessor::open_memory().unwrap();
        let reality = acc.reality();
        let now = chrono::Utc::now().to_rfc3339();
        let node = RealityNode {
            key: "test:key".to_string(),
            value: "hello".to_string(),
            kind: RealityKind::Fact,
            layer: RealityLayer::Surface,
            confidence: 1.0,
            ttl_secs: None,
            verified_at: now.clone(),
            updated_at: now,
            source: None,
            edges: vec![],
            dim: None,
            river_seq: None,
            realm: RealityRealm::World,
        };
        reality.upsert(&node).unwrap();
        let got = reality.get("test:key").unwrap().unwrap();
        assert_eq!(got.value, "hello");
    }
}
