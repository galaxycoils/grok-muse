use crate::dogfood::DogfoodTracker;
use crate::nightly_review::{NightlyReviewSummary, run_nightly_review};
use crate::notice::ProactiveNotice;
use crate::quiet_gate::{QuietGate, QuietGateConfig};
use crate::scheduler::ProactiveScheduler;
use chrono::{DateTime, Utc};
use gm_core::error::Result;
use gm_llm::BudgetManager;
use gm_platform::Platform;
use parking_lot::Mutex;
use std::sync::Arc;
use tracing::{info, warn};

pub struct ProactiveEngine {
    scheduler: ProactiveScheduler,
    quiet_gate: QuietGate,
    budget: Arc<BudgetManager>,
    notices_history: Arc<Mutex<Vec<ProactiveNotice>>>,
    dogfood_tracker: Arc<DogfoodTracker>,
}

impl ProactiveEngine {
    pub fn new(quiet_gate_config: QuietGateConfig, budget: Arc<BudgetManager>) -> Self {
        let tracker = Arc::new(
            DogfoodTracker::default_tracker(budget.config().daily_spend_cap_usd)
                .unwrap_or_else(|_| DogfoodTracker::in_memory()),
        );
        Self {
            scheduler: ProactiveScheduler::new(),
            quiet_gate: QuietGate::new(quiet_gate_config),
            budget,
            notices_history: Arc::new(Mutex::new(Vec::new())),
            dogfood_tracker: tracker,
        }
    }

    pub fn with_tracker(
        quiet_gate_config: QuietGateConfig,
        budget: Arc<BudgetManager>,
        dogfood_tracker: Arc<DogfoodTracker>,
    ) -> Self {
        Self {
            scheduler: ProactiveScheduler::new(),
            quiet_gate: QuietGate::new(quiet_gate_config),
            budget,
            notices_history: Arc::new(Mutex::new(Vec::new())),
            dogfood_tracker,
        }
    }

    pub fn scheduler(&self) -> &ProactiveScheduler {
        &self.scheduler
    }

    pub fn quiet_gate(&self) -> &QuietGate {
        &self.quiet_gate
    }

    pub fn budget(&self) -> &Arc<BudgetManager> {
        &self.budget
    }

    pub fn dogfood_tracker(&self) -> &Arc<DogfoodTracker> {
        &self.dogfood_tracker
    }

    pub fn get_history(&self) -> Vec<ProactiveNotice> {
        self.notices_history.lock().clone()
    }

    pub fn rate_notice(&self, notice_id: &str, useful: bool) -> bool {
        let mut history = self.notices_history.lock();
        if let Some(notice) = history.iter_mut().find(|n| n.id == notice_id) {
            notice.rate_useful(useful);
            let _ = self
                .dogfood_tracker
                .record_rating(notice_id, useful, Utc::now());
            true
        } else {
            false
        }
    }

