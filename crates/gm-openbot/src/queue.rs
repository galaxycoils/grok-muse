use chrono::Utc;
use gm_core::error::{GmError, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedTaskStatus {
    Queued,
    Running,
    PauseRequested,
    CancelRequested,
    Paused,
    WaitingForHuman,
    Completed,
    Failed,
    Cancelled,
}

impl QueuedTaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            QueuedTaskStatus::Queued => "queued",
            QueuedTaskStatus::Running => "running",
            QueuedTaskStatus::PauseRequested => "pause_requested",
            QueuedTaskStatus::CancelRequested => "cancel_requested",
            QueuedTaskStatus::Paused => "paused",
            QueuedTaskStatus::WaitingForHuman => "waiting_for_human",
            QueuedTaskStatus::Completed => "completed",
            QueuedTaskStatus::Failed => "failed",
            QueuedTaskStatus::Cancelled => "cancelled",
        }
    }

    pub fn from_db_str(s: &str) -> Result<Self> {
        Ok(match s {
            "queued" => QueuedTaskStatus::Queued,
            "running" => QueuedTaskStatus::Running,
            "pause_requested" => QueuedTaskStatus::PauseRequested,
            "cancel_requested" => QueuedTaskStatus::CancelRequested,
            "paused" => QueuedTaskStatus::Paused,
            "waiting_for_human" => QueuedTaskStatus::WaitingForHuman,
            "completed" => QueuedTaskStatus::Completed,
            "failed" => QueuedTaskStatus::Failed,
            "cancelled" => QueuedTaskStatus::Cancelled,
            _ => {
                return Err(GmError::Config(format!(
                    "Unknown persisted task status: {s}"
                )));
            }
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedTask {
    pub id: String,
    pub bot_id: String,
    pub channel_id: Option<String>,
    pub priority: i32,
    pub status: QueuedTaskStatus,
    pub instruction: String,
    pub blocker_note: Option<String>,
    pub human_resume_note: Option<String>,
    pub result_summary: Option<String>,
    pub mutating_in_flight: bool,
    pub recovery_count: u32,
    pub handoff_depth: u8,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub attempt_id: Option<String>,
}

/// Crash-safe SQLite WAL FIFO task queue supporting per-bot backlogs, pause/resume/cancel/reassign,
/// "Take the Wheel" human handoff states, reboot recovery, and JSONL backup/restore.
#[derive(Clone)]
pub struct DurableTaskQueue {
    pub(crate) conn: Arc<Mutex<Connection>>,
    max_recoveries: u32,
}

impl DurableTaskQueue {
    pub fn open(db_path: impl AsRef<Path>) -> Result<Self> {
        let conn = crate::db_recovery::open_sqlite_with_recovery(db_path.as_ref())?;
        Self::init_conn(conn)
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| GmError::Platform(format!("Failed to open in-memory queue DB: {e}")))?;
        Self::init_conn(conn)
    }

    fn init_conn(mut conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| GmError::Platform(e.to_string()))?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = FULL;
            CREATE TABLE IF NOT EXISTS runtime_events (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                event_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS bridge_deliveries (
                delivery_id TEXT PRIMARY KEY, received_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS task_attempts (
                id TEXT PRIMARY KEY, task_id TEXT NOT NULL, started_at TEXT NOT NULL,
                checkpoint TEXT
            );
            CREATE TABLE IF NOT EXISTS bot_tasks (
                id TEXT PRIMARY KEY,
                bot_id TEXT NOT NULL,
                channel_id TEXT,
                priority INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL,
                instruction TEXT NOT NULL,
                blocker_note TEXT,
                human_resume_note TEXT,
                result_summary TEXT,
                mutating_in_flight INTEGER NOT NULL DEFAULT 0,
                recovery_count INTEGER NOT NULL DEFAULT 0,
                handoff_depth INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_bot_tasks_bot_status
                ON bot_tasks(bot_id, status, priority DESC, created_at ASC);
            "#,
        )
        .map_err(|e| GmError::Platform(format!("Queue schema init error: {e}")))?;

        let migration = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| GmError::Platform(e.to_string()))?;
        let has_attempt = {
            let mut statement = migration
                .prepare("PRAGMA table_info(bot_tasks)")
                .map_err(|e| GmError::Platform(e.to_string()))?;
            let columns = statement
                .query_map([], |r| r.get::<_, String>(1))
                .map_err(|e| GmError::Platform(e.to_string()))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| GmError::Platform(e.to_string()))?;
            columns.iter().any(|s| s == "attempt_id")
        };
        if !has_attempt {
            migration
                .execute("ALTER TABLE bot_tasks ADD COLUMN attempt_id TEXT", [])
                .map_err(|e| GmError::Platform(e.to_string()))?;
        }
        migration
            .commit()
            .map_err(|e| GmError::Platform(e.to_string()))?;

        let queue = Self {
            conn: Arc::new(Mutex::new(conn)),
            max_recoveries: 2,
        };
        queue.init_routines()?;
        Ok(queue)
    }

    pub fn enqueue(
        &self,
        bot_id: &str,
        channel_id: Option<&str>,
        instruction: &str,
        priority: i32,
    ) -> Result<QueuedTask> {
        self.enqueue_with_depth(bot_id, channel_id, instruction, priority, 0)
    }

    pub fn enqueue_with_depth(
        &self,
        bot_id: &str,
        channel_id: Option<&str>,
        instruction: &str,
        priority: i32,
        handoff_depth: u8,
    ) -> Result<QueuedTask> {
        let id = format!("task-{}", uuid::Uuid::new_v4());
        let now = Utc::now().to_rfc3339();
        let task = QueuedTask {
            id: id.clone(),
            bot_id: bot_id.to_string(),
            channel_id: channel_id.map(|s| s.to_string()),
            priority,
            status: QueuedTaskStatus::Queued,
            instruction: instruction.to_string(),
            blocker_note: None,
            human_resume_note: None,
            result_summary: None,
            mutating_in_flight: false,
            recovery_count: 0,
            handoff_depth,
            created_at: now.clone(),
            updated_at: now,
            attempt_id: None,
        };

        self.conn
            .lock()
            .execute(
                r#"INSERT INTO bot_tasks (
                    id, bot_id, channel_id, priority, status, instruction,
                    blocker_note, human_resume_note, result_summary,
                    mutating_in_flight, recovery_count, handoff_depth,
                    created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"#,
                params![
                    task.id,
                    task.bot_id,
                    task.channel_id,
                    task.priority,
                    task.status.as_str(),
                    task.instruction,
                    task.blocker_note,
                    task.human_resume_note,
                    task.result_summary,
                    task.mutating_in_flight as i32,
                    task.recovery_count,
                    task.handoff_depth as i32,
                    task.created_at,
                    task.updated_at,
                ],
            )
            .map_err(|e| GmError::Platform(format!("Failed to enqueue task: {e}")))?;

        Ok(task)
    }

    /// Recover any tasks left in `running` state after an unclean daemon shutdown or reboot.
    /// Read-only tasks with `recovery_count < max_recoveries` return to `queued`;
    /// tasks with `mutating_in_flight == true` transition to `paused` so irreversible actions never double-fire.
    pub fn recover_after_crash(&self) -> Result<usize> {
        let now = Utc::now().to_rfc3339();
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| GmError::Platform(e.to_string()))?;

        let paused_count = tx
            .execute(
                r#"UPDATE bot_tasks
                   SET status = 'paused',
                       blocker_note = 'Paused automatically on crash recovery (mutating step was in flight)',
                       updated_at = ?1
                   WHERE status = 'running' AND (mutating_in_flight = 1 OR recovery_count >= ?2)"#,
                params![now, self.max_recoveries],
            )
            .map_err(|e| GmError::Platform(format!("Crash recovery pause update failed: {e}")))?;

        let requeued_count = tx
            .execute(
                r#"UPDATE bot_tasks
                   SET status = 'queued',
                       recovery_count = recovery_count + 1,
                       updated_at = ?1
                   WHERE status = 'running' AND mutating_in_flight = 0 AND recovery_count < ?2"#,
                params![now, self.max_recoveries],
            )
            .map_err(|e| GmError::Platform(format!("Crash recovery requeue failed: {e}")))?;

        tx.execute(
            "UPDATE bot_tasks SET status = 'paused' WHERE status = 'pause_requested'",
            [],
        )
        .map_err(|e| GmError::Platform(e.to_string()))?;
        tx.execute(
            "UPDATE bot_tasks SET status = 'cancelled' WHERE status = 'cancel_requested'",
            [],
        )
        .map_err(|e| GmError::Platform(e.to_string()))?;
        tx.commit().map_err(|e| GmError::Platform(e.to_string()))?;
        Ok(paused_count + requeued_count)
    }

    /// Atomically claim the next highest-priority FIFO `queued` task for `bot_id` (if the bot has no `running` task).
    pub fn claim_next_fifo(&self, bot_id: &str) -> Result<Option<QueuedTask>> {
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| GmError::Platform(e.to_string()))?;

        // Do not claim if this bot already has a running or waiting_for_human task
        let active_exists: Option<String> = tx
            .query_row(
                "SELECT id FROM bot_tasks WHERE bot_id = ?1 AND status IN ('running', 'waiting_for_human', 'pause_requested', 'cancel_requested') LIMIT 1",
                params![bot_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| GmError::Platform(format!("Failed to check active task: {e}")))?;

        if active_exists.is_some() {
            return Ok(None);
        }

        let next_id: Option<String> = tx
            .query_row(
                r#"SELECT id FROM bot_tasks
                   WHERE bot_id = ?1 AND status = 'queued'
                   ORDER BY priority DESC, created_at ASC, rowid ASC
                   LIMIT 1"#,
                params![bot_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| GmError::Platform(format!("Failed to select next FIFO task: {e}")))?;

        let Some(id) = next_id else {
            return Ok(None);
        };

        let now = Utc::now().to_rfc3339();
        let attempt_id = uuid::Uuid::new_v4().to_string();
        tx.execute(
            "UPDATE bot_tasks SET status = 'running', updated_at = ?1, attempt_id = ?3 WHERE id = ?2",
            params![now, id, attempt_id],
        )
        .map_err(|e| GmError::Platform(format!("Failed to mark task running: {e}")))?;

        tx.execute(
            "INSERT INTO task_attempts(id,task_id,started_at) VALUES (?1,?2,?3)",
            params![attempt_id, id, now],
        )
        .map_err(|e| GmError::Platform(e.to_string()))?;
        tx.commit().map_err(|e| GmError::Platform(e.to_string()))?;
        drop(conn);
        self.get_task(&id)
    }

    pub fn set_mutating_in_flight(&self, task_id: &str, in_flight: bool) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let rows = self.conn
            .lock()
            .execute(
                "UPDATE bot_tasks SET mutating_in_flight = ?1, updated_at = ?2 WHERE id = ?3 AND status IN ('running','pause_requested','cancel_requested') AND (?1 = 0 OR status = 'running')",
                params![in_flight as i32, now, task_id],
            )
            .map_err(|e| GmError::Platform(format!("Failed to update mutating_in_flight: {e}")))?;
        if rows == 0 {
            return Err(GmError::SafetyViolation(
                "Task is no longer running; action refused".into(),
            ));
        }
        Ok(())
    }

    pub fn pause(&self, task_id: &str) -> Result<bool> {
        self.request_control(task_id, "paused", "pause_requested")
    }

    pub fn resume(&self, task_id: &str) -> Result<bool> {
        if self
            .get_task(task_id)?
            .is_some_and(|t| t.mutating_in_flight)
        {
            return Err(GmError::SafetyViolation(
                "An uncertain action requires human review and a resume note".into(),
            ));
        }
        self.transition(task_id, "queued", None, "paused,waiting_for_human")
    }

    pub fn retry(&self, task_id: &str) -> Result<bool> {
        if self
            .get_task(task_id)?
            .is_some_and(|t| t.mutating_in_flight)
        {
            return Err(GmError::SafetyViolation(
                "Cannot retry an attempt with an uncertain external action".into(),
            ));
        }
        self.transition(task_id, "queued", None, "failed,cancelled")
    }

    pub fn cancel(&self, task_id: &str) -> Result<bool> {
        self.request_control(task_id, "cancelled", "cancel_requested")
    }

    pub fn mark_waiting_for_human(&self, task_id: &str, blocker: &str) -> Result<bool> {
        let now = Utc::now().to_rfc3339();
        let rows = self
            .conn
            .lock()
            .execute(
                "UPDATE bot_tasks SET status = 'waiting_for_human', blocker_note = ?1, updated_at = ?2 WHERE id = ?3 AND status = 'running'",
                params![blocker, now, task_id],
            )
            .map_err(|e| GmError::Platform(format!("Failed to mark waiting_for_human: {e}")))?;
        Ok(rows > 0)
    }

    pub fn resume_from_human(&self, task_id: &str, note: Option<&str>) -> Result<bool> {
        if self
            .get_task(task_id)?
            .is_some_and(|t| t.mutating_in_flight)
            && note.is_none_or(|n| n.trim().is_empty())
        {
            return Err(GmError::SafetyViolation(
                "Review the uncertain action and supply a resume note".into(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        let rows = self
            .conn
            .lock()
            .execute(
                "UPDATE bot_tasks SET status = 'queued', mutating_in_flight = 0, human_resume_note = ?1, updated_at = ?2 WHERE id = ?3 AND (status IN ('paused','waiting_for_human') OR (status IN ('failed','cancelled') AND mutating_in_flight=1))",
                params![note, now, task_id],
            )
            .map_err(|e| GmError::Platform(format!("Failed to resume_from_human: {e}")))?;
        Ok(rows > 0)
    }

    pub fn complete(&self, task_id: &str, summary: &str) -> Result<bool> {
        self.transition(task_id, "completed", Some(summary), "running")
    }

    pub fn fail(&self, task_id: &str, err_msg: &str) -> Result<bool> {
        self.transition(task_id, "failed", Some(err_msg), "running")
    }

    pub fn reassign(&self, task_id: &str, new_bot_id: &str) -> Result<bool> {
        let now = Utc::now().to_rfc3339();
        let rows = self
            .conn
            .lock()
            .execute(
                "UPDATE bot_tasks SET bot_id = ?1, status = 'queued', updated_at = ?2 WHERE id = ?3 AND status IN ('queued','paused')",
                params![new_bot_id, now, task_id],
            )
            .map_err(|e| GmError::Platform(format!("Failed to reassign task: {e}")))?;
        Ok(rows > 0)
    }

    fn request_control(&self, task_id: &str, stopped: &str, requested: &str) -> Result<bool> {
        self.conn.lock().execute(
            "UPDATE bot_tasks SET status = CASE WHEN status IN ('running','pause_requested') THEN ?1 ELSE ?2 END, updated_at = ?3 WHERE id = ?4 AND status IN ('queued','running','paused','waiting_for_human','pause_requested')",
            params![requested, stopped, Utc::now().to_rfc3339(), task_id],
        ).map(|rows| rows > 0).map_err(|e| GmError::Platform(e.to_string()))
    }

    pub fn acknowledge_control(&self, task_id: &str) -> Result<bool> {
        self.conn.lock().execute(
            "UPDATE bot_tasks SET status = CASE status WHEN 'pause_requested' THEN 'paused' ELSE 'cancelled' END, updated_at = ?1 WHERE id = ?2 AND status IN ('pause_requested','cancel_requested')",
            params![Utc::now().to_rfc3339(), task_id],
        ).map(|rows| rows > 0).map_err(|e| GmError::Platform(e.to_string()))
    }

    fn transition(
        &self,
        task_id: &str,
        status: &str,
        summary: Option<&str>,
        allowed: &str,
    ) -> Result<bool> {
        self.conn.lock().execute(
            "UPDATE bot_tasks SET status = ?1, result_summary = COALESCE(?2,result_summary), updated_at = ?3 WHERE id = ?4 AND instr(',' || ?5 || ',', ',' || status || ',') > 0",
            params![status, summary, Utc::now().to_rfc3339(), task_id, allowed],
        ).map(|rows| rows > 0).map_err(|e| GmError::Platform(e.to_string()))
    }

    pub fn store_checkpoint(&self, task_id: &str, history: &serde_json::Value) -> Result<()> {
        self.conn.lock().execute("UPDATE task_attempts SET checkpoint=?1 WHERE id=(SELECT attempt_id FROM bot_tasks WHERE id=?2)",params![history.to_string(),task_id]).map_err(|e|GmError::Platform(e.to_string()))?;
        Ok(())
    }
    pub fn load_checkpoint(&self, task_id: &str) -> Result<Vec<gm_agent::StepRecord>> {
        let value:Option<String>=self.conn.lock().query_row("SELECT checkpoint FROM task_attempts WHERE task_id=?1 AND checkpoint IS NOT NULL ORDER BY started_at DESC,rowid DESC LIMIT 1",params![task_id],|row|row.get(0)).optional().map_err(|e|GmError::Platform(e.to_string()))?;
        match value {
            Some(raw) => Ok(serde_json::from_str(&raw)?),
            None => Ok(Vec::new()),
        }
    }

    pub fn append_event(&self, event: &serde_json::Value) -> Result<u64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO runtime_events(event_json) VALUES (?1)",
            params![event.to_string()],
        )
        .map_err(|e| GmError::Platform(e.to_string()))?;
        Ok(conn.last_insert_rowid() as u64)
    }

    pub fn events_after(&self, cursor: u64, limit: usize) -> Result<Vec<(u64, serde_json::Value)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT sequence,event_json FROM runtime_events WHERE sequence > ?1 ORDER BY sequence LIMIT ?2")
            .map_err(|e| GmError::Platform(e.to_string()))?;
        let rows = stmt
            .query_map(params![cursor, limit.min(1000)], |row| {
                let raw: String = row.get(1)?;
                Ok((row.get(0)?, raw))
            })
            .map_err(|e| GmError::Platform(e.to_string()))?;
        rows.map(|row| {
            let (id, raw) = row.map_err(|e| GmError::Platform(e.to_string()))?;
            Ok((id, serde_json::from_str(&raw)?))
        })
        .collect()
    }

    pub fn claim_delivery(&self, id: &str) -> Result<bool> {
        self.conn
            .lock()
            .execute(
                "INSERT OR IGNORE INTO bridge_deliveries(delivery_id,received_at) VALUES (?1,?2)",
                params![id, Utc::now().timestamp()],
            )
            .map(|rows| rows > 0)
            .map_err(|e| GmError::Platform(e.to_string()))
    }

    pub fn get_task(&self, task_id: &str) -> Result<Option<QueuedTask>> {
        let conn = self.conn.lock();
        conn.query_row(
            r#"SELECT id, bot_id, channel_id, priority, status, instruction,
                      blocker_note, human_resume_note, result_summary,
                      mutating_in_flight, recovery_count, handoff_depth,
                      created_at, updated_at, attempt_id
               FROM bot_tasks WHERE id = ?1"#,
            params![task_id],
            Self::map_row,
        )
        .optional()
        .map_err(|e| GmError::Platform(format!("Failed to fetch task: {e}")))
    }

    pub fn has_queued(&self, bot_id: &str) -> Result<bool> {
        self.conn
            .lock()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM bot_tasks WHERE bot_id=?1 AND status='queued')",
                params![bot_id],
                |row| row.get(0),
            )
            .map_err(|e| GmError::Platform(e.to_string()))
    }

    pub fn list_tasks(&self, bot_id: Option<&str>) -> Result<Vec<QueuedTask>> {
        let conn = self.conn.lock();
        let mut out = Vec::new();
        if let Some(b) = bot_id {
            let mut stmt = conn
                .prepare(
                    r#"SELECT id, bot_id, channel_id, priority, status, instruction,
                              blocker_note, human_resume_note, result_summary,
                              mutating_in_flight, recovery_count, handoff_depth,
                              created_at, updated_at, attempt_id
                       FROM bot_tasks WHERE bot_id = ?1
                       ORDER BY priority DESC, created_at ASC, rowid ASC"#,
                )
                .map_err(|e| GmError::Platform(e.to_string()))?;
            let rows = stmt
                .query_map(params![b], Self::map_row)
                .map_err(|e| GmError::Platform(e.to_string()))?;
            for r in rows {
                out.push(r.map_err(|e| GmError::Platform(e.to_string()))?);
            }
        } else {
            let mut stmt = conn
                .prepare(
                    r#"SELECT id, bot_id, channel_id, priority, status, instruction,
                              blocker_note, human_resume_note, result_summary,
                              mutating_in_flight, recovery_count, handoff_depth,
                              created_at, updated_at, attempt_id
                       FROM bot_tasks
                       ORDER BY priority DESC, created_at ASC, rowid ASC"#,
                )
                .map_err(|e| GmError::Platform(e.to_string()))?;
            let rows = stmt
                .query_map([], Self::map_row)
                .map_err(|e| GmError::Platform(e.to_string()))?;
            for r in rows {
                out.push(r.map_err(|e| GmError::Platform(e.to_string()))?);
            }
        }
        Ok(out)
    }

    pub fn export_backup_jsonl(&self, path: impl AsRef<Path>) -> Result<usize> {
        let tasks = self.list_tasks(None)?;
        let mut f = std::fs::File::create(path.as_ref())?;
        for t in &tasks {
            let line = serde_json::to_string(t)?;
            writeln!(f, "{line}")?;
        }
        Ok(tasks.len())
    }

    pub fn import_backup_jsonl(&self, path: impl AsRef<Path>) -> Result<usize> {
        let f = std::fs::File::open(path.as_ref())?;
        let reader = std::io::BufReader::new(f);
        let lines = reader.lines().collect::<std::io::Result<Vec<_>>>()?;
        let mut tasks = Vec::new();
        for line in lines {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let mut t: QueuedTask = serde_json::from_str(trimmed)?;
            if matches!(
                t.status,
                QueuedTaskStatus::Running
                    | QueuedTaskStatus::PauseRequested
                    | QueuedTaskStatus::CancelRequested
            ) {
                t.status = QueuedTaskStatus::Paused;
                t.blocker_note = Some("Restored active attempt; review before resuming".into());
            }
            tasks.push(t);
        }
        let count = tasks.len();
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| GmError::Platform(e.to_string()))?;
        for t in tasks {
            tx.execute(
                r#"INSERT INTO bot_tasks (
                    id, bot_id, channel_id, priority, status, instruction,
                    blocker_note, human_resume_note, result_summary,
                    mutating_in_flight, recovery_count, handoff_depth,
                    created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"#,
                params![
                    t.id,
                    t.bot_id,
                    t.channel_id,
                    t.priority,
                    t.status.as_str(),
                    t.instruction,
                    t.blocker_note,
                    t.human_resume_note,
                    t.result_summary,
                    t.mutating_in_flight as i32,
                    t.recovery_count,
                    t.handoff_depth as i32,
                    t.created_at,
                    t.updated_at,
                ],
            )
            .map_err(|e| GmError::Platform(e.to_string()))?;
        }
        tx.commit().map_err(|e| GmError::Platform(e.to_string()))?;
        Ok(count)
    }

    fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<QueuedTask> {
        let status_str: String = row.get(4)?;
        let mutating_int: i32 = row.get(9)?;
        let depth_int: i32 = row.get(11)?;
        Ok(QueuedTask {
            id: row.get(0)?,
            bot_id: row.get(1)?,
            channel_id: row.get(2)?,
            priority: row.get(3)?,
            status: QueuedTaskStatus::from_db_str(&status_str).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?,
            instruction: row.get(5)?,
            blocker_note: row.get(6)?,
            human_resume_note: row.get(7)?,
            result_summary: row.get(8)?,
            mutating_in_flight: mutating_int != 0,
            recovery_count: row.get(10)?,
            handoff_depth: depth_int.clamp(0, 255) as u8,
            created_at: row.get(12)?,
            updated_at: row.get(13)?,
            attempt_id: row.get(14)?,
        })
    }
}

