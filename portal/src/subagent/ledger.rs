//! On-disk bookkeeping for sub-agent sessions and tasks (PRD §4.3 / §4.10).
//!
//! `<state_dir>/ledger.json` answers two questions after a restart: which pi
//! session file belongs to which session key (so a harness can be resumed),
//! and which tasks were still running (so the being gets an `interrupted`
//! callback instead of silence — PRD §7.4).
//!
//! This is *bookkeeping, not memory*: the sub-agent's accumulated experience
//! lives in pi's session JSONL, never here.
//!
//! Writes are atomic (tmp + rename) and 0600, because the brief head can quote
//! being-supplied text.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

/// Bumped only for incompatible shape changes; unknown values load as empty.
pub const LEDGER_VERSION: u32 = 1;
/// Finished tasks retained on disk (PRD §7.3).
pub const MAX_FINISHED_TASKS: usize = 200;
/// Head of the brief kept for `portal_subagent_status`.
const BRIEF_HEAD_BYTES: usize = 200;

/// Lifecycle of one task, as persisted. Mirrors `TaskStatus` in `mod.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerTaskStatus {
    /// No longer produced: Portal refuses a busy session instead of queueing.
    /// Kept so ledgers written by older Portals still load and reconcile.
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
    Interrupted,
    BudgetExhausted,
    Timeout,
}

impl LedgerTaskStatus {
    /// Running (and legacy queued) tasks are the ones restart reconciliation
    /// must resolve.
    pub fn is_open(self) -> bool {
        matches!(self, LedgerTaskStatus::Queued | LedgerTaskStatus::Running)
    }
}

/// Whether Heart was told about the outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerCallbackState {
    Pending,
    Sent,
    Failed,
    /// Deliberately not delivered (cancel — mirrors `portal_process kill`).
    Suppressed,
}

/// One session key → one pi session file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionRecord {
    /// Path to pi's session JSONL. Resuming is `create{sessionPath}`.
    pub session_file: Option<String>,
    /// Fixed at first create; a later spawn with a different workdir is refused.
    pub cwd: String,
    pub created_ms: u64,
    pub last_used_ms: u64,
    pub tasks_total: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskRecord {
    pub task_id: String,
    pub session: String,
    pub brief_head: String,
    pub workdir: String,
    pub scene_id: Option<String>,
    pub status: Option<LedgerTaskStatus>,
    pub callback: Option<LedgerCallbackState>,
    pub created_ms: u64,
    pub ended_ms: Option<u64>,
    pub turns: u32,
    pub tokens_total: u64,
    pub error: Option<String>,
}

impl TaskRecord {
    pub fn status(&self) -> LedgerTaskStatus {
        self.status.unwrap_or(LedgerTaskStatus::Interrupted)
    }
}

/// The whole file. `path` is runtime state, not serialized.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub sessions: BTreeMap<String, SessionRecord>,
    #[serde(default)]
    pub tasks: Vec<TaskRecord>,
    #[serde(skip)]
    path: PathBuf,
}

fn default_version() -> u32 {
    LEDGER_VERSION
}

impl Ledger {
    /// Empty ledger bound to `path` (nothing written until `save`).
    pub fn empty(path: PathBuf) -> Self {
        Self {
            version: LEDGER_VERSION,
            sessions: BTreeMap::new(),
            tasks: Vec::new(),
            path,
        }
    }

    /// Load, or start empty. A corrupt or future-version ledger is moved aside
    /// rather than failing Portal startup: losing bookkeeping costs one
    /// reconciliation pass, losing Portal costs the being their body.
    pub fn load(path: PathBuf) -> Self {
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::empty(path),
            Err(e) => {
                warn!("cannot read {}: {e}; starting a fresh ledger", path.display());
                return Self::empty(path);
            }
        };

