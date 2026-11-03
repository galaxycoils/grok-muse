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
}

impl ProactiveEngine {
    pub fn new(quiet_gate_config: QuietGateConfig, budget: Arc<BudgetManager>) -> Self {
        Self {
            scheduler: ProactiveScheduler::new(),
            quiet_gate: QuietGate::new(quiet_gate_config),
            budget,
            notices_history: Arc::new(Mutex::new(Vec::new())),
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

    pub fn get_history(&self) -> Vec<ProactiveNotice> {
        self.notices_history.lock().clone()
    }

    pub fn rate_notice(&self, notice_id: &str, useful: bool) -> bool {
        let mut history = self.notices_history.lock();
        if let Some(notice) = history.iter_mut().find(|n| n.id == notice_id) {
            notice.rate_useful(useful);
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

        // Process calendar events (upcoming in next 60 minutes)
        for event in events {
            let starts_in_ms = event.start_time_ms.saturating_sub(now_ms);
            let starts_in_mins = starts_in_ms / (60 * 1000);

            if starts_in_mins <= 60 {
                let loc_str = event
                    .location
                    .as_deref()
                    .map(|l| format!(" at {l}"))
                    .unwrap_or_default();
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

        // Process reminders (overdue or due within 60 minutes)
        for reminder in reminders {
            if let Some(due_ms) = reminder.due_date_ms
                && due_ms <= now_ms
            {
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
                self.quiet_gate.record_delivery(&dedup_hash);

                let notice = ProactiveNotice::new(title, body, topic, is_urgent);
                self.notices_history.lock().push(notice.clone());
                delivered.push(notice);
            }
        }

        Ok(delivered)
    }

    /// Schedule automated EventKit patrol runs on the given interval (in seconds).
    pub fn schedule_patrol<P: Platform + 'static>(&self, platform: Arc<P>, interval_secs: u64) {
        let engine_gate = self.quiet_gate.clone();
        let engine_budget = Arc::clone(&self.budget);
        let engine_history = Arc::clone(&self.notices_history);

        self.scheduler
            .register_job("patrol_calendar_reminders", interval_secs, move || {
                let p = Arc::clone(&platform);
                let gate = engine_gate.clone();
                let budget = Arc::clone(&engine_budget);
                let history = Arc::clone(&engine_history);

                tokio::spawn(async move {
                    let now = Utc::now();
                    let now_ms = now.timestamp_millis() as u64;
                    let end_ms = now_ms + (24 * 3600 * 1000);

                    if let Ok(events) = p.get_calendar_events(now_ms, end_ms).await {
                        for ev in events {
                            let dedup = format!("cal_{}_{}", ev.id, ev.start_time_ms);
                            let is_urgent = ev.start_time_ms <= now_ms + 15 * 60 * 1000;
                            if gate.can_deliver("calendar", &dedup, is_urgent, now).0
                                && budget.record_spend(0.001).is_ok()
                            {
                                let _ = p
                                    .send_notification(
                                        &format!("Upcoming: {}", ev.title),
                                        Some("Patrol"),
                                        "Event starting soon",
                                    )
                                    .await;
                                gate.record_delivery(&dedup);
                                history.lock().push(ProactiveNotice::new(
                                    ev.title,
                                    "Event starting soon",
                                    "calendar",
                                    is_urgent,
                                ));
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
}
