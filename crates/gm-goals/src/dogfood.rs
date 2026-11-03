use chrono::{DateTime, Utc};
use gm_core::error::{GmError, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum DogfoodEvent {
    NoticeDelivered {
        notice_id: String,
        topic: String,
        title: String,
        is_urgent: bool,
        dedup_hash: String,
        cost_usd: f64,
        timestamp: DateTime<Utc>,
    },
    UserFeedback {
        notice_id: String,
        topic: String,
        action: String, // "done", "later", "stop"
        useful: Option<bool>,
        timestamp: DateTime<Utc>,
    },
    NoticeRated {
        notice_id: String,
        useful: bool,
        timestamp: DateTime<Utc>,
    },
    DailySpend {
        date: String, // "YYYY-MM-DD"
        spend_usd: f64,
        cap_usd: f64,
        timestamp: DateTime<Utc>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DogfoodMetricsSummary {
    pub total_notices_delivered: u32,
    pub total_rated_notices: u32,
    pub useful_ratings_count: u32,
    pub not_useful_ratings_count: u32,
    pub interruption_precision_pct: f64,
    pub active_days_count: u32,
    pub avg_notices_per_day: f64,
    pub total_spend_usd: f64,
    pub daily_spend_cap_usd: f64,
    pub max_daily_spend_usd: f64,
    pub spend_cap_enforced: bool,
    pub g6_gate_passed: bool,
    pub feedback_done_count: u32,
    pub feedback_later_count: u32,
    pub feedback_stop_count: u32,
    pub topic_counts: HashMap<String, u32>,
}

#[derive(Debug, Clone)]
pub struct DogfoodTracker {
    file_path: Option<PathBuf>,
    events: Arc<Mutex<Vec<DogfoodEvent>>>,
    daily_spend_cap_usd: f64,
}

impl Default for DogfoodTracker {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl DogfoodTracker {
    pub fn in_memory() -> Self {
        Self {
            file_path: None,
            events: Arc::new(Mutex::new(Vec::new())),
            daily_spend_cap_usd: 1.0,
        }
    }

    pub fn with_file(path: impl AsRef<Path>, daily_spend_cap_usd: f64) -> Result<Self> {
        let p = path.as_ref().to_path_buf();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut loaded_events = Vec::new();
        if p.exists() {
            let f = std::fs::File::open(&p)?;
            let reader = BufReader::new(f);
            for line in reader.lines() {
                let l = line?;
                let trimmed = l.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(ev) = serde_json::from_str::<DogfoodEvent>(trimmed) {
                    loaded_events.push(ev);
                }
            }
        }

        Ok(Self {
            file_path: Some(p),
            events: Arc::new(Mutex::new(loaded_events)),
            daily_spend_cap_usd,
        })
    }

    pub fn default_tracker(daily_spend_cap_usd: f64) -> Result<Self> {
        let dir = if let Ok(custom) = std::env::var("GROK_MUSE_DATA_DIR") {
            PathBuf::from(custom)
        } else if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join("Library/Application Support/GrokMuse")
        } else {
            PathBuf::from(".")
        };

        let file = dir.join("dogfood_metrics.jsonl");
        Self::with_file(file, daily_spend_cap_usd)
    }

    pub fn file_path(&self) -> Option<&Path> {
        self.file_path.as_deref()
    }

    pub fn record_event(&self, event: DogfoodEvent) -> Result<()> {
        self.events.lock().push(event.clone());

        if let Some(ref path) = self.file_path {
            let line = serde_json::to_string(&event)?;
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(GmError::Io)?;
            writeln!(f, "{line}").map_err(GmError::Io)?;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_notice_delivered(
        &self,
        notice_id: &str,
        topic: &str,
        title: &str,
        is_urgent: bool,
        dedup_hash: &str,
        cost_usd: f64,
        timestamp: DateTime<Utc>,
    ) -> Result<()> {
        self.record_event(DogfoodEvent::NoticeDelivered {
            notice_id: notice_id.to_string(),
            topic: topic.to_string(),
            title: title.to_string(),
            is_urgent,
            dedup_hash: dedup_hash.to_string(),
            cost_usd,
            timestamp,
        })
    }

    pub fn record_feedback(
        &self,
        notice_id: &str,
        topic: &str,
        action: &str,
        useful: Option<bool>,
        timestamp: DateTime<Utc>,
    ) -> Result<()> {
        self.record_event(DogfoodEvent::UserFeedback {
            notice_id: notice_id.to_string(),
            topic: topic.to_string(),
            action: action.to_string(),
            useful,
            timestamp,
        })
    }

    pub fn record_rating(
        &self,
        notice_id: &str,
        useful: bool,
        timestamp: DateTime<Utc>,
    ) -> Result<()> {
        self.record_event(DogfoodEvent::NoticeRated {
            notice_id: notice_id.to_string(),
            useful,
            timestamp,
        })
    }

    pub fn record_daily_spend(
        &self,
        date: &str,
        spend_usd: f64,
        cap_usd: f64,
        timestamp: DateTime<Utc>,
    ) -> Result<()> {
        self.record_event(DogfoodEvent::DailySpend {
            date: date.to_string(),
            spend_usd,
            cap_usd,
            timestamp,
        })
    }

    pub fn get_events(&self) -> Vec<DogfoodEvent> {
        self.events.lock().clone()
    }

    pub fn compute_metrics(&self) -> DogfoodMetricsSummary {
        let events = self.events.lock().clone();

        let mut total_notices_delivered = 0u32;
        let mut ratings_map: HashMap<String, bool> = HashMap::new();
        let mut feedback_done = 0u32;
        let mut feedback_later = 0u32;
        let mut feedback_stop = 0u32;
        let mut topic_counts: HashMap<String, u32> = HashMap::new();
        let mut active_dates: HashSet<String> = HashSet::new();
        let mut daily_spend_map: HashMap<String, f64> = HashMap::new();
        let mut total_spend_usd = 0.0;

        for ev in &events {
            match ev {
                DogfoodEvent::NoticeDelivered {
                    notice_id: _,
                    topic,
                    cost_usd,
                    timestamp,
                    ..
                } => {
                    total_notices_delivered += 1;
                    *topic_counts.entry(topic.clone()).or_insert(0) += 1;
                    let date_str = timestamp.format("%Y-%m-%d").to_string();
                    active_dates.insert(date_str.clone());
                    *daily_spend_map.entry(date_str).or_insert(0.0) += *cost_usd;
                    total_spend_usd += *cost_usd;
                }
                DogfoodEvent::UserFeedback {
                    notice_id,
                    action,
                    useful,
                    timestamp,
                    ..
                } => {
                    let act = action.to_lowercase();
                    if act == "done" {
                        feedback_done += 1;
                    } else if act == "later" {
                        feedback_later += 1;
                    } else if act == "stop" {
                        feedback_stop += 1;
                    }
                    if let Some(is_useful) = useful {
                        ratings_map.entry(notice_id.clone()).or_insert(*is_useful);
                    }
                    let date_str = timestamp.format("%Y-%m-%d").to_string();
                    active_dates.insert(date_str);
                }
                DogfoodEvent::NoticeRated {
                    notice_id, useful, ..
                } => {
                    ratings_map.insert(notice_id.clone(), *useful);
                }
                DogfoodEvent::DailySpend {
                    date,
                    spend_usd,
                    timestamp,
                    ..
                } => {
                    active_dates.insert(date.clone());
                    active_dates.insert(timestamp.format("%Y-%m-%d").to_string());
                    let entry = daily_spend_map.entry(date.clone()).or_insert(0.0);
                    if *spend_usd > *entry {
                        *entry = *spend_usd;
                    }
                }
            }
        }

        let mut useful_count = 0u32;
        let mut not_useful_count = 0u32;
        for &is_useful in ratings_map.values() {
            if is_useful {
                useful_count += 1;
            } else {
                not_useful_count += 1;
            }
        }
        let total_rated = useful_count + not_useful_count;
        let precision_pct = if total_rated == 0 {
            100.0
        } else {
            (useful_count as f64 / total_rated as f64) * 100.0
        };

        let active_days_count = (active_dates.len() as u32).max(1);
        let avg_notices_per_day = (total_notices_delivered as f64) / (active_days_count as f64);

        let max_daily_spend_usd = daily_spend_map
            .values()
            .cloned()
            .fold(0.0f64, |acc, v| if v > acc { v } else { acc });

        // Cap enforced if peak daily spend does not exceed cap (with 0.0001 float margin)
        let spend_cap_enforced = max_daily_spend_usd <= (self.daily_spend_cap_usd + 0.0001);

        // G6 Gate: precision >= 80%, notices/day <= 3, daily spend cap respected
        let g6_gate_passed =
            precision_pct >= 80.0 && avg_notices_per_day <= 3.0 && spend_cap_enforced;

        DogfoodMetricsSummary {
            total_notices_delivered,
            total_rated_notices: total_rated,
            useful_ratings_count: useful_count,
            not_useful_ratings_count: not_useful_count,
            interruption_precision_pct: precision_pct,
            active_days_count,
            avg_notices_per_day,
            total_spend_usd,
            daily_spend_cap_usd: self.daily_spend_cap_usd,
            max_daily_spend_usd,
            spend_cap_enforced,
            g6_gate_passed,
            feedback_done_count: feedback_done,
            feedback_later_count: feedback_later,
            feedback_stop_count: feedback_stop,
            topic_counts,
        }
    }

    pub fn format_report(&self) -> String {
        let summary = self.compute_metrics();
        let mut out = String::new();
        out.push_str("\n============================================================\n");
        out.push_str("             Grok-Muse Dogfood Metrics (G6 Gate)            \n");
        out.push_str("============================================================\n");

        let precision_status = if summary.interruption_precision_pct >= 80.0 {
            "[PASS]"
        } else {
            "[FAIL]"
        };
        out.push_str(&format!(
            "Interruption Precision:   {:.1}% (Target: ≥ 80.0%) {}\n",
            summary.interruption_precision_pct, precision_status
        ));

        let delivery_status = if summary.avg_notices_per_day <= 3.0 {
            "[PASS]"
        } else {
            "[FAIL]"
        };
        out.push_str(&format!(
            "Daily Notice Rate:        {:.1} notices/day (Target: ≤ 3.0/day) {}\n",
            summary.avg_notices_per_day, delivery_status
        ));

        let spend_status = if summary.spend_cap_enforced {
            "[PASS]"
        } else {
            "[FAIL]"
        };
        out.push_str(&format!(
            "Spend Cap Enforcement:    Peak ${:.4}/day vs ${:.2} cap {}\n",
            summary.max_daily_spend_usd, summary.daily_spend_cap_usd, spend_status
        ));

        out.push_str("------------------------------------------------------------\n");
        out.push_str(&format!(
            "Total Notices Delivered:  {} (over {} active day{})\n",
            summary.total_notices_delivered,
            summary.active_days_count,
            if summary.active_days_count == 1 {
                ""
            } else {
                "s"
            }
        ));
        out.push_str(&format!(
            "Notice Ratings:           {} useful / {} not useful ({} total rated)\n",
            summary.useful_ratings_count,
            summary.not_useful_ratings_count,
            summary.total_rated_notices
        ));
        out.push_str(&format!(
            "User Feedback Actions:    Done: {}, Later: {}, Stop: {}\n",
            summary.feedback_done_count, summary.feedback_later_count, summary.feedback_stop_count
        ));
        out.push_str(&format!(
            "Total Cumulative Spend:   ${:.4} USD\n",
            summary.total_spend_usd
        ));

        if !summary.topic_counts.is_empty() {
            out.push_str("Topics Delivered:         ");
            let mut parts = Vec::new();
            for (topic, count) in &summary.topic_counts {
                parts.push(format!("{topic}: {count}"));
            }
            out.push_str(&parts.join(", "));
            out.push('\n');
        }

        out.push_str("------------------------------------------------------------\n");
        let overall = if summary.g6_gate_passed {
            "PASSED (All G6 criteria satisfied)"
        } else {
            "ACTION NEEDED (G6 criteria not yet fully met)"
        };
        out.push_str(&format!("G6 Dogfood Gate Status:   {}\n", overall));
        out.push_str("============================================================\n");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn test_dogfood_metrics_tracking_and_g6_gate() {
        let tracker = DogfoodTracker::in_memory();

        let t1 = Utc.with_ymd_and_hms(2026, 11, 1, 10, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2026, 11, 2, 11, 0, 0).unwrap();

        // 1. Deliver 4 notices over 2 days (2 notices/day <= 3.0 cap)
        tracker
            .record_notice_delivered(
                "n1",
                "calendar",
                "Meeting with Team",
                false,
                "cal_1",
                0.001,
                t1,
            )
            .unwrap();
        tracker
            .record_notice_delivered(
                "n2",
                "reminders",
                "Buy Groceries",
                false,
                "rem_1",
                0.001,
                t1,
            )
            .unwrap();
        tracker
            .record_notice_delivered("n3", "mail", "Urgent Budget", true, "mail_1", 0.001, t2)
            .unwrap();
        tracker
            .record_notice_delivered("n4", "calendar", "Dentist Visit", false, "cal_2", 0.001, t2)
            .unwrap();

        // 2. Feedback and ratings: 3 useful, 1 not useful -> 75% precision
        tracker
            .record_feedback("n1", "calendar", "done", Some(true), t1)
            .unwrap();
        tracker
            .record_feedback("n2", "reminders", "later", Some(true), t1)
            .unwrap();
        tracker.record_rating("n3", true, t2).unwrap();
        tracker.record_rating("n4", false, t2).unwrap();

        let metrics = tracker.compute_metrics();
        assert_eq!(metrics.total_notices_delivered, 4);
        assert_eq!(metrics.useful_ratings_count, 3);
        assert_eq!(metrics.not_useful_ratings_count, 1);
        assert!((metrics.interruption_precision_pct - 75.0).abs() < 0.001);
        assert!(!metrics.g6_gate_passed); // 75% < 80%

        // 3. User rates another notice useful, bringing precision to 4/5 = 80.0%
        let t3 = Utc.with_ymd_and_hms(2026, 11, 3, 9, 0, 0).unwrap();
        tracker
            .record_notice_delivered(
                "n5",
                "calendar",
                "Architecture Sync",
                true,
                "cal_3",
                0.001,
                t3,
            )
            .unwrap();
        tracker.record_rating("n5", true, t3).unwrap();

        let updated = tracker.compute_metrics();
        assert_eq!(updated.total_notices_delivered, 5);
        assert_eq!(updated.useful_ratings_count, 4);
        assert_eq!(updated.not_useful_ratings_count, 1);
        assert!((updated.interruption_precision_pct - 80.0).abs() < 0.001);
        assert!(updated.avg_notices_per_day <= 3.0);
        assert!(updated.spend_cap_enforced);
        assert!(updated.g6_gate_passed);

        let report = tracker.format_report();
        assert!(report.contains("G6 Dogfood Gate Status:   PASSED"));
    }

    #[test]
    fn test_dogfood_tracker_file_persistence() {
        let temp_dir =
            std::env::temp_dir().join(format!("gm_dogfood_test_{}", uuid::Uuid::new_v4()));
        let file_path = temp_dir.join("dogfood_metrics.jsonl");

        let tracker1 = DogfoodTracker::with_file(&file_path, 1.0).unwrap();
        tracker1
            .record_notice_delivered(
                "persisted_1",
                "calendar",
                "Team Standup",
                false,
                "cal_persist",
                0.002,
                Utc::now(),
            )
            .unwrap();
        tracker1
            .record_feedback("persisted_1", "calendar", "done", Some(true), Utc::now())
            .unwrap();

        // Reload from same file
        let tracker2 = DogfoodTracker::with_file(&file_path, 1.0).unwrap();
        let metrics = tracker2.compute_metrics();
        assert_eq!(metrics.total_notices_delivered, 1);
        assert_eq!(metrics.useful_ratings_count, 1);
        assert_eq!(metrics.feedback_done_count, 1);

        let _ = std::fs::remove_dir_all(temp_dir);
    }
}
