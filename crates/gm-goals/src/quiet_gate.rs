use chrono::{DateTime, Timelike, Utc};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct QuietGateConfig {
    pub quiet_start_hour: u32, // e.g. 22 (10 PM)
    pub quiet_end_hour: u32,   // e.g. 8 (8 AM)
    pub max_daily_notices: u32,
}

impl Default for QuietGateConfig {
    fn default() -> Self {
        Self {
            quiet_start_hour: 22,
            quiet_end_hour: 8,
            max_daily_notices: 3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct QuietGate {
    config: QuietGateConfig,
    opted_out_topics: Arc<Mutex<HashSet<String>>>,
    sent_notices_today: Arc<Mutex<u32>>,
    last_sent_date: Arc<Mutex<Option<chrono::NaiveDate>>>,
    recent_hashes: Arc<Mutex<HashSet<String>>>,
    snoozed_until: Arc<Mutex<Option<DateTime<Utc>>>>,
    topic_snooze: Arc<Mutex<std::collections::HashMap<String, DateTime<Utc>>>>,
}

impl QuietGate {
    pub fn new(config: QuietGateConfig) -> Self {
        Self {
            config,
            opted_out_topics: Arc::new(Mutex::new(HashSet::new())),
            sent_notices_today: Arc::new(Mutex::new(0)),
            last_sent_date: Arc::new(Mutex::new(None)),
            recent_hashes: Arc::new(Mutex::new(HashSet::new())),
            snoozed_until: Arc::new(Mutex::new(None)),
            topic_snooze: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    pub fn opt_out_topic(&self, topic: &str) {
        self.opted_out_topics.lock().insert(topic.to_lowercase());
    }

    pub fn opt_in_topic(&self, topic: &str) {
        self.opted_out_topics.lock().remove(&topic.to_lowercase());
    }

    pub fn snooze(&self, until: DateTime<Utc>) {
        *self.snoozed_until.lock() = Some(until);
    }

    pub fn snooze_topic(&self, topic: &str, until: DateTime<Utc>) {
        self.topic_snooze.lock().insert(topic.to_lowercase(), until);
    }

    /// Evaluates whether a notice is allowed to be delivered right now.
    pub fn can_deliver(
        &self,
        topic: &str,
        dedup_hash: &str,
        is_urgent: bool,
        current_time: DateTime<Utc>,
    ) -> (bool, Option<&'static str>) {
        // 1. Topic opt-out check
        if self.opted_out_topics.lock().contains(&topic.to_lowercase()) {
            return (false, Some("Topic opted out"));
        }

        // 2. Dedup check
        if self.recent_hashes.lock().contains(dedup_hash) {
            return (false, Some("Duplicate notification suppressed"));
        }

        // 3. Urgency bypass skips quiet hours, snoozes, and daily caps
        if is_urgent {
            return (true, None);
        }

        // 4. Global snooze check
        if let Some(until) = *self.snoozed_until.lock()
            && current_time < until
        {
            return (false, Some("Notifications snoozed"));
        }

        // 5. Topic snooze check
        if let Some(until) = self.topic_snooze.lock().get(&topic.to_lowercase())
            && current_time < *until
        {
            return (false, Some("Topic snoozed"));
        }

        // 6. Quiet hours check
        let hour = current_time.hour();
        let in_quiet_hours = if self.config.quiet_start_hour > self.config.quiet_end_hour {
            // Overnight window e.g. 22 to 8
            hour >= self.config.quiet_start_hour || hour < self.config.quiet_end_hour
        } else {
            hour >= self.config.quiet_start_hour && hour < self.config.quiet_end_hour
        };

        if in_quiet_hours {
            return (false, Some("Quiet hours active"));
        }

        // 7. Daily notification cap check (using NaiveDate for robust date rollover)
        let current_date = current_time.date_naive();
        let mut last_date = self.last_sent_date.lock();
        let mut sent_today = self.sent_notices_today.lock();

        if *last_date != Some(current_date) {
            *last_date = Some(current_date);
            *sent_today = 0;
            self.recent_hashes.lock().clear();
        }

        if *sent_today >= self.config.max_daily_notices {
            return (false, Some("Daily notification cap reached"));
        }

        (true, None)
    }

    /// Mark a notice as delivered
    pub fn record_delivery(&self, dedup_hash: &str) {
        *self.sent_notices_today.lock() += 1;
        self.recent_hashes.lock().insert(dedup_hash.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn test_quiet_gate() {
        let gate = QuietGate::new(QuietGateConfig::default());

        // Quiet hour test (midnight: hour 0)
        let midnight = Utc.with_ymd_and_hms(2026, 11, 3, 0, 0, 0).unwrap();
        let (allowed, reason) = gate.can_deliver("calendar", "hash_1", false, midnight);
        assert!(!allowed);
        assert_eq!(reason, Some("Quiet hours active"));

        // Urgency bypass during quiet hours
        let (allowed, _) = gate.can_deliver("calendar", "hash_1", true, midnight);
        assert!(allowed);

        // Day time (hour 14)
        let daytime = Utc.with_ymd_and_hms(2026, 11, 3, 14, 0, 0).unwrap();
        let (allowed, _) = gate.can_deliver("calendar", "hash_2", false, daytime);
        assert!(allowed);
        gate.record_delivery("hash_2");

        // Duplicate test
        let (allowed, reason) = gate.can_deliver("calendar", "hash_2", false, daytime);
        assert!(!allowed);
        assert_eq!(reason, Some("Duplicate notification suppressed"));

        // Snooze test
        let future_wake = daytime + chrono::Duration::hours(2);
        gate.snooze(future_wake);
        let during_snooze = daytime + chrono::Duration::hours(1);
        let (allowed, reason) = gate.can_deliver("calendar", "hash_3", false, during_snooze);
        assert!(!allowed);
        assert_eq!(reason, Some("Notifications snoozed"));

        // Urgency bypass during snooze
        let (allowed, _) = gate.can_deliver("calendar", "hash_3", true, during_snooze);
        assert!(allowed);

        // After snooze expires
        let after_snooze = future_wake + chrono::Duration::minutes(5);
        let (allowed, _) = gate.can_deliver("calendar", "hash_4", false, after_snooze);
        assert!(allowed);

        // Month-spanning date rollover test (same day-of-month across months)
        let nov_3 = Utc.with_ymd_and_hms(2026, 11, 3, 14, 0, 0).unwrap();
        let dec_3 = Utc.with_ymd_and_hms(2026, 12, 3, 14, 0, 0).unwrap();
        let _ = gate.can_deliver("calendar", "h_nov", false, nov_3);
        gate.record_delivery("h_nov");
        // Advancing to Dec 3 should properly reset sent_today even though day is 3 in both
        let (allowed, _) = gate.can_deliver("calendar", "h_dec", false, dec_3);
        assert!(allowed);
    }
}
