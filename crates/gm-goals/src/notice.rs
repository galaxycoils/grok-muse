use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProactiveNotice {
    pub id: String,
    pub title: String,
    pub body: String,
    pub topic: String,
    pub sent_at: DateTime<Utc>,
    pub is_urgent: bool,
    /// Dogfood evaluation rating: None = unrated, Some(true) = Useful, Some(false) = Not Useful
    pub useful_rating: Option<bool>,
}

impl ProactiveNotice {
    pub fn new(
        title: impl Into<String>,
        body: impl Into<String>,
        topic: impl Into<String>,
        is_urgent: bool,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            title: title.into(),
            body: body.into(),
            topic: topic.into(),
            sent_at: Utc::now(),
            is_urgent,
            useful_rating: None,
        }
    }

    pub fn rate_useful(&mut self, useful: bool) {
        self.useful_rating = Some(useful);
    }
}