    /// Evaluates calendar and reminder state via EventKit, generates candidate notices,
    /// checks quiet gate rules and daily spend caps, and delivers authorized alerts via macOS UserNotifications.
    pub async fn patrol_calendar_and_reminders<P: Platform>(
        &self,
        platform: &P,
        window_hours: i64,
        now: DateTime<Utc>,
    ) -> Result<Vec<ProactiveNotice>> {
        let now_ms = now.timestamp_millis() as u64;
        let window_end_ms = now_ms + (window_hours as u64 * 3600 * 1000);

        // 1. Fetch Calendar events from EventKit via Platform
        let events = platform.get_calendar_events(now_ms, window_end_ms).await?;

        // 2. Fetch Incomplete Reminders from EventKit via Platform
        let reminders = platform.get_reminders(false).await?;

        let mut candidate_notices = Vec::new();

        // Process calendar events (upcoming in next 60 minutes or active)
        for event in events {
            if event.end_time_ms <= now_ms {
                // Event already ended, skip
                continue;
            }

            let loc_str = event
                .location
                .as_deref()
                .map(|l| format!(" at {l}"))
                .unwrap_or_default();

            if event.start_time_ms <= now_ms {
                // Currently in progress
                let title = format!("Happening Now: {}", event.title);
                let body = format!("Event in progress{loc_str}");
                candidate_notices.push((
                    title,
                    body,
                    "calendar".to_string(),
                    false,
                    format!("cal_{}_{}", event.id, event.start_time_ms),
                ));
            } else {
                let starts_in_ms = event.start_time_ms - now_ms;
                let starts_in_mins = starts_in_ms / (60 * 1000);

                if starts_in_mins <= 60 {
                    let title = format!("Upcoming: {}", event.title);
                    let body = format!("Event starts in {starts_in_mins}m{loc_str}");
                    let is_urgent = starts_in_mins <= 15; // Urgent if starting in <= 15m

                    candidate_notices.push((
                        title,
                        body,
                        "calendar".to_string(),
                        is_urgent,
                        format!("cal_{}_{}", event.id, event.start_time_ms),
                    ));
                }
            }
        }

        // Process reminders (overdue or due within 60 minutes)
        for reminder in reminders {
            if let Some(due_ms) = reminder.due_date_ms {
                if due_ms <= now_ms {
                    let title = format!("Overdue Reminder: {}", reminder.title);
                    let body = reminder
                        .list_name
                        .as_deref()
                        .map(|l| format!("In list '{l}'"))
                        .unwrap_or_else(|| "Reminder is overdue".into());
                    let is_urgent = reminder.priority <= 1; // High priority

                    candidate_notices.push((
                        title,
                        body,
                        "reminders".to_string(),
                        is_urgent,
                        format!("rem_overdue_{due_ms}_{}", reminder.id),
                    ));
                } else if due_ms <= now_ms + (60 * 60 * 1000) {
                    let due_in_mins = (due_ms - now_ms) / (60 * 1000);
                    let title = format!("Upcoming Reminder: {}", reminder.title);
                    let body = reminder
                        .list_name
                        .as_deref()
                        .map(|l| format!("Due in {due_in_mins}m in '{l}'"))
                        .unwrap_or_else(|| format!("Due in {due_in_mins}m"));
                    let is_urgent = due_in_mins <= 15 || reminder.priority <= 1;

                    candidate_notices.push((
                        title,
                        body,
                        "reminders".to_string(),
                        is_urgent,
                        format!("rem_due_{due_ms}_{}", reminder.id),
                    ));
                }
            }
        }

        let mut delivered = Vec::new();

        for (title, body, topic, is_urgent, dedup_hash) in candidate_notices {
            // A. Check Quiet Gate
            let (allowed, reason) =
                self.quiet_gate
                    .can_deliver(&topic, &dedup_hash, is_urgent, now);
            if !allowed {
                info!(
                    "Notice '{}' suppressed by QuietGate: {}",
                    title,
                    reason.unwrap_or("unknown")
                );
                continue;
            }

            // B. Enforce Daily Spend Cap Tracking
            // Simulated cost of triage/dispatch check ($0.001)
            let triage_cost = 0.001;
            if let Err(e) = self.budget.record_spend(triage_cost) {
                warn!("Proactive alert suppressed due to budget spend cap: {e}");
                continue;
            }

            // C. Deliver via macOS UserNotifications alert
            let sent = platform
                .send_notification(&title, Some("Grok-Muse Patrol"), &body)
                .await?;

            if sent {
                self.quiet_gate.record_delivery_at(&dedup_hash, now);

                let notice = ProactiveNotice::new(title, body, topic, is_urgent);
                let _ = self.dogfood_tracker.record_notice_delivered(
                    &notice.id,
                    &notice.topic,
                    &notice.title,
                    notice.is_urgent,
                    &dedup_hash,
                    triage_cost,
                    now,
                );
                let _ = self.dogfood_tracker.record_daily_spend(
                    &now.format("%Y-%m-%d").to_string(),
                    self.budget.get_daily_spent(),
                    self.budget.config().daily_spend_cap_usd,
                    now,
                );
                self.notices_history.lock().push(notice.clone());
                delivered.push(notice);
            }
        }

        Ok(delivered)
    }

