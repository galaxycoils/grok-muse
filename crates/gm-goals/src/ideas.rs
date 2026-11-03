use chrono::{DateTime, Utc};
use gm_core::error::Result;
use gm_memory::PersonalMemoryStore;
use gm_platform::Platform;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdeaSource {
    Calendar,
    Mail,
    Memory,
    Hybrid,
}

impl IdeaSource {
    pub fn label(&self) -> &'static str {
        match self {
            IdeaSource::Calendar => "Calendar",
            IdeaSource::Mail => "Mail",
            IdeaSource::Memory => "Memory",
            IdeaSource::Hybrid => "Hybrid",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProactiveIdea {
    pub id: String,
    pub title: String,
    pub description: String,
    pub source: IdeaSource,
    pub action_prompt: Option<String>,
    pub confidence: f32,
    pub created_at: DateTime<Utc>,
}

pub async fn generate_ideas<P: Platform>(
    platform: Option<&P>,
    memory: &PersonalMemoryStore,
    now: DateTime<Utc>,
) -> Result<Vec<ProactiveIdea>> {
    let mut ideas = Vec::new();
    let now_ms = now.timestamp_millis() as u64;

    // 1. Calendar Ideas
    if let Some(plat) = platform {
        // Look ahead 24 hours
        let window_end_ms = now_ms + (24 * 3600 * 1000);
        if let Ok(mut events) = plat.get_calendar_events(now_ms, window_end_ms).await {
            events.sort_by_key(|e| e.start_time_ms);

            // A. Detect Focus/Deep-work gaps >= 2 hours between meetings
            for i in 0..events.len().saturating_sub(1) {
                let e1 = &events[i];
                let e2 = &events[i + 1];
                if e2.start_time_ms > e1.end_time_ms {
                    let gap_ms = e2.start_time_ms - e1.end_time_ms;
                    let gap_hours = gap_ms as f64 / (3600.0 * 1000.0);
                    if gap_hours >= 2.0 {
                        let start_dt = DateTime::from_timestamp_millis(e1.end_time_ms as i64)
                            .unwrap_or(now)
                            .format("%H:%M")
                            .to_string();
                        let end_dt = DateTime::from_timestamp_millis(e2.start_time_ms as i64)
                            .unwrap_or(now)
                            .format("%H:%M")
                            .to_string();

                        ideas.push(ProactiveIdea {
                            id: format!("idea_gap_{}_{}", e1.id, e2.id),
                            title: "Schedule Deep Work Focus Block".into(),
                            description: format!(
                                "You have a {:.1}h open window between '{}' and '{}' ({}-{}). Protect this time for focus work.",
                                gap_hours, e1.title, e2.title, start_dt, end_dt
                            ),
                            source: IdeaSource::Calendar,
                            action_prompt: Some(format!(
                                "Create Calendar event 'Deep Work Block' from {} to {}",
                                start_dt, end_dt
                            )),
                            confidence: 0.92,
                            created_at: now,
                        });
                    }
                }
            }

            // B. Upcoming meeting preparation cross-referenced with personal memory
            for event in &events {
                let time_until_ms = event.start_time_ms.saturating_sub(now_ms);
                let hours_until = time_until_ms as f64 / (3600.0 * 1000.0);

                if hours_until > 0.0 && hours_until <= 4.0 {
                    // Search memory for relevant project notes matching event title
                    let search_terms: Vec<&str> = event.title.split_whitespace().collect();
                    for term in search_terms {
                        if term.len() >= 4
                            && let Ok(matched_records) = memory.search(term, 2)
                            && !matched_records.is_empty()
                        {
                            ideas.push(ProactiveIdea {
                                id: format!("idea_prep_{}", event.id),
                                title: format!("Prepare for Upcoming Meeting: '{}'", event.title),
                                description: format!(
                                    "Meeting starts in {:.1}h. Found relevant notes in memory: '{}'. Review action items before joining.",
                                    hours_until, matched_records[0].body
                                ),
                                source: IdeaSource::Hybrid,
                                action_prompt: Some(format!("Open Notes and search for '{}'", term)),
                                confidence: 0.88,
                                created_at: now,
                            });
                            break;
                        }
                    }
                }
            }
        }

        // 2. Mail Ideas (Unread Threads)
        if let Ok(emails) = plat.get_unread_emails().await {
            // Find urgent or flagged unread emails
            let urgent_emails: Vec<_> = emails
                .iter()
                .filter(|e| !e.is_read && (e.is_urgent || e.is_flagged || e.is_urgent_heuristic()))
                .collect();
            for mail in urgent_emails {
                ideas.push(ProactiveIdea {
                    id: format!("idea_mail_{}", mail.id),
                    title: format!("Follow Up: '{}'", mail.subject),
                    description: format!(
                        "Urgent unread message from {}. High priority thread awaiting response.",
                        mail.sender
                    ),
                    source: IdeaSource::Mail,
                    action_prompt: Some(format!("Open Mail and draft reply to {}", mail.sender)),
                    confidence: 0.95,
                    created_at: now,
                });
            }

            let unread_count = emails.iter().filter(|e| !e.is_read).count();
            if unread_count >= 5 {
                ideas.push(ProactiveIdea {
                    id: "idea_inbox_triage".into(),
                    title: "Automated Inbox Triage Opportunity".into(),
                    description: format!(
                        "You have {} unread emails. Triage and categorize newsletters vs actionable threads.",
                        unread_count
                    ),
                    source: IdeaSource::Mail,
                    action_prompt: Some("Open Mail and archive newsletters".into()),
                    confidence: 0.80,
                    created_at: now,
                });
            }
        }

        // 3. Reminders check (EventKit - independent from Mail)
        if let Ok(reminders) = plat.get_reminders(false).await {
            let overdue_or_soon = reminders.iter().filter(|r| {
                if !r.is_completed
                    && let Some(due) = r.due_date_ms
                {
                    due <= now_ms + (4 * 3600 * 1000)
                } else {
                    false
                }
            });
            for r in overdue_or_soon {
                ideas.push(ProactiveIdea {
                    id: format!("idea_rem_{}", r.id),
                    title: format!("Complete Pending Reminder: '{}'", r.title),
                    description: format!(
                        "Priority {} reminder due soon in list '{}'. Complete or reschedule.",
                        r.priority,
                        r.list_name.as_deref().unwrap_or("Default")
                    ),
                    source: IdeaSource::Calendar,
                    action_prompt: Some(format!("Mark reminder '{}' as completed", r.title)),
                    confidence: 0.85,
                    created_at: now,
                });
            }
        }
    }

    // 4. Memory Ideas (User Preferences & Active Knowledge)
    if let Ok(records) = memory.all_records(50) {
        for r in &records {
            let lower = r.body.to_lowercase();
            if (lower.contains("preference")
                || lower.contains("prefers")
                || lower.contains("daily"))
                && ideas.len() < 8
            {
                ideas.push(ProactiveIdea {
                    id: format!("idea_mem_{}", r.id),
                    title: format!(
                        "Preference Check: {}",
                        r.title.as_deref().unwrap_or("Routine Preference")
                    ),
                    description: format!(
                        "Personal rule noted: '{}'. Verify current system state conforms.",
                        r.body
                    ),
                    source: IdeaSource::Memory,
                    action_prompt: Some(format!("Verify system status for: {}", r.body)),
                    confidence: 0.75,
                    created_at: now,
                });
            }
        }
    }

    // Deduplicate ideas by title
    let mut seen_titles = std::collections::HashSet::new();
    ideas.retain(|i| seen_titles.insert(i.title.clone()));

    // Sort by confidence descending
    ideas.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(ideas)
}

pub fn format_ideas(ideas: &[ProactiveIdea]) -> String {
    let mut out = String::new();
    out.push_str("═══════════════════════════════════════════════════════════\n");
    out.push_str("💡 Grok-Muse Proactive Ideas & Suggestions\n");
    out.push_str("═══════════════════════════════════════════════════════════\n\n");

    if ideas.is_empty() {
        out.push_str(
            "No proactive suggestions at this time. All schedules and inboxes are clear!\n",
        );
    } else {
        for (i, idea) in ideas.iter().enumerate() {
            out.push_str(&format!(
                "{}. [{}] {}\n",
                i + 1,
                idea.source.label(),
                idea.title
            ));
            out.push_str(&format!("   Rationale:  {}\n", idea.description));
            if let Some(ref prompt) = idea.action_prompt {
                out.push_str(&format!("   Action:     gm do \"{}\"\n", prompt));
            }
            out.push_str(&format!(
                "   Confidence: {:.0}%\n\n",
                idea.confidence * 100.0
            ));
        }
        out.push_str("To execute any suggestion, run:\n");
        out.push_str("  gm do \"<action>\"\n");
    }

    out.push_str("═══════════════════════════════════════════════════════════\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use gm_core::types::{CalendarItem, MailItem, ReminderItem};
    use gm_platform::MockPlatform;

    #[tokio::test]
    async fn test_generate_ideas_calendar_and_mail() {
        let platform = MockPlatform::new();
        let memory = PersonalMemoryStore::in_memory().unwrap();

        let now = Utc::now();
        let now_ms = now.timestamp_millis() as u64;

        // 1. Two meetings with a 2.5 hour gap
        let e1 = CalendarItem {
            id: "e1".into(),
            title: "Morning Standup".into(),
            start_time_ms: now_ms + (30 * 60 * 1000),
            end_time_ms: now_ms + (60 * 60 * 1000),
            location: None,
            notes: None,
            is_all_day: false,
        };
        let e2 = CalendarItem {
            id: "e2".into(),
            title: "Afternoon Sync".into(),
            start_time_ms: now_ms + (210 * 60 * 1000), // 2.5 hr gap from end of e1
            end_time_ms: now_ms + (240 * 60 * 1000),
            location: None,
            notes: None,
            is_all_day: false,
        };
        platform.set_calendar_events(vec![e1, e2]);

        // 2. Urgent unread email
        platform.set_unread_emails(vec![MailItem {
            id: "mail_urgent".into(),
            sender: "boss@corp.com".into(),
            subject: "URGENT: Review release notes".into(),
            snippet: "Please check the release notes before 5 PM".into(),
            received_time_ms: now_ms,
            is_read: false,
            is_flagged: true,
            is_urgent: true,
        }]);

        // 3. Pending reminder
        platform.set_reminders(vec![ReminderItem {
            id: "rem_1".into(),
            title: "Submit Expense Report".into(),
            due_date_ms: Some(now_ms + 60 * 60 * 1000),
            is_completed: false,
            priority: 1,
            list_name: Some("Work".into()),
        }]);

        // 4. Memory note with preference
        memory
            .record_preference("User prefers deep work in the afternoon", &["work"])
            .unwrap();

        let ideas = generate_ideas(Some(&platform), &memory, now).await.unwrap();
        assert!(!ideas.is_empty());

        // Verify deep work gap idea was generated
        assert!(
            ideas
                .iter()
                .any(|i| i.title.contains("Deep Work Focus Block"))
        );

        // Verify urgent mail follow-up idea was generated
        assert!(
            ideas
                .iter()
                .any(|i| i.title.contains("URGENT: Review release notes"))
        );

        // Verify reminder idea was generated
        assert!(
            ideas
                .iter()
                .any(|i| i.title.contains("Submit Expense Report"))
        );

        let formatted = format_ideas(&ideas);
        assert!(formatted.contains("Proactive Ideas & Suggestions"));
        assert!(formatted.contains("Deep Work Focus Block"));
        assert!(formatted.contains("boss@corp.com"));
    }

    #[tokio::test]
    async fn test_generate_ideas_reminders_independent_of_mail() {
        let platform = MockPlatform::new();
        let memory = PersonalMemoryStore::in_memory().unwrap();
        let now = Utc::now();
        let now_ms = now.timestamp_millis() as u64;

        // No calendar events, no emails
        platform.set_calendar_events(vec![]);
        platform.set_unread_emails(vec![]);

        // Only a reminder
        platform.set_reminders(vec![ReminderItem {
            id: "rem_solo".into(),
            title: "Taxes filing deadline".into(),
            due_date_ms: Some(now_ms + 30 * 60 * 1000), // Due in 30 mins
            is_completed: false,
            priority: 1,
            list_name: Some("Finance".into()),
        }]);

        let ideas = generate_ideas(Some(&platform), &memory, now).await.unwrap();
        assert_eq!(ideas.len(), 1);
        assert!(ideas[0].title.contains("Taxes filing deadline"));
        assert_eq!(ideas[0].source, IdeaSource::Calendar);
    }
}