        match serde_json::from_slice::<Ledger>(&data) {
            Ok(mut ledger) if ledger.version <= LEDGER_VERSION => {
                ledger.path = path;
                debug!(
                    "loaded subagent ledger: {} session(s), {} task(s)",
                    ledger.sessions.len(),
                    ledger.tasks.len()
                );
                ledger
            }
            Ok(ledger) => {
                warn!(
                    "ledger {} is version {} (Portal understands {}); ignoring it",
                    path.display(),
                    ledger.version,
                    LEDGER_VERSION
                );
                Self::archive_and_reset(path)
            }
            Err(e) => {
                warn!("ledger {} is unreadable ({e}); archiving it", path.display());
                Self::archive_and_reset(path)
            }
        }
    }

    fn archive_and_reset(path: PathBuf) -> Self {
        let archived = path.with_extension(format!("json.broken-{}", now_ms()));
        if let Err(e) = std::fs::rename(&path, &archived) {
            debug!("could not archive {}: {e}", path.display());
        }
        Self::empty(path)
    }

    #[allow(dead_code)] // used by tests and future ledger tooling
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Atomic write: serialize to `<path>.tmp`, fsync, rename over the target.
    /// A crash mid-write leaves the previous ledger intact.
    pub fn save(&self) -> Result<()> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let json = serde_json::to_vec_pretty(self)?;
        let tmp = self.path.with_extension("json.tmp");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(&json)?;
            f.sync_all()?;
        }
        restrict_file(&tmp);
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming {} into place", tmp.display()))?;
        Ok(())
    }

    /// Best-effort save: a ledger write failure must not fail a task that ran.
    pub fn save_lossy(&self) {
        if let Err(e) = self.save() {
            warn!("could not persist the subagent ledger: {e:#}");
        }
    }

    // ── sessions ────────────────────────────────────────────────────

    pub fn session(&self, key: &str) -> Option<&SessionRecord> {
        self.sessions.get(key)
    }

    /// Record (or refresh) the mapping from session key to pi session file.
    pub fn upsert_session(&mut self, key: &str, cwd: &str, session_file: Option<&str>) {
        let now = now_ms();
        let entry = self.sessions.entry(key.to_string()).or_insert_with(|| {
            SessionRecord {
                cwd: cwd.to_string(),
                created_ms: now,
                ..Default::default()
            }
        });
        entry.last_used_ms = now;
        if entry.cwd.is_empty() {
            entry.cwd = cwd.to_string();
        }
        if let Some(file) = session_file {
            entry.session_file = Some(file.to_string());
        }
    }

    pub fn touch_session(&mut self, key: &str) {
        if let Some(entry) = self.sessions.get_mut(key) {
            entry.last_used_ms = now_ms();
            entry.tasks_total += 1;
        }
    }

    /// Forget a session mapping (`reset`) so the next spawn starts a fresh
    /// harness. The file itself is archived by the caller, never deleted.
    #[allow(dead_code)] // wired up by portal_subagent_session (PRD §6.5)
    pub fn forget_session(&mut self, key: &str) -> Option<SessionRecord> {
        self.sessions.remove(key)
    }

    // ── tasks ───────────────────────────────────────────────────────

    #[allow(dead_code)] // used by tests and restart diagnostics
    pub fn task(&self, task_id: &str) -> Option<&TaskRecord> {
        self.tasks.iter().find(|t| t.task_id == task_id)
    }

    /// Append a task row. Called before the prompt is sent, so a crash between
    /// spawn and completion still leaves evidence to reconcile.
    #[allow(clippy::too_many_arguments)]
    pub fn start_task(
        &mut self,
        task_id: &str,
        session: &str,
        brief: &str,
        workdir: &str,
        scene_id: Option<&str>,
    ) {
        self.tasks.push(TaskRecord {
            task_id: task_id.to_string(),
            session: session.to_string(),
            brief_head: head(brief, BRIEF_HEAD_BYTES),
            workdir: workdir.to_string(),
            scene_id: scene_id.map(str::to_string),
            status: Some(LedgerTaskStatus::Running),
            callback: Some(LedgerCallbackState::Pending),
            created_ms: now_ms(),
            ..Default::default()
        });
    }

    /// Final state for a task. Unknown ids are ignored (the row may have been
    /// pruned while the task ran).
    pub fn finish_task(
        &mut self,
        task_id: &str,
        status: LedgerTaskStatus,
        turns: u32,
        tokens_total: u64,
        error: Option<&str>,
    ) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.task_id == task_id) {
            t.status = Some(status);
            t.ended_ms = Some(now_ms());
            t.turns = turns;
            t.tokens_total = tokens_total;
            t.error = error.map(str::to_string);
        }
    }

    pub fn set_callback_state(&mut self, task_id: &str, state: LedgerCallbackState) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.task_id == task_id) {
            t.callback = Some(state);
        }
    }

    /// Tasks that were mid-flight when Portal stopped (PRD §7.4 step 1).
    pub fn open_tasks(&self) -> Vec<TaskRecord> {
        self.tasks
            .iter()
            .filter(|t| t.status().is_open())
            .cloned()
            .collect()
    }

    /// Mark every open task interrupted — the honest outcome when the daemon
    /// is gone and the work cannot be recovered.
    pub fn interrupt_open_tasks(&mut self, error: &str) -> Vec<TaskRecord> {
        let mut changed = Vec::new();
        for t in self.tasks.iter_mut().filter(|t| t.status().is_open()) {
            t.status = Some(LedgerTaskStatus::Interrupted);
            t.ended_ms = Some(now_ms());
            t.error = Some(error.to_string());
            changed.push(t.clone());
        }
        changed
    }

    /// Keep the newest [`MAX_FINISHED_TASKS`] finished rows; open tasks always stay.
    pub fn prune(&mut self) {
        let finished = self.tasks.iter().filter(|t| !t.status().is_open()).count();
        if finished <= MAX_FINISHED_TASKS {
            return;
        }
        let mut to_drop = finished - MAX_FINISHED_TASKS;
        // `tasks` is append-ordered, so dropping from the front drops the oldest.
        self.tasks.retain(|t| {
            if to_drop > 0 && !t.status().is_open() {
                to_drop -= 1;
                false
            } else {
                true
            }
        });
    }
}