    /// Handle feedback actions on proactive notices with quiet gate semantics.
    pub fn handle_notice_action(
        &self,
        notice_id: &str,
        topic: &str,
        dedup_hash: &str,
        action: crate::quiet_gate::NoticeAction,
        now: DateTime<Utc>,
    ) {
        self.quiet_gate
            .handle_feedback(topic, dedup_hash, action, now);
        let useful = !matches!(action, crate::quiet_gate::NoticeAction::Stop);
        self.rate_notice(notice_id, useful);
        let action_str = match action {
            crate::quiet_gate::NoticeAction::Done => "done",
            crate::quiet_gate::NoticeAction::Later(_) => "later",
            crate::quiet_gate::NoticeAction::Stop => "stop",
        };
        let _ =
            self.dogfood_tracker
                .record_feedback(notice_id, topic, action_str, Some(useful), now);
    }

    /// Evaluates unread emails via Platform, checks for urgency, applies QuietGate rules
    /// and budget limits, and dispatches native alert notifications.
    pub async fn patrol_mail<P: Platform>(
        &self,
        platform: &P,
        now: DateTime<Utc>,
    ) -> Result<Vec<ProactiveNotice>> {
        let emails = platform.get_unread_emails().await?;
        let mut candidate_notices = Vec::new();

        for email in emails {
            if email.is_read {
                continue;
            }
            let is_urgent = email.is_urgent_heuristic();
            let title = if is_urgent {
                format!("Urgent Mail: {}", email.subject)
            } else {
                format!("Mail: {}", email.subject)
            };

            let body = if email.snippet.trim().is_empty() {
                format!("From {}", email.sender)
            } else {
                format!("From {}: {}", email.sender, email.snippet)
            };

            let dedup_hash = format!("mail_{}_{}", email.id, email.received_time_ms);
            candidate_notices.push((title, body, "mail".to_string(), is_urgent, dedup_hash));
        }

        let mut delivered = Vec::new();

        for (title, body, topic, is_urgent, dedup_hash) in candidate_notices {
            // A. Check Quiet Gate
            let (allowed, reason) =
                self.quiet_gate
                    .can_deliver(&topic, &dedup_hash, is_urgent, now);
            if !allowed {
                info!(
                    "Mail notice '{}' suppressed by QuietGate: {}",
                    title,
                    reason.unwrap_or("unknown")
                );
                continue;
            }

            // B. Enforce Daily Spend Cap Tracking
            let triage_cost = 0.001;
            if let Err(e) = self.budget.record_spend(triage_cost) {
                warn!("Mail alert suppressed due to budget spend cap: {e}");
                continue;
            }

            // C. Deliver via macOS UserNotifications alert
            let sent = platform
                .send_notification(&title, Some("Mail Patrol"), &body)
                .await?;

            if sent {
                self.quiet_gate.record_delivery_at(&dedup_hash, now);

                let notice = ProactiveNotice::new(title, body, topic, is_urgent);
                let _ = self.dogfood_tracker.record_notice_delivered(
                    &notice.id,
                    &notice.topic,
                    &notice.title,
                    notice.is_urgent,
                    &dedup_hash,
                    triage_cost,
                    now,
                );
                let _ = self.dogfood_tracker.record_daily_spend(
                    &now.format("%Y-%m-%d").to_string(),
                    self.budget.get_daily_spent(),
                    self.budget.config().daily_spend_cap_usd,
                    now,
                );
                self.notices_history.lock().push(notice.clone());
                delivered.push(notice);
            }
        }

        Ok(delivered)
    }

    /// Synthesizes structured nightly review from audit events, memories, dogfood metrics and calendar/reminders.
    pub async fn nightly_review<P: Platform>(
        &self,
        platform: Option<&P>,
        audit: &gm_safety::AuditLogger,
        memory: &gm_memory::PersonalMemoryStore,
        now: DateTime<Utc>,
    ) -> Result<NightlyReviewSummary> {
        let df_metrics = self.dogfood_tracker.compute_metrics();
        run_nightly_review(platform, audit, memory, Some(&df_metrics), now).await
    }

