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
            QueuedTaskStatus::Paused => "paused",
            QueuedTaskStatus::WaitingForHuman => "waiting_for_human",
            QueuedTaskStatus::Completed => "completed",
            QueuedTaskStatus::Failed => "failed",
            QueuedTaskStatus::Cancelled => "cancelled",
        }
    }

    pub fn from_db_str(s: &str) -> Self {
        match s {
            "queued" => QueuedTaskStatus::Queued,
            "running" => QueuedTaskStatus::Running,
            "paused" => QueuedTaskStatus::Paused,
            "waiting_for_human" => QueuedTaskStatus::WaitingForHuman,
            "completed" => QueuedTaskStatus::Completed,
            "failed" => QueuedTaskStatus::Failed,
            "cancelled" => QueuedTaskStatus::Cancelled,
            _ => QueuedTaskStatus::Queued,
        }
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
}

/// Crash-safe SQLite WAL FIFO task queue supporting per-bot backlogs, pause/resume/cancel/reassign,
/// "Take the Wheel" human handoff states, reboot recovery, and JSONL backup/restore.
#[derive(Clone)]
pub struct DurableTaskQueue {
    conn: Arc<Mutex<Connection>>,
    max_recoveries: u32,
}

impl DurableTaskQueue {
    pub fn open(db_path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = db_path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(db_path.as_ref())
            .map_err(|e| GmError::Platform(format!("Failed to open queue SQLite DB: {e}")))?;
        Self::init_conn(conn)
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| GmError::Platform(format!("Failed to open in-memory queue DB: {e}")))?;
        Self::init_conn(conn)
    }

    fn init_conn(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
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

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            max_recoveries: 2,
        })
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
        let conn = self.conn.lock();

        let paused_count = conn
            .execute(
                r#"UPDATE bot_tasks
                   SET status = 'paused',
                       blocker_note = 'Paused automatically on crash recovery (mutating step was in flight)',
                       updated_at = ?1
                   WHERE status = 'running' AND (mutating_in_flight = 1 OR recovery_count >= ?2)"#,
                params![now, self.max_recoveries],
            )
            .map_err(|e| GmError::Platform(format!("Crash recovery pause update failed: {e}")))?;

        let requeued_count = conn
            .execute(
                r#"UPDATE bot_tasks
                   SET status = 'queued',
                       recovery_count = recovery_count + 1,
                       updated_at = ?1
                   WHERE status = 'running' AND mutating_in_flight = 0 AND recovery_count < ?2"#,
                params![now, self.max_recoveries],
            )
            .map_err(|e| GmError::Platform(format!("Crash recovery requeue failed: {e}")))?;

        Ok(paused_count + requeued_count)
    }

    /// Atomically claim the next highest-priority FIFO `queued` task for `bot_id` (if the bot has no `running` task).
    pub fn claim_next_fifo(&self, bot_id: &str) -> Result<Option<QueuedTask>> {
        let conn = self.conn.lock();

        // Do not claim if this bot already has a running or waiting_for_human task
        let active_exists: Option<String> = conn
            .query_row(
                "SELECT id FROM bot_tasks WHERE bot_id = ?1 AND status IN ('running', 'waiting_for_human') LIMIT 1",
                params![bot_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| GmError::Platform(format!("Failed to check active task: {e}")))?;

        if active_exists.is_some() {
            return Ok(None);
        }

        let next_id: Option<String> = conn
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
        conn.execute(
            "UPDATE bot_tasks SET status = 'running', updated_at = ?1 WHERE id = ?2",
            params![now, id],
        )
        .map_err(|e| GmError::Platform(format!("Failed to mark task running: {e}")))?;

        drop(conn);
        self.get_task(&id)
    }

    pub fn set_mutating_in_flight(&self, task_id: &str, in_flight: bool) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn
            .lock()
            .execute(
                "UPDATE bot_tasks SET mutating_in_flight = ?1, updated_at = ?2 WHERE id = ?3",
                params![in_flight as i32, now, task_id],
            )
            .map_err(|e| GmError::Platform(format!("Failed to update mutating_in_flight: {e}")))?;
        Ok(())
    }

    pub fn pause(&self, task_id: &str) -> Result<bool> {
        self.update_status(task_id, QueuedTaskStatus::Paused, None)
    }

    pub fn resume(&self, task_id: &str) -> Result<bool> {
        self.update_status(task_id, QueuedTaskStatus::Queued, None)
    }

    pub fn cancel(&self, task_id: &str) -> Result<bool> {
        self.update_status(task_id, QueuedTaskStatus::Cancelled, None)
    }

    pub fn mark_waiting_for_human(&self, task_id: &str, blocker: &str) -> Result<bool> {
        let now = Utc::now().to_rfc3339();
        let rows = self
            .conn
            .lock()
            .execute(
                "UPDATE bot_tasks SET status = 'waiting_for_human', blocker_note = ?1, updated_at = ?2 WHERE id = ?3",
                params![blocker, now, task_id],
            )
            .map_err(|e| GmError::Platform(format!("Failed to mark waiting_for_human: {e}")))?;
        Ok(rows > 0)
    }

    pub fn resume_from_human(&self, task_id: &str, note: Option<&str>) -> Result<bool> {
        let now = Utc::now().to_rfc3339();
        let rows = self
            .conn
            .lock()
            .execute(
                "UPDATE bot_tasks SET status = 'queued', human_resume_note = ?1, updated_at = ?2 WHERE id = ?3",
                params![note, now, task_id],
            )
            .map_err(|e| GmError::Platform(format!("Failed to resume_from_human: {e}")))?;
        Ok(rows > 0)
    }

    pub fn complete(&self, task_id: &str, summary: &str) -> Result<bool> {
        self.update_status(task_id, QueuedTaskStatus::Completed, Some(summary))
    }

    pub fn fail(&self, task_id: &str, err_msg: &str) -> Result<bool> {
        self.update_status(task_id, QueuedTaskStatus::Failed, Some(err_msg))
    }

    pub fn reassign(&self, task_id: &str, new_bot_id: &str) -> Result<bool> {
        let now = Utc::now().to_rfc3339();
        let rows = self
            .conn
            .lock()
            .execute(
                "UPDATE bot_tasks SET bot_id = ?1, status = 'queued', updated_at = ?2 WHERE id = ?3",
                params![new_bot_id, now, task_id],
            )
            .map_err(|e| GmError::Platform(format!("Failed to reassign task: {e}")))?;
        Ok(rows > 0)
    }

    fn update_status(
        &self,
        task_id: &str,
        status: QueuedTaskStatus,
        summary: Option<&str>,
    ) -> Result<bool> {
        let now = Utc::now().to_rfc3339();
        let rows = self
            .conn
            .lock()
            .execute(
                "UPDATE bot_tasks SET status = ?1, result_summary = COALESCE(?2, result_summary), mutating_in_flight = 0, updated_at = ?3 WHERE id = ?4",
                params![status.as_str(), summary, now, task_id],
            )
            .map_err(|e| GmError::Platform(format!("Failed to update task status: {e}")))?;
        Ok(rows > 0)
    }

    pub fn get_task(&self, task_id: &str) -> Result<Option<QueuedTask>> {
        let conn = self.conn.lock();
        conn.query_row(
            r#"SELECT id, bot_id, channel_id, priority, status, instruction,
                      blocker_note, human_resume_note, result_summary,
                      mutating_in_flight, recovery_count, handoff_depth,
                      created_at, updated_at
               FROM bot_tasks WHERE id = ?1"#,
            params![task_id],
            Self::map_row,
        )
        .optional()
        .map_err(|e| GmError::Platform(format!("Failed to fetch task: {e}")))
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
                              created_at, updated_at
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
                              created_at, updated_at
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
        let mut count = 0;
        let conn = self.conn.lock();
        for line in reader.lines().map_while(std::result::Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let t: QueuedTask = serde_json::from_str(trimmed)?;
            conn.execute(
                r#"INSERT OR REPLACE INTO bot_tasks (
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
            count += 1;
        }
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
            status: QueuedTaskStatus::from_db_str(&status_str),
            instruction: row.get(5)?,
            blocker_note: row.get(6)?,
            human_resume_note: row.get(7)?,
            result_summary: row.get(8)?,
            mutating_in_flight: mutating_int != 0,
            recovery_count: row.get(10)?,
            handoff_depth: depth_int.clamp(0, 255) as u8,
            created_at: row.get(12)?,
            updated_at: row.get(13)?,
        })
    }
}
