use crate::notice::ProactiveNotice;
use chrono::{DateTime, Utc};
use gm_core::error::Result;
use gm_memory::PersonalMemoryStore;
use gm_safety::AuditLogger;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FeedCategory {
    Observation,
    PatrolInsight,
    TaskCompleted,
}

impl FeedCategory {
    pub fn badge(&self) -> &'static str {
        match self {
            FeedCategory::Observation => "📝 [Observation]",
            FeedCategory::PatrolInsight => "📬 [Patrol Insight]",
            FeedCategory::TaskCompleted => "⚡ [Task Completed]",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedItem {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    pub category: FeedCategory,
    pub title: String,
    pub summary: String,
    pub details: serde_json::Value,
}

pub fn build_feed(
    memory: &PersonalMemoryStore,
    notices: &[ProactiveNotice],
    audit: &AuditLogger,
    limit: usize,
) -> Result<Vec<FeedItem>> {
    let mut feed = Vec::new();

    // 1. Observations from memory
    if let Ok(records) = memory.all_records(limit * 2) {
        for r in records {
            let ts = DateTime::from_timestamp_millis(r.created_at).unwrap_or_else(Utc::now);
            feed.push(FeedItem {
                id: format!("obs_{}", r.id),
                timestamp: ts,
                category: FeedCategory::Observation,
                title: r.title.unwrap_or_else(|| "Personal Note".into()),
                summary: r.body,
                details: serde_json::json!({
                    "kind": format!("{:?}", r.kind),
                    "tags": r.tags,
                    "importance": r.importance,
                    "source": r.source,
                }),
            });
        }
    }

    // 2. Patrol insights from proactive notices
    for n in notices {
        feed.push(FeedItem {
            id: format!("notice_{}", n.id),
            timestamp: n.sent_at,
            category: FeedCategory::PatrolInsight,
            title: n.title.clone(),
            summary: n.body.clone(),
            details: serde_json::json!({
                "topic": n.topic,
                "urgency": n.is_urgent,
                "rating": n.useful_rating,
            }),
        });
    }

    // 3. Completed tasks from audit log
    for entry in audit.entries() {
        if entry.event_type == "task_done" {
            let ts = chrono::DateTime::parse_from_rfc3339(&entry.timestamp)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());

            let title = entry
                .action_summary
                .clone()
                .unwrap_or_else(|| "Autonomous Task".into());
            let instruction = entry
                .details
                .get("instruction")
                .and_then(|v| v.as_str())
                .unwrap_or("Executed computer-use task");

            feed.push(FeedItem {
                id: format!("task_{}", entry.timestamp),
                timestamp: ts,
                category: FeedCategory::TaskCompleted,
                title,
                summary: instruction.to_string(),
                details: entry.details.clone(),
            });
        }
    }

    // Sort descending by timestamp (newest first)
    feed.sort_by_key(|a| std::cmp::Reverse(a.timestamp));
    feed.truncate(limit);

    Ok(feed)
}

pub fn format_feed(items: &[FeedItem]) -> String {
    let mut out = String::new();
    out.push_str("═══════════════════════════════════════════════════════════\n");
    out.push_str("📡 Grok-Muse Proactive Live Feed\n");
    out.push_str("═══════════════════════════════════════════════════════════\n\n");

    if items.is_empty() {
        out.push_str("No recent observations, insights, or tasks recorded.\n");
    } else {
        for item in items {
            let time_str = item.timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string();
            out.push_str(&format!(
                "[{}] {} {}\n",
                time_str,
                item.category.badge(),
                item.title
            ));
            out.push_str(&format!("  • Details: {}\n\n", item.summary));
        }
    }

    out.push_str("═══════════════════════════════════════════════════════════\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feed_building_and_formatting() {
        let memory = PersonalMemoryStore::in_memory().unwrap();
        memory
            .record_fact(
                Some("Morning preference"),
                "User starts work at 9:00 AM",
                &["preferences"],
            )
            .unwrap();

        let mut notice = ProactiveNotice::new(
            "Upcoming Meeting: Sync",
            "Meeting starts in 10 minutes",
            "calendar",
            true,
        );
        notice.rate_useful(true);
        let notices = vec![notice];

        let audit = AuditLogger::in_memory();
        audit.log(
            "task_done",
            None,
            None,
            None,
            None,
            Some("Downloads folder cleanup".into()),
            serde_json::json!({ "instruction": "Clean up files" }),
        );

        let feed = build_feed(&memory, &notices, &audit, 10).unwrap();
        assert_eq!(feed.len(), 3);

        // Verify categories are present
        assert!(feed.iter().any(|i| i.category == FeedCategory::Observation));
        assert!(
            feed.iter()
                .any(|i| i.category == FeedCategory::PatrolInsight)
        );
        assert!(
            feed.iter()
                .any(|i| i.category == FeedCategory::TaskCompleted)
        );

        let formatted = format_feed(&feed);
        assert!(formatted.contains("Grok-Muse Proactive Live Feed"));
        assert!(formatted.contains("[Observation]"));
        assert!(formatted.contains("[Patrol Insight]"));
        assert!(formatted.contains("[Task Completed]"));
        assert!(formatted.contains("Downloads folder cleanup"));
    }
}
