use crate::dogfood::DogfoodMetricsSummary;
use chrono::{DateTime, Utc};
use gm_core::error::Result;
use gm_core::types::{CalendarItem, ReminderItem};
use gm_memory::PersonalMemoryStore;
use gm_platform::Platform;
use gm_safety::AuditLogger;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NightlyReviewSummary {
    pub date: String,
    pub tasks_completed: Vec<String>,
    pub proactive_notices_count: u32,
    pub useful_feedback_count: u32,
    pub interruption_precision_pct: f64,
    pub total_spend_usd: f64,
    pub total_tokens: u32,
    pub memories_formed: Vec<String>,
    pub upcoming_commitments: Vec<String>,
    pub pending_reminders: Vec<String>,
    pub suggested_priorities: Vec<String>,
    pub review_markdown: String,
}

pub fn synthesize_nightly_review(
    date: &str,
    audit_entries: &[gm_safety::AuditEntry],
    memories: &[gm_memory::Record],
    dogfood: Option<&DogfoodMetricsSummary>,
    calendar_events_tomorrow: &[CalendarItem],
    reminders: &[ReminderItem],
) -> NightlyReviewSummary {
    let mut tasks_completed = Vec::new();
    let mut total_spend_usd = 0.0;
    let mut total_tokens = 0u32;

    for entry in audit_entries {
        if entry.timestamp.starts_with(date) {
            if let Some(cost) = entry.cost_usd {
                total_spend_usd += cost;
            }
            if let Some(tok) = entry.tokens {
                total_tokens += tok;
            }
            if entry.event_type == "task_done" {
                if let Some(ref summary) = entry.action_summary {
                    tasks_completed.push(summary.clone());
                } else if let Some(inst) = entry.details.get("instruction").and_then(|v| v.as_str())
                {
                    tasks_completed.push(inst.to_string());
                }
            }
        }
    }

    let proactive_spend = dogfood.map(|df| df.total_spend_usd).unwrap_or(0.0);
    let total_spend_usd = total_spend_usd + proactive_spend;

    let mut memories_formed = Vec::new();
    for mem in memories {
        // Collect facts or preferences, excluding recursive nightly review summaries
        if mem.tags.iter().any(|t| t == "nightly_review") {
            continue;
        }
        let text = if let Some(ref title) = mem.title {
            format!("{}: {}", title, mem.body)
        } else {
            mem.body.clone()
        };
        memories_formed.push(text);
    }

    let mut upcoming_commitments = Vec::new();
    for ev in calendar_events_tomorrow {
        let loc = ev
            .location
            .as_deref()
            .map(|l| format!(" ({l})"))
            .unwrap_or_default();
        upcoming_commitments.push(format!("Event: {}{loc}", ev.title));
    }

    let mut pending_reminders = Vec::new();
    for rem in reminders {
        if !rem.is_completed {
            let list_str = rem
                .list_name
                .as_deref()
                .map(|l| format!(" [{l}]"))
                .unwrap_or_default();
            pending_reminders.push(format!("Reminder: {}{list_str}", rem.title));
            if pending_reminders.len() >= 5 {
                break;
            }
        }
    }

    let (proactive_notices_count, useful_feedback_count, interruption_precision_pct) =
        if let Some(df) = dogfood {
            (
                df.total_notices_delivered,
                df.useful_ratings_count,
                df.interruption_precision_pct,
            )
        } else {
            (0, 0, 100.0)
        };

    let mut suggested_priorities = Vec::new();
    if !upcoming_commitments.is_empty() {
        suggested_priorities.push(format!(
            "Prepare materials and agenda for tomorrow's commitments ({})",
            upcoming_commitments[0]
        ));
    }
    if !pending_reminders.is_empty() {
        suggested_priorities.push(format!(
            "Address pending high-priority task: {}",
            pending_reminders[0]
        ));
    }
    if suggested_priorities.is_empty() {
        suggested_priorities
            .push("Review ongoing strategic goals and schedule quiet focus time.".into());
    }

    // Markdown synthesis
    let mut md = String::new();
    md.push_str(&format!("# 🌙 Grok-Muse Nightly Review — {}\n\n", date));

    md.push_str("## 🎯 Accomplishments & Automated Tasks\n");
    if tasks_completed.is_empty() {
        md.push_str("- No automated computer-use tasks executed today.\n");
    } else {
        for t in &tasks_completed {
            md.push_str(&format!("- ✔ {}\n", t));
        }
    }
    md.push('\n');

    md.push_str("## 🔔 Proactive Interruption Performance\n");
    md.push_str(&format!(
        "- **Delivered Notices:** {} notices\n",
        proactive_notices_count
    ));
    md.push_str(&format!(
        "- **Interruption Precision:** {:.1}% ({} useful ratings)\n",
        interruption_precision_pct, useful_feedback_count
    ));
    md.push_str(&format!(
        "- **Resource Spend:** ${:.4} USD ({} tokens)\n\n",
        total_spend_usd, total_tokens
    ));

    md.push_str("## 🧠 Memories & Facts Learned\n");
    if memories_formed.is_empty() {
        md.push_str("- No new personal memories recorded today.\n");
    } else {
        for m in &memories_formed {
            md.push_str(&format!("- • {}\n", m));
        }
    }
    md.push('\n');

    md.push_str("## 📅 Tomorrow's Horizon & Commitments\n");
    if upcoming_commitments.is_empty() && pending_reminders.is_empty() {
        md.push_str("- Schedule is clear for tomorrow.\n");
    } else {
        for ev in &upcoming_commitments {
            md.push_str(&format!("- 🗓 {}\n", ev));
        }
        for rem in &pending_reminders {
            md.push_str(&format!("- 📌 {}\n", rem));
        }
    }
    md.push('\n');

    md.push_str("## 💡 Suggested Priorities for Tomorrow\n");
    for prio in &suggested_priorities {
        md.push_str(&format!("- 🚀 {}\n", prio));
    }
    md.push('\n');

    NightlyReviewSummary {
        date: date.to_string(),
        tasks_completed,
        proactive_notices_count,
        useful_feedback_count,
        interruption_precision_pct,
        total_spend_usd,
        total_tokens,
        memories_formed,
        upcoming_commitments,
        pending_reminders,
        suggested_priorities,
        review_markdown: md,
    }
}