/// Head of `s` capped at `max` bytes, never splitting a UTF-8 char.
fn head(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn restrict_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        debug!("could not chmod 0600 {}: {e}", path.display());
    }
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_ledger_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("portal-ledger-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("ledger.json")
    }

    fn cleanup(path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn missing_file_loads_as_empty() {
        let path = temp_ledger_path("missing");
        let ledger = Ledger::load(path.clone());
        assert_eq!(ledger.version, LEDGER_VERSION);
        assert!(ledger.sessions.is_empty());
        assert!(ledger.tasks.is_empty());
        cleanup(&path);
    }

    #[test]
    fn save_then_load_round_trips_sessions_and_tasks() {
        let path = temp_ledger_path("roundtrip");
        let mut ledger = Ledger::empty(path.clone());
        ledger.upsert_session("scene-42", "/ws/proj", Some("/state/pi/sessions/a.jsonl"));
        ledger.start_task("sub_1", "scene-42", "Refactor auth", "/ws/proj", Some("desk-1"));
        ledger.finish_task("sub_1", LedgerTaskStatus::Done, 14, 38330, None);
        ledger.set_callback_state("sub_1", LedgerCallbackState::Sent);
        ledger.save().unwrap();

        let loaded = Ledger::load(path.clone());
        let session = loaded.session("scene-42").unwrap();
        assert_eq!(session.cwd, "/ws/proj");
        assert_eq!(
            session.session_file.as_deref(),
            Some("/state/pi/sessions/a.jsonl")
        );
        let task = loaded.task("sub_1").unwrap();
        assert_eq!(task.status(), LedgerTaskStatus::Done);
        assert_eq!(task.callback, Some(LedgerCallbackState::Sent));
        assert_eq!(task.turns, 14);
        assert_eq!(task.tokens_total, 38330);
        assert_eq!(task.scene_id.as_deref(), Some("desk-1"));
        assert_eq!(loaded.path(), path.as_path());
        cleanup(&path);
    }

    #[test]
    fn save_is_atomic_and_private() {
        let path = temp_ledger_path("atomic");
        let mut ledger = Ledger::empty(path.clone());
        ledger.upsert_session("k", "/ws", None);
        ledger.save().unwrap();
        assert!(path.exists());
        assert!(
            !path.with_extension("json.tmp").exists(),
            "the tmp file must be renamed, not left behind"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        cleanup(&path);
    }

    #[test]
    fn corrupt_ledger_is_archived_not_fatal() {
        let path = temp_ledger_path("corrupt");
        std::fs::write(&path, b"{ this is not json").unwrap();
        let ledger = Ledger::load(path.clone());
        assert!(ledger.tasks.is_empty());
        let dir = path.parent().unwrap();
        let archived: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("broken"))
            .collect();
        assert_eq!(archived.len(), 1, "the unreadable file must be kept aside");
        cleanup(&path);
    }

    #[test]
    fn future_version_is_ignored() {
        let path = temp_ledger_path("future");
        std::fs::write(&path, br#"{"version":99,"sessions":{},"tasks":[]}"#).unwrap();
        let ledger = Ledger::load(path.clone());
        assert_eq!(ledger.version, LEDGER_VERSION);
        cleanup(&path);
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        let path = temp_ledger_path("unknown");
        std::fs::write(
            &path,
            br#"{"version":1,"sessions":{"k":{"cwd":"/ws","future":1}},"tasks":[],"extra":true}"#,
        )
        .unwrap();
        let ledger = Ledger::load(path.clone());
        assert_eq!(ledger.session("k").unwrap().cwd, "/ws");
        cleanup(&path);
    }

    #[test]
    fn open_tasks_are_reported_and_interruptible() {
        let mut ledger = Ledger::empty(PathBuf::new());
        ledger.start_task("sub_open", "k", "b", "/ws", None);
        ledger.start_task("sub_done", "k", "b", "/ws", None);
        ledger.finish_task("sub_done", LedgerTaskStatus::Done, 1, 10, None);

        let open = ledger.open_tasks();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].task_id, "sub_open");

        let changed = ledger.interrupt_open_tasks("Portal restarted while the task was running");
        assert_eq!(changed.len(), 1);
        assert_eq!(
            ledger.task("sub_open").unwrap().status(),
            LedgerTaskStatus::Interrupted
        );
        assert!(ledger.task("sub_open").unwrap().error.is_some());
        assert!(ledger.open_tasks().is_empty());
    }

    #[test]
    fn prune_keeps_the_cap_and_never_drops_open_tasks() {
        let mut ledger = Ledger::empty(PathBuf::new());
        ledger.start_task("sub_running", "k", "b", "/ws", None);
        for i in 0..MAX_FINISHED_TASKS + 25 {
            let id = format!("sub_{i}");
            ledger.start_task(&id, "k", "b", "/ws", None);
            ledger.finish_task(&id, LedgerTaskStatus::Done, 0, 0, None);
        }
        ledger.prune();

        let finished = ledger.tasks.iter().filter(|t| !t.status().is_open()).count();
        assert_eq!(finished, MAX_FINISHED_TASKS);
        assert!(ledger.task("sub_running").is_some(), "open task must survive");
        // The oldest finished rows are the ones dropped.
        assert!(ledger.task("sub_0").is_none());
        assert!(ledger.task(&format!("sub_{}", MAX_FINISHED_TASKS + 24)).is_some());
    }

    #[test]
    fn brief_head_is_clamped_on_a_char_boundary() {
        let mut ledger = Ledger::empty(PathBuf::new());
        let brief = "é".repeat(500);
        ledger.start_task("sub_1", "k", &brief, "/ws", None);
        let stored = &ledger.task("sub_1").unwrap().brief_head;
        assert!(stored.len() <= BRIEF_HEAD_BYTES + 4);
        assert!(stored.ends_with('…'));
        assert!(std::str::from_utf8(stored.as_bytes()).is_ok());
    }

    #[test]
    fn touch_session_counts_tasks_and_forget_removes_the_mapping() {
        let mut ledger = Ledger::empty(PathBuf::new());
        ledger.upsert_session("k", "/ws", Some("/f.jsonl"));
        ledger.touch_session("k");
        ledger.touch_session("k");
        assert_eq!(ledger.session("k").unwrap().tasks_total, 2);

        let removed = ledger.forget_session("k").unwrap();
        assert_eq!(removed.session_file.as_deref(), Some("/f.jsonl"));
        assert!(ledger.session("k").is_none());
    }

    #[test]
    fn upsert_preserves_cwd_and_created_but_refreshes_the_file() {
        let mut ledger = Ledger::empty(PathBuf::new());
        ledger.upsert_session("k", "/ws/original", None);
        let created = ledger.session("k").unwrap().created_ms;
        ledger.upsert_session("k", "/ws/other", Some("/new.jsonl"));
        let rec = ledger.session("k").unwrap();
        assert_eq!(rec.cwd, "/ws/original", "a session's cwd is fixed at create");
        assert_eq!(rec.created_ms, created);
        assert_eq!(rec.session_file.as_deref(), Some("/new.jsonl"));
    }
}
