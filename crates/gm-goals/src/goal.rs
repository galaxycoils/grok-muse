use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GoalStatus {
    Active,
    Paused,
    Completed,
    Abandoned,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: GoalStatus,
    pub stop_conditions: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub next_check_at: DateTime<Utc>,
    pub verified_close: bool,
}

impl Goal {
    pub fn new(title: impl Into<String>, description: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            title: title.into(),
            description: description.into(),
            status: GoalStatus::Active,
            stop_conditions: Vec::new(),
            created_at: now,
            next_check_at: now + chrono::Duration::hours(24),
            verified_close: false,
        }
    }

    pub fn complete(&mut self, verified: bool) {
        self.status = GoalStatus::Completed;
        self.verified_close = verified;
    }
}