pub async fn run_nightly_review<P: Platform>(
    platform: Option<&P>,
    audit: &AuditLogger,
    memory: &PersonalMemoryStore,
    dogfood: Option<&DogfoodMetricsSummary>,
    now: DateTime<Utc>,
) -> Result<NightlyReviewSummary> {
    let date = now.format("%Y-%m-%d").to_string();
    let audit_entries = audit.entries();

    let memories = memory.search("", 20).unwrap_or_default();

    let mut events_tomorrow = Vec::new();
    let mut reminders = Vec::new();

    if let Some(p) = platform {
        let tomorrow_date = (now + chrono::Duration::days(1)).date_naive();
        let tomorrow_start = tomorrow_date
            .and_hms_opt(0, 0, 0)
            .unwrap_or_default()
            .and_utc();
        let tomorrow_end = tomorrow_date
            .and_hms_opt(23, 59, 59)
            .unwrap_or_default()
            .and_utc();
        let start_ms = tomorrow_start.timestamp_millis() as u64;
        let end_ms = tomorrow_end.timestamp_millis() as u64;

        if let Ok(evs) = p.get_calendar_events(start_ms, end_ms).await {
            events_tomorrow = evs;
        }
        if let Ok(rems) = p.get_reminders(false).await {
            reminders = rems;
        }
    }

    let summary = synthesize_nightly_review(
        &date,
        &audit_entries,
        &memories,
        dogfood,
        &events_tomorrow,
        &reminders,
    );

    // Persist summary into memory store as review fact
    let _ = memory.record_fact(
        Some(&format!("Nightly Review {date}")),
        &summary.review_markdown,
        &["nightly_review", "summary", &date],
    );

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use gm_platform::MockPlatform;

    #[tokio::test]
    async fn test_nightly_review_synthesis() {
        let audit = AuditLogger::in_memory();
        audit.log(
            "task_done",
            Some("meta".into()),
            Some("muse-spark".into()),
            Some(250),
            Some(0.005),
            Some("Created quarterly summary in Notes".into()),
            serde_json::json!({ "instruction": "Open Notes and summarize Q3" }),
        );

        let memory = PersonalMemoryStore::in_memory().unwrap();
        memory
            .record_fact(
                Some("Preference"),
                "User works best before 2 PM",
                &["preferences"],
            )
            .unwrap();

        let mock_platform = MockPlatform::new();
        let now = Utc.with_ymd_and_hms(2026, 11, 3, 21, 0, 0).unwrap();
        let now_ms = now.timestamp_millis() as u64;

        mock_platform.set_calendar_events(vec![CalendarItem {
            id: "cal_tomorrow".into(),
            title: "Sprint Planning".into(),
            start_time_ms: now_ms + (12 * 3600 * 1000),
            end_time_ms: now_ms + (13 * 3600 * 1000),
            location: Some("Zoom".into()),
            notes: None,
            is_all_day: false,
        }]);

        let dogfood = DogfoodMetricsSummary {
            total_notices_delivered: 2,
            total_rated_notices: 2,
            useful_ratings_count: 2,
            not_useful_ratings_count: 0,
            interruption_precision_pct: 100.0,
            active_days_count: 1,
            avg_notices_per_day: 2.0,
            total_spend_usd: 0.002,
            daily_spend_cap_usd: 1.0,
            max_daily_spend_usd: 0.002,
            spend_cap_enforced: true,
            g6_gate_passed: true,
            feedback_done_count: 1,
            feedback_later_count: 1,
            feedback_stop_count: 0,
            topic_counts: std::collections::HashMap::new(),
        };

        let review = run_nightly_review(Some(&mock_platform), &audit, &memory, Some(&dogfood), now)
            .await
            .unwrap();

        assert_eq!(review.date, "2026-11-03");
        assert_eq!(review.tasks_completed.len(), 1);
        assert!(review.tasks_completed[0].contains("Created quarterly summary in Notes"));
        assert!(
            review
                .memories_formed
                .iter()
                .any(|m| m.contains("User works best before 2 PM"))
        );
        assert!(
            review
                .upcoming_commitments
                .iter()
                .any(|c| c.contains("Sprint Planning"))
        );
        assert_eq!(review.interruption_precision_pct, 100.0);
        assert!(review.review_markdown.contains("Grok-Muse Nightly Review"));
        assert!(review.review_markdown.contains("Sprint Planning"));
    }

    #[tokio::test]
    async fn test_nightly_review_avoids_recursive_memories() {
        let audit = AuditLogger::in_memory();
        let memory = PersonalMemoryStore::in_memory().unwrap();

        // 1. Record normal preference
        memory
            .record_preference("Prefers dark mode", &["ui", "preferences"])
            .unwrap();

        // 2. Record past nightly review summary
        memory
            .record_fact(
                Some("Nightly Review 2026-11-02"),
                "# 🌙 Grok-Muse Nightly Review — 2026-11-02\n- Past summary",
                &["nightly_review", "summary", "2026-11-02"],
            )
            .unwrap();

        let mock_platform = MockPlatform::new();
        let now = Utc::now();
        let review = run_nightly_review(Some(&mock_platform), &audit, &memory, None, now)
            .await
            .unwrap();

        // Must include genuine preference
        assert!(
            review
                .memories_formed
                .iter()
                .any(|m| m.contains("Prefers dark mode"))
        );
        // Must NOT include recursive past review
        assert!(
            !review
                .memories_formed
                .iter()
                .any(|m| m.contains("Nightly Review 2026-11-02"))
        );
    }
}
