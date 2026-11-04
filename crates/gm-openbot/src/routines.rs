//! Durable local-time schedules. Queue insertion and schedule advancement commit together.
use crate::{DurableTaskQueue, OpenBotSupervisor};
use chrono::{DateTime, Datelike, Days, LocalResult, NaiveTime, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use gm_core::error::{GmError, Result};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
fn error(e: impl std::fmt::Display) -> GmError {
    GmError::Config(format!("Routine: {e}"))
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuietHours {
    pub start_hour: u32,
    pub end_hour: u32,
}

impl QuietHours {
    pub fn is_quiet(&self, hour: u32) -> bool {
        if self.start_hour <= self.end_hour {
            hour >= self.start_hour && hour < self.end_hour
        } else {
            hour >= self.start_hour || hour < self.end_hour
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Routine {
    pub id: String,
    pub name: String,
    pub bot_id: String,
    pub channel_id: Option<String>,
    pub instruction: String,
    pub timezone: String,
    pub hour: u32,
    pub minute: u32,
    /// Monday=0 through Sunday=6. Empty means every day.
    pub weekdays: Vec<u32>,
    #[serde(default)]
    pub quiet_hours: Option<QuietHours>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutineRunRecord {
    pub id: String,
    pub routine_id: String,
    pub task_id: Option<String>,
    pub scheduled_for: String,
    pub fired_at: String,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct RoutineState {
    pub routine: Routine,
    pub next_run_utc: String,
    pub last_task_id: Option<String>,
}
impl Routine {
    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty()
            || self.id.len() > 64
            || !self
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            || self.name.trim().is_empty()
            || self.name.len() > 256
            || self.instruction.trim().is_empty()
            || self.instruction.len() > 64 * 1024
            || self.weekdays.len() > 7
            || self.weekdays.iter().any(|d| *d > 6)
            || self.hour > 23
            || self.minute > 59
        {
            return Err(error("Invalid schedule, identifier or text limits"));
        }
        if let Some(ref q) = self.quiet_hours
            && (q.start_hour > 23 || q.end_hour > 23)
        {
            return Err(error("Invalid quiet hours: must be 0-23"));
        }
        self.timezone.parse::<Tz>().map_err(error)?;
        Ok(())
    }
    /// DST policy: once on the earlier offset of a repeated time; a skipped
    /// time advances to the first valid local minute on that date.
    pub fn next_after(&self, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
        self.validate()?;
        let timezone = self.timezone.parse::<Tz>().map_err(error)?;
        let today = now.with_timezone(&timezone).date_naive();
        for offset in 0..15 {
            let date = today
                .checked_add_days(Days::new(offset))
                .ok_or_else(|| error("Date exceeds supported range"))?;
            if !self.weekdays.is_empty()
                && !self
                    .weekdays
                    .contains(&date.weekday().num_days_from_monday())
            {
                continue;
            }
            let requested = NaiveTime::from_hms_opt(self.hour, self.minute, 0)
                .ok_or_else(|| error("Invalid local time"))?;
            for minutes in 0..180 {
                let candidate = date.and_time(requested) + chrono::Duration::minutes(minutes);
                if candidate.date() != date {
                    break;
                }
                let candidate = match timezone.from_local_datetime(&candidate) {
                    LocalResult::Single(time) => Some(time),
                    LocalResult::Ambiguous(first, second) => Some(first.min(second)),
                    LocalResult::None => None,
                };
                if let Some(candidate) = candidate {
                    let candidate = candidate.with_timezone(&Utc);
                    if candidate > now {
                        return Ok(candidate);
                    }
                    break;
                }
            }
        }
        Err(error("No next occurrence within 15 days"))
    }
}
impl DurableTaskQueue {
    pub(crate) fn init_routines(&self) -> Result<()> {
        self.conn.lock().execute_batch("CREATE TABLE IF NOT EXISTS routines_v1 (
            id TEXT PRIMARY KEY, config_json TEXT NOT NULL, next_due TEXT NOT NULL, last_task_id TEXT);
            CREATE INDEX IF NOT EXISTS routines_due ON routines_v1(next_due);
            CREATE TABLE IF NOT EXISTS routine_runs_v1 (
            id TEXT PRIMARY KEY, routine_id TEXT NOT NULL, task_id TEXT, scheduled_for TEXT NOT NULL, fired_at TEXT NOT NULL, status TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS routine_runs_idx ON routine_runs_v1(routine_id, fired_at DESC);").map_err(error)
    }
    pub fn save_routine(&self, routine: &Routine, now: DateTime<Utc>) -> Result<()> {
        let next = routine.next_after(now)?.to_rfc3339();
        self.conn.lock().execute("INSERT INTO routines_v1(id,config_json,next_due) VALUES (?1,?2,?3)
            ON CONFLICT(id) DO UPDATE SET config_json=excluded.config_json,next_due=excluded.next_due",
            params![routine.id,serde_json::to_string(routine)?,next]).map_err(error)?;
        Ok(())
    }
    pub fn list_routines(&self) -> Result<Vec<RoutineState>> {
        let connection = self.conn.lock();
        let mut statement = connection
            .prepare("SELECT config_json,next_due,last_task_id FROM routines_v1 ORDER BY id")
            .map_err(error)?;
        let rows = statement
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(error)?;
        rows.map(|row| {
            let (json, next_run_utc, last_task_id) = row.map_err(error)?;
            Ok(RoutineState {
                routine: serde_json::from_str(&json)?,
                next_run_utc,
                last_task_id,
            })
        })
        .collect()
    }
    pub fn list_routine_runs(
        &self,
        routine_id: &str,
        limit: usize,
    ) -> Result<Vec<RoutineRunRecord>> {
        let connection = self.conn.lock();
        let mut statement = connection
            .prepare("SELECT id, routine_id, task_id, scheduled_for, fired_at, status FROM routine_runs_v1 WHERE routine_id=?1 ORDER BY fired_at DESC LIMIT ?2")
            .map_err(error)?;
        let rows = statement
            .query_map(params![routine_id, limit as i64], |r| {
                Ok(RoutineRunRecord {
                    id: r.get(0)?,
                    routine_id: r.get(1)?,
                    task_id: r.get(2)?,
                    scheduled_for: r.get(3)?,
                    fired_at: r.get(4)?,
                    status: r.get(5)?,
                })
            })
            .map_err(error)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(error)?);
        }
        Ok(out)
    }
    fn fire_routine(&self, routine: &Routine, now: DateTime<Utc>) -> Result<Option<String>> {
        let mut connection = self.conn.lock();
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(error)?;
        let current: Option<(String, String, Option<String>)> = tx
            .query_row(
                "SELECT config_json,next_due,last_task_id FROM routines_v1 WHERE id=?1",
                [&routine.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(error)?;
        let Some((json, due, last)) = current else {
            return Ok(None);
        };
        // Compare the reviewed configuration again under the transaction, so a
        // concurrent disable/edit cannot fire a stale schedule.
        if json != serde_json::to_string(routine)? || !routine.enabled {
            return Ok(None);
        }
        let due = DateTime::parse_from_rfc3339(&due)
            .map_err(error)?
            .with_timezone(&Utc);
        if due > now {
            return Ok(None);
        }
        let next = routine.next_after(now)?.to_rfc3339();

        if let Some(ref q) = routine.quiet_hours {
            let tz = routine.timezone.parse::<Tz>().map_err(error)?;
            let local_hour = now.with_timezone(&tz).hour();
            if q.is_quiet(local_hour) {
                let run_id = format!("run-quiet-{}-{}", routine.id, due.timestamp());
                let _ = tx.execute(
                    "INSERT OR REPLACE INTO routine_runs_v1(id,routine_id,task_id,scheduled_for,fired_at,status) VALUES (?1,?2,NULL,?3,?4,'skipped_quiet_hours')",
                    params![run_id, routine.id, due.to_rfc3339(), now.to_rfc3339()],
                );
                tx.execute(
                    "UPDATE routines_v1 SET next_due=?1 WHERE id=?2",
                    params![next, routine.id],
                )
                .map_err(error)?;
                tx.commit().map_err(error)?;
                return Ok(None);
            }
        }

        let active = if let Some(last) = last {
            tx.query_row("SELECT status NOT IN ('completed','failed','cancelled') FROM bot_tasks WHERE id=?1",[last],|r|r.get::<_,bool>(0)).optional().map_err(error)?.unwrap_or(false)
        } else {
            false
        };
        if active {
            let run_id = format!("run-overlap-{}-{}", routine.id, due.timestamp());
            let _ = tx.execute(
                "INSERT OR REPLACE INTO routine_runs_v1(id,routine_id,task_id,scheduled_for,fired_at,status) VALUES (?1,?2,NULL,?3,?4,'overlap_suppressed')",
                params![run_id, routine.id, due.to_rfc3339(), now.to_rfc3339()],
            );
            tx.execute(
                "UPDATE routines_v1 SET next_due=?1 WHERE id=?2",
                params![next, routine.id],
            )
            .map_err(error)?;
            tx.commit().map_err(error)?;
            return Ok(None);
        }
        let task_id = format!("routine-{}-{}", routine.id, due.timestamp());
        let timestamp = now.to_rfc3339();
        tx.execute("INSERT OR REPLACE INTO bot_tasks(id,bot_id,channel_id,status,instruction,created_at,updated_at) VALUES (?1,?2,?3,'queued',?4,?5,?5)",
            params![task_id,routine.bot_id,routine.channel_id,routine.instruction,timestamp]).map_err(error)?;
        let run_id = format!("run-{}-{}", routine.id, due.timestamp());
        let _ = tx.execute(
            "INSERT OR REPLACE INTO routine_runs_v1(id,routine_id,task_id,scheduled_for,fired_at,status) VALUES (?1,?2,?3,?4,?5,'queued')",
            params![run_id, routine.id, task_id, due.to_rfc3339(), timestamp],
        );
        tx.execute(
            "UPDATE routines_v1 SET next_due=?1,last_task_id=?2 WHERE id=?3",
            params![next, task_id, routine.id],
        )
        .map_err(error)?;
        tx.commit().map_err(error)?;
        Ok(Some(task_id))
    }
}
impl OpenBotSupervisor {
    pub fn save_routine(&self, routine: &Routine) -> Result<()> {
        routine.validate()?;
        if self.roster().get_bot(&routine.bot_id).is_none() {
            return Err(error("Unknown bot"));
        }
        if let Some(channel) = &routine.channel_id
            && self.channels().get_channel(channel)?.is_none()
        {
            return Err(error("Unknown channel"));
        }

        self.queue().save_routine(routine, Utc::now())
    }
    pub(crate) fn tick_routines(&self, now: DateTime<Utc>) -> Result<usize> {
        let mut count = 0;
        for state in self.queue().list_routines()? {
            let routine = state.routine;
            if !routine.enabled || self.roster().get_bot(&routine.bot_id).is_none() {
                continue;
            }
            if let Some(channel) = &routine.channel_id {
                match self.channels().get_channel(channel)? {
                    Some(c) if !c.paused && !c.archived => {}
                    _ => continue,
                }
            }
            if let Some(id) = self.queue().fire_routine(&routine, now)? {
                count += 1;
                self.emit_agui(
                    "ROUTINE_QUEUED",
                    id,
                    &routine.bot_id,
                    serde_json::json!({"routine_id":routine.id}),
                );
            }
        }
        Ok(count)
    }
    pub fn list_routine_runs(
        &self,
        routine_id: &str,
        limit: usize,
    ) -> Result<Vec<RoutineRunRecord>> {
        self.queue().list_routine_runs(routine_id, limit)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn daily() -> Routine {
        Routine {
            id: "daily".into(),
            name: "Daily report".into(),
            bot_id: "researcher".into(),
            channel_id: None,
            instruction: "Prepare report".into(),
            timezone: "America/New_York".into(),
            hour: 2,
            minute: 30,
            weekdays: vec![],
            quiet_hours: None,
            enabled: true,
        }
    }
    fn time(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }
    #[test]
    fn dst_gap_and_fold_fire_once_with_an_explicit_policy() {
        let mut routine = daily();
        assert_eq!(
            routine.next_after(time("2026-03-08T05:00:00Z")).unwrap(),
            time("2026-03-08T07:00:00Z")
        );
        routine.hour = 1;
        routine.minute = 30;
        assert_eq!(
            routine.next_after(time("2026-11-01T04:00:00Z")).unwrap(),
            time("2026-11-01T05:30:00Z")
        );
        assert_eq!(
            routine.next_after(time("2026-11-01T05:31:00Z")).unwrap(),
            time("2026-11-02T06:30:00Z")
        );
        routine.timezone = "Invalid/Zone".into();
        assert!(routine.validate().is_err());
    }
    #[test]
    fn restart_and_parallel_connections_enqueue_once_and_suppress_overlap() {
        let path = std::env::temp_dir().join(format!("gm-routine-{}.sqlite", uuid::Uuid::new_v4()));
        let first = DurableTaskQueue::open(&path).unwrap();
        let second = DurableTaskQueue::open(&path).unwrap();
        let routine = daily();
        first
            .save_routine(&routine, time("2026-11-04T00:00:00Z"))
            .unwrap();
        let now = time("2026-11-07T12:00:00Z");
        let task = first.fire_routine(&routine, now).unwrap().unwrap();
        assert!(second.fire_routine(&routine, now).unwrap().is_none());
        assert!(
            second
                .fire_routine(&routine, time("2026-11-08T12:00:00Z"))
                .unwrap()
                .is_none()
        );
        assert_eq!(second.list_tasks(None).unwrap().len(), 1);
        first.cancel(&task).unwrap();
        assert!(
            second
                .fire_routine(&routine, time("2026-11-09T12:00:00Z"))
                .unwrap()
                .is_some()
        );
        let mut disabled = routine.clone();
        disabled.enabled = false;
        first
            .save_routine(&disabled, time("2026-11-09T12:00:00Z"))
            .unwrap();
        assert!(
            second
                .fire_routine(&routine, time("2026-11-10T12:00:00Z"))
                .unwrap()
                .is_none()
        );
        drop(first);
        drop(second);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_routine_quiet_hours_and_run_history_tracking() {
        let path =
            std::env::temp_dir().join(format!("gm-routine-quiet-{}.sqlite", uuid::Uuid::new_v4()));
        let queue = DurableTaskQueue::open(&path).unwrap();
        let mut routine = daily();
        routine.quiet_hours = Some(QuietHours {
            start_hour: 22,
            end_hour: 6, // 10 PM to 6 AM is quiet
        });
        queue
            .save_routine(&routine, time("2026-11-04T00:00:00Z"))
            .unwrap();

        // 2:30 AM EST is 07:30:00Z, which is within quiet hours (22 -> 6)
        let quiet_time = time("2026-11-04T07:30:00Z");
        let result = queue.fire_routine(&routine, quiet_time).unwrap();
        assert!(result.is_none());

        let runs = queue.list_routine_runs(&routine.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "skipped_quiet_hours");

        // Now disable quiet hours
        routine.quiet_hours = None;
        queue
            .save_routine(&routine, time("2026-11-04T08:00:00Z"))
            .unwrap();

        let fire_time = time("2026-11-05T08:00:00Z");
        let task = queue.fire_routine(&routine, fire_time).unwrap();
        assert!(task.is_some());

        let runs2 = queue.list_routine_runs(&routine.id, 10).unwrap();
        assert_eq!(runs2.len(), 2);
        assert_eq!(runs2[0].status, "queued");

        drop(queue);
        let _ = std::fs::remove_file(path);
    }
}
