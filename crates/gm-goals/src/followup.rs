use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FollowUpStatus {
    Pending,
    Done,
    Later,
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FollowUp {
    pub id: String,
    pub goal_id: Option<String>,
    pub prompt: String,
    pub due_at: DateTime<Utc>,
    pub status: FollowUpStatus,
    pub snooze_count: u32,
}

impl FollowUp {
    pub fn new(prompt: impl Into<String>, due_at: DateTime<Utc>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            goal_id: None,
            prompt: prompt.into(),
            due_at,
            status: FollowUpStatus::Pending,
            snooze_count: 0,
        }
    }

    pub fn snooze(&mut self, duration: chrono::Duration) {
        self.due_at = Utc::now() + duration;
        self.snooze_count += 1;
        self.status = FollowUpStatus::Later;
    }

    pub fn mark_done(&mut self) {
        self.status = FollowUpStatus::Done;
    }

    pub fn stop(&mut self) {
        self.status = FollowUpStatus::Stopped;
    }
}