    /// Schedule automated EventKit patrol runs on the given interval (in seconds).
    pub fn schedule_patrol<P: Platform + 'static>(&self, platform: Arc<P>, interval_secs: u64) {
        let engine_gate = self.quiet_gate.clone();
        let engine_budget = Arc::clone(&self.budget);
        let engine_history = Arc::clone(&self.notices_history);
        let engine_tracker = Arc::clone(&self.dogfood_tracker);
        let cal_platform = Arc::clone(&platform);

        self.scheduler
            .register_job("patrol_calendar_reminders", interval_secs, move || {
                let p = Arc::clone(&cal_platform);
                let gate = engine_gate.clone();
                let budget = Arc::clone(&engine_budget);
                let history = Arc::clone(&engine_history);
                let tracker = Arc::clone(&engine_tracker);

                tokio::spawn(async move {
                    let now = Utc::now();
                    let now_ms = now.timestamp_millis() as u64;
                    let end_ms = now_ms + (24 * 3600 * 1000);

                    // 1. Calendar
                    if let Ok(events) = p.get_calendar_events(now_ms, end_ms).await {
                        for ev in events {
                            if ev.end_time_ms <= now_ms {
                                continue;
                            }
                            let dedup = format!("cal_{}_{}", ev.id, ev.start_time_ms);
                            let starts_in_ms = ev.start_time_ms.saturating_sub(now_ms);
                            let is_urgent = starts_in_ms <= 15 * 60 * 1000;
                            let starts_in_mins = starts_in_ms / (60 * 1000);
                            let body = format!("Event starts in {starts_in_mins}m");

                            if gate.can_deliver("calendar", &dedup, is_urgent, now).0
                                && budget.record_spend(0.001).is_ok()
                            {
                                let _ = p
                                    .send_notification(
                                        &format!("Upcoming: {}", ev.title),
                                        Some("Patrol"),
                                        &body,
                                    )
                                    .await;
                                gate.record_delivery_at(&dedup, now);
                                let notice =
                                    ProactiveNotice::new(&ev.title, &body, "calendar", is_urgent);
                                let _ = tracker.record_notice_delivered(
                                    &notice.id,
                                    "calendar",
                                    &notice.title,
                                    is_urgent,
                                    &dedup,
                                    0.001,
                                    now,
                                );
                                history.lock().push(notice);
                            }
                        }
                    }

                    // 2. Reminders
                    if let Ok(rems) = p.get_reminders(false).await {
                        for rem in rems {
                            if let Some(due_ms) = rem.due_date_ms {
                                let (is_candidate, is_urgent, title, body, dedup) =
                                    if due_ms <= now_ms {
                                        (
                                            true,
                                            rem.priority <= 1,
                                            format!("Overdue: {}", rem.title),
                                            rem.list_name
                                                .unwrap_or_else(|| "Reminder is overdue".into()),
                                            format!("rem_overdue_{due_ms}_{}", rem.id),
                                        )
                                    } else if due_ms <= now_ms + (60 * 60 * 1000) {
                                        let mins = (due_ms - now_ms) / (60 * 1000);
                                        (
                                            true,
                                            mins <= 15 || rem.priority <= 1,
                                            format!("Upcoming Reminder: {}", rem.title),
                                            format!("Due in {mins}m"),
                                            format!("rem_due_{due_ms}_{}", rem.id),
                                        )
                                    } else {
                                        (false, false, String::new(), String::new(), String::new())
                                    };

                                if is_candidate
                                    && gate.can_deliver("reminders", &dedup, is_urgent, now).0
                                    && budget.record_spend(0.001).is_ok()
                                {
                                    let _ =
                                        p.send_notification(&title, Some("Patrol"), &body).await;
                                    gate.record_delivery_at(&dedup, now);
                                    let notice =
                                        ProactiveNotice::new(&title, &body, "reminders", is_urgent);
                                    let _ = tracker.record_notice_delivered(
                                        &notice.id,
                                        "reminders",
                                        &notice.title,
                                        is_urgent,
                                        &dedup,
                                        0.001,
                                        now,
                                    );
                                    history.lock().push(notice);
                                }
                            }
                        }
                    }
                });
            });