#[cfg(test)]
mod runtime_invariants {
    use super::*;
    #[test]
    fn cancellation_and_pause_require_worker_acknowledgement() {
        let queue = DurableTaskQueue::in_memory().unwrap();
        let task = queue.enqueue("coder", None, "work", 0).unwrap();
        queue.claim_next_fifo("coder").unwrap().unwrap();
        assert!(queue.pause(&task.id).unwrap());
        assert_eq!(
            queue.get_task(&task.id).unwrap().unwrap().status,
            QueuedTaskStatus::PauseRequested
        );
        assert!(!queue.resume(&task.id).unwrap());
        assert!(!queue.reassign(&task.id, "other").unwrap());
        assert!(queue.cancel(&task.id).unwrap());
        assert!(!queue.complete(&task.id, "too late").unwrap());
        queue.acknowledge_control(&task.id).unwrap();
        assert_eq!(
            queue.get_task(&task.id).unwrap().unwrap().status,
            QueuedTaskStatus::Cancelled
        );
        assert!(!queue.resume(&task.id).unwrap());
        assert!(queue.retry(&task.id).unwrap());
    }
    #[test]
    fn claims_are_atomic_across_independent_connections() {
        let dir = std::env::temp_dir().join(format!("gm-claim-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("queue.sqlite");
        let first = DurableTaskQueue::open(&path).unwrap();
        let second = DurableTaskQueue::open(&path).unwrap();
        first.enqueue("coder", None, "first", 0).unwrap();
        first.enqueue("coder", None, "second", 0).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let b = barrier.clone();
        let worker = std::thread::spawn(move || {
            b.wait();
            first.claim_next_fifo("coder").unwrap()
        });
        barrier.wait();
        let other = second.claim_next_fifo("coder").unwrap();
        assert_eq!(
            usize::from(worker.join().unwrap().is_some()) + usize::from(other.is_some()),
            1
        );
        drop(second);
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn corrupt_states_and_imports_fail_without_partial_writes() {
        let queue = DurableTaskQueue::in_memory().unwrap();
        let task = queue.enqueue("coder", None, "keep", 0).unwrap();
        queue
            .conn
            .lock()
            .execute("UPDATE bot_tasks SET status='future_unknown'", [])
            .unwrap();
        assert!(queue.get_task(&task.id).is_err());
        queue
            .conn
            .lock()
            .execute("UPDATE bot_tasks SET status='queued'", [])
            .unwrap();
        let file = std::env::temp_dir().join(format!("gm-import-{}", uuid::Uuid::new_v4()));
        let mut imported = task.clone();
        imported.id = "new-task".into();
        std::fs::write(
            &file,
            format!(
                "{}\ninvalid json",
                serde_json::to_string(&imported).unwrap()
            ),
        )
        .unwrap();
        assert!(queue.import_backup_jsonl(&file).is_err());
        assert!(queue.get_task("new-task").unwrap().is_none());
        assert_eq!(queue.list_tasks(None).unwrap().len(), 1);
        std::fs::remove_file(file).unwrap();
    }
    #[test]
    fn events_and_delivery_deduplication_survive_reopening() {
        let dir = std::env::temp_dir().join(format!("gm-events-{}", uuid::Uuid::new_v4()));
        let file = dir.join("db");
        let queue = DurableTaskQueue::open(&file).unwrap();
        assert!(queue.claim_delivery("slack:one").unwrap());
        assert!(!queue.claim_delivery("slack:one").unwrap());
        let id = queue
            .append_event(&serde_json::json!({"type":"RUN_STARTED"}))
            .unwrap();
        drop(queue);
        let queue = DurableTaskQueue::open(&file).unwrap();
        assert!(!queue.claim_delivery("slack:one").unwrap());
        assert_eq!(queue.events_after(0, 100).unwrap()[0].0, id);
        assert!(queue.events_after(id, 100).unwrap().is_empty());
        drop(queue);
        let _ = std::fs::remove_dir_all(dir);
    }
}