        let mail_gate = self.quiet_gate.clone();
        let mail_budget = Arc::clone(&self.budget);
        let mail_history = Arc::clone(&self.notices_history);
        let mail_tracker = Arc::clone(&self.dogfood_tracker);
        let mail_platform = Arc::clone(&platform);

        self.scheduler
            .register_job("patrol_mail", interval_secs, move || {
                let p = Arc::clone(&mail_platform);
                let gate = mail_gate.clone();
                let budget = Arc::clone(&mail_budget);
                let history = Arc::clone(&mail_history);
                let tracker = Arc::clone(&mail_tracker);

                tokio::spawn(async move {
                    let now = Utc::now();
                    if let Ok(emails) = p.get_unread_emails().await {
                        for email in emails {
                            if email.is_read {
                                continue;
                            }
                            let is_urgent = email.is_urgent_heuristic();
                            let dedup = format!("mail_{}_{}", email.id, email.received_time_ms);
                            let title = if is_urgent {
                                format!("Urgent Mail: {}", email.subject)
                            } else {
                                format!("Unread Mail: {}", email.subject)
                            };
                            let body = format!("From {}: {}", email.sender, email.snippet);

                            if gate.can_deliver("mail", &dedup, is_urgent, now).0
                                && budget.record_spend(0.001).is_ok()
                            {
                                let _ = p
                                    .send_notification(&title, Some("Mail Patrol"), &body)
                                    .await;
                                gate.record_delivery_at(&dedup, now);
                                let notice = ProactiveNotice::new(&title, &body, "mail", is_urgent);
                                let _ = tracker.record_notice_delivered(
                                    &notice.id, "mail", &title, is_urgent, &dedup, 0.001, now,
                                );
                                history.lock().push(notice);
                            }
                        }
                    }
                });
            });
    }

    /// Tick scheduler for time progression.
    pub fn tick(&self, now: DateTime<Utc>) -> u32 {
        self.scheduler.tick(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use gm_core::config::BudgetConfig;
    use gm_core::types::{CalendarItem, ReminderItem};
    use gm_platform::MockPlatform;

    #[tokio::test]
    async fn test_proactive_engine_calendar_patrol_delivery_and_quiet_gate() {
        let mock_platform = Arc::new(MockPlatform::new());
        let budget_config = BudgetConfig {
            daily_spend_cap_usd: 1.0,
            per_job_spend_cap_usd: 0.1,
            max_daily_notifications: 3,
        };
        let budget = Arc::new(BudgetManager::new(budget_config));
        let quiet_gate_config = QuietGateConfig {
            quiet_start_hour: 22,
            quiet_end_hour: 8,
            max_daily_notices: 3,
        };

        let engine = ProactiveEngine::new(quiet_gate_config, budget);

        // Afternoon time (14:00 UTC) outside quiet hours
        let now = Utc.with_ymd_and_hms(2026, 11, 3, 14, 0, 0).unwrap();
        let now_ms = now.timestamp_millis() as u64;

        // Set an upcoming event 20 minutes from now
        mock_platform.set_calendar_events(vec![CalendarItem {
            id: "cal_1".into(),
            title: "Architecture Review".into(),
            start_time_ms: now_ms + (20 * 60 * 1000),
            end_time_ms: now_ms + (50 * 60 * 1000),
            location: Some("Room 4B".into()),
            notes: None,
            is_all_day: false,
        }]);

        // Set an overdue reminder
        mock_platform.set_reminders(vec![ReminderItem {
            id: "rem_1".into(),
            title: "Submit Expense Report".into(),
            due_date_ms: Some(now_ms - 1000),
            is_completed: false,
            priority: 1,
            list_name: Some("Finance".into()),
        }]);

        // Run patrol
        let delivered = engine
            .patrol_calendar_and_reminders(&*mock_platform, 24, now)
            .await
            .expect("patrol");

        assert_eq!(delivered.len(), 2);
        assert!(
            delivered
                .iter()
                .any(|n| n.title.contains("Architecture Review"))
        );
        assert!(
            delivered
                .iter()
                .any(|n| n.title.contains("Submit Expense Report"))
        );

        // Verify notification was dispatched to platform
        let notifs = mock_platform.get_sent_notifications();
        assert_eq!(notifs.len(), 2);

        // Running patrol again immediately should suppress duplicate via QuietGate dedup
        let delivered_again = engine
            .patrol_calendar_and_reminders(&*mock_platform, 24, now)
            .await
            .expect("patrol dedup");
        assert_eq!(delivered_again.len(), 0);
    }

    #[tokio::test]
    async fn test_proactive_engine_quiet_hours_and_spend_cap_enforcement() {
        let mock_platform = Arc::new(MockPlatform::new());
        let budget_config = BudgetConfig {
            daily_spend_cap_usd: 0.0005, // Lower than per-job triage cost to trigger cap
            per_job_spend_cap_usd: 0.1,
            max_daily_notifications: 3,
        };
        let budget = Arc::new(BudgetManager::new(budget_config));
        let quiet_gate_config = QuietGateConfig::default();

        let engine = ProactiveEngine::new(quiet_gate_config, budget);

        let midnight = Utc.with_ymd_and_hms(2026, 11, 3, 2, 0, 0).unwrap();
        let now_ms = midnight.timestamp_millis() as u64;

        mock_platform.set_calendar_events(vec![CalendarItem {
            id: "cal_quiet".into(),
            title: "Midnight Sync".into(),
            start_time_ms: now_ms + (20 * 60 * 1000),
            end_time_ms: now_ms + (50 * 60 * 1000),
            location: None,
            notes: None,
            is_all_day: false,
        }]);

        // Non-urgent meeting at 2 AM should be suppressed by QuietGate quiet hours!
        let delivered = engine
            .patrol_calendar_and_reminders(&*mock_platform, 24, midnight)
            .await
            .expect("patrol quiet hours");
        assert_eq!(delivered.len(), 0);
    }

    #[tokio::test]
    async fn test_proactive_engine_upcoming_reminders_and_notice_action() {
        let mock_platform = Arc::new(MockPlatform::new());
        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let engine = ProactiveEngine::new(QuietGateConfig::default(), budget);

        let now = Utc.with_ymd_and_hms(2026, 11, 3, 14, 0, 0).unwrap();
        let now_ms = now.timestamp_millis() as u64;

        // Reminder due in 25 minutes
        mock_platform.set_reminders(vec![ReminderItem {
            id: "rem_upcoming".into(),
            title: "Dentist Appointment".into(),
            due_date_ms: Some(now_ms + 25 * 60 * 1000),
            is_completed: false,
            priority: 2,
            list_name: Some("Personal".into()),
        }]);

        let delivered = engine
            .patrol_calendar_and_reminders(&*mock_platform, 24, now)
            .await
            .expect("patrol upcoming");
        assert_eq!(delivered.len(), 1);
        assert!(
            delivered[0]
                .title
                .contains("Upcoming Reminder: Dentist Appointment")
        );

        // Test user dismissing with Done action
        let notice = &delivered[0];
        let dedup_hash = format!("rem_due_{}_{}", now_ms + 25 * 60 * 1000, "rem_upcoming");
        engine.handle_notice_action(
            &notice.id,
            "reminders",
            &dedup_hash,
            crate::quiet_gate::NoticeAction::Done,
            now,
        );

        // Verify that notice was rated useful and subsequent patrol suppresses it permanently
        let history = engine.get_history();
        assert_eq!(history[0].useful_rating, Some(true));

        let delivered_again = engine
            .patrol_calendar_and_reminders(&*mock_platform, 24, now)
            .await
            .expect("patrol after done");
        assert_eq!(delivered_again.len(), 0);
    }

    #[tokio::test]
    async fn test_proactive_engine_mail_patrol() {
        use gm_core::types::MailItem;

        let mock_platform = Arc::new(MockPlatform::new());
        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let tracker = Arc::new(DogfoodTracker::in_memory());
        let engine = ProactiveEngine::with_tracker(QuietGateConfig::default(), budget, tracker);

        // 1. Midnight time (2 AM UTC) - quiet hours active
        let midnight = Utc.with_ymd_and_hms(2026, 11, 3, 2, 0, 0).unwrap();
        let now_ms = midnight.timestamp_millis() as u64;

        mock_platform.set_unread_emails(vec![
            // Non-urgent newsletter -> should be suppressed during quiet hours
            MailItem {
                id: "msg_news".into(),
                sender: "news@tech.com".into(),
                subject: "Weekly Digest".into(),
                snippet: "Here is your weekly summary".into(),
                received_time_ms: now_ms,
                is_read: false,
                is_flagged: false,
                is_urgent: false,
            },
            // Urgent mail -> should bypass quiet hours
            MailItem {
                id: "msg_urgent".into(),
                sender: "boss@corp.com".into(),
                subject: "CRITICAL: Server Down ASAP".into(),
                snippet: "Production database unreachable".into(),
                received_time_ms: now_ms,
                is_read: false,
                is_flagged: true,
                is_urgent: true,
            },
        ]);

        let delivered = engine
            .patrol_mail(&*mock_platform, midnight)
            .await
            .expect("mail patrol");

        // Only the urgent mail should be delivered during quiet hours
        assert_eq!(delivered.len(), 1);
        assert!(delivered[0].title.contains("CRITICAL: Server Down ASAP"));
        assert!(delivered[0].is_urgent);

        // Verify dogfood metrics recorded this delivery
        let metrics = engine.dogfood_tracker().compute_metrics();
        assert_eq!(metrics.total_notices_delivered, 1);

        // Immediate subsequent mail patrol should dedup
        let delivered_again = engine
            .patrol_mail(&*mock_platform, midnight)
            .await
            .expect("mail dedup");
        assert_eq!(delivered_again.len(), 0);
    }

    #[tokio::test]
    async fn test_proactive_engine_nightly_review() {
        use gm_memory::PersonalMemoryStore;
        use gm_safety::AuditLogger;

        let mock_platform = Arc::new(MockPlatform::new());
        let budget = Arc::new(BudgetManager::new(BudgetConfig::default()));
        let engine = ProactiveEngine::new(QuietGateConfig::default(), budget);

        let audit = AuditLogger::in_memory();
        audit.log(
            "task_done",
            Some("meta".into()),
            Some("muse-spark".into()),
            Some(300),
            Some(0.003),
            Some("Automated file reorganization".into()),
            serde_json::json!({}),
        );

        let memory = PersonalMemoryStore::in_memory().unwrap();
        memory
            .record_preference("Prefers dark mode in all apps", &["ui", "preferences"])
            .unwrap();

        let now = Utc.with_ymd_and_hms(2026, 11, 3, 21, 30, 0).unwrap();
        let summary = engine
            .nightly_review(Some(&*mock_platform), &audit, &memory, now)
            .await
            .expect("nightly review");

        assert_eq!(summary.date, "2026-11-03");
        assert_eq!(summary.tasks_completed.len(), 1);
        assert!(summary.tasks_completed[0].contains("Automated file reorganization"));
        assert!(
            summary
                .memories_formed
                .iter()
                .any(|m| m.contains("Prefers dark mode"))
        );
        assert!(summary.review_markdown.contains("Grok-Muse Nightly Review"));

        // Verify that nightly review was persisted in memory store
        let search_results = memory.search("Nightly Review", 5).unwrap();
        assert!(!search_results.is_empty());
    }
}
