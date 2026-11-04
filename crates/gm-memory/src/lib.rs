pub mod upkeep;
pub use upkeep::*;

pub use darius_memory::{MemoryEngine, NewRecord, Record, RecordKind, SearchQuery};
use gm_core::error::{GmError, Result};
use parking_lot::Mutex;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone)]
pub struct PersonalMemoryStore {
    engine: Arc<Mutex<MemoryEngine>>,
}

impl PersonalMemoryStore {
    pub fn open(db_path: impl AsRef<Path>) -> Result<Self> {
        let engine = MemoryEngine::open(db_path.as_ref())
            .map_err(|e| GmError::Platform(format!("Failed to open memory db: {e}")))?;
        Ok(Self {
            engine: Arc::new(Mutex::new(engine)),
        })
    }

    pub fn in_memory() -> Result<Self> {
        let engine = MemoryEngine::open_in_memory()
            .map_err(|e| GmError::Platform(format!("Failed to open in-memory db: {e}")))?;
        Ok(Self {
            engine: Arc::new(Mutex::new(engine)),
        })
    }

    pub fn default_store() -> Result<Self> {
        let dir = if let Ok(custom) = std::env::var("GROK_MUSE_DATA_DIR") {
            std::path::PathBuf::from(custom)
        } else {
            gm_core::config::GmConfig::load(None)?.storage_dir
        };
        gm_core::storage::ensure_private_dir(&dir)?;
        Self::open(dir.join("memory.sqlite"))
    }

    pub fn record_fact(&self, title: Option<&str>, body: &str, tags: &[&str]) -> Result<Record> {
        let rec = NewRecord {
            kind: RecordKind::Fact,
            title: title.map(|s| s.to_string()),
            body: body.to_string(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            importance: 0.8,
            source: Some("proactive".into()),
        };
        self.engine
            .lock()
            .upsert(rec)
            .map_err(|e| GmError::Platform(e.to_string()))
    }

    pub fn record_preference(&self, preference: &str, tags: &[&str]) -> Result<Record> {
        let rec = NewRecord {
            kind: RecordKind::Preference,
            title: None,
            body: preference.to_string(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            importance: 0.9,
            source: Some("user".into()),
        };
        self.engine
            .lock()
            .upsert(rec)
            .map_err(|e| GmError::Platform(e.to_string()))
    }

    pub fn record_observation(
        &self,
        title: Option<&str>,
        body: &str,
        tags: &[&str],
        importance: f32,
    ) -> Result<Record> {
        let rec = NewRecord {
            kind: RecordKind::Episode,
            title: title.map(|s| s.to_string()),
            body: body.to_string(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            importance,
            source: Some("observation".into()),
        };
        self.engine
            .lock()
            .upsert(rec)
            .map_err(|e| GmError::Platform(e.to_string()))
    }

    pub fn delete_record(&self, id: &str) -> Result<bool> {
        self.engine
            .lock()
            .delete(id)
            .map_err(|e| GmError::Platform(e.to_string()))
    }

    pub fn update_tags(&self, id: &str, tags: &[String]) -> Result<bool> {
        self.engine
            .lock()
            .update_tags(id, tags)
            .map_err(|e| GmError::Platform(e.to_string()))
    }

    pub fn all_records(&self, limit: usize) -> Result<Vec<Record>> {
        self.engine
            .lock()
            .all_records(limit)
            .map_err(|e| GmError::Platform(e.to_string()))
    }

    pub fn perform_upkeep(
        &self,
        config: &UpkeepConfig,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<MemoryUpkeepReport> {
        run_memory_upkeep(self, config, now)
    }

    pub fn search(&self, query_str: &str, limit: usize) -> Result<Vec<Record>> {
        let text = if query_str.trim().is_empty() {
            None
        } else {
            Some(query_str.to_string())
        };
        let query = SearchQuery {
            text,
            kinds: vec![],
            limit,
        };
        self.engine
            .lock()
            .search(&query)
            .map_err(|e| GmError::Platform(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    #[test]
    fn test_personal_memory() {
        let mem = PersonalMemoryStore::in_memory().unwrap();
        mem.record_fact(
            Some("Meeting preference"),
            "User prefers meetings after 10 AM",
            &["calendar", "schedule"],
        )
        .unwrap();

        let results = mem.search("meetings", 5).unwrap();
        assert_eq!(results.len(), 1);
        assert!(
            results[0]
                .body
                .contains("User prefers meetings after 10 AM")
        );
    }

    #[test]
    fn test_personal_memory_sqlite_persistence() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("gm_mem_test_{nanos}"));
        let db_path = temp_dir.join("memory.sqlite");
        std::fs::create_dir_all(&temp_dir).unwrap();

        let mem1 = PersonalMemoryStore::open(&db_path).unwrap();
        mem1.record_fact(
            Some("Project note"),
            "Working on Grok-Muse v2 enhancements",
            &["project", "muse"],
        )
        .unwrap();

        // Reopen same database file
        let mem2 = PersonalMemoryStore::open(&db_path).unwrap();
        let results = mem2.search("Grok-Muse", 5).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].body.contains("Grok-Muse v2 enhancements"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_memory_upkeep_deduplication_and_clustering() {
        let mem = PersonalMemoryStore::in_memory().unwrap();
        let now = Utc::now();

        // 1. Insert duplicate facts with slightly different wording / same core
        mem.record_fact(
            Some("User Focus"),
            "User prefers quiet work mornings before 11 AM",
            &["focus", "schedule"],
        )
        .unwrap();

        mem.record_fact(
            Some("Focus Time"),
            "User prefers quiet work mornings before 11 AM.",
            &["focus", "productivity"],
        )
        .unwrap();

        // 2. Insert related records sharing the 'project' theme
        mem.record_fact(
            Some("Project Alpha Roadmap"),
            "Project Alpha milestone 1 scheduled for next Friday",
            &["project", "alpha"],
        )
        .unwrap();

        mem.record_fact(
            Some("Project Alpha Tech Stack"),
            "Project Alpha backend is built in Rust with async tokio",
            &["project", "alpha", "rust"],
        )
        .unwrap();

        // 3. Perform upkeep
        let report = mem.perform_upkeep(&UpkeepConfig::default(), now).unwrap();

        assert_eq!(report.records_scanned, 4);
        assert_eq!(report.duplicates_removed, 1);
        assert!(report.clusters_formed >= 1);
        assert_eq!(report.total_records_retained, 3);

        // Verify remaining focus record merged the tags
        let focus_records = mem.search("quiet work mornings", 5).unwrap();
        assert_eq!(focus_records.len(), 1);
        let tags = &focus_records[0].tags;
        assert!(tags.iter().any(|t| t == "productivity"));
        assert!(tags.iter().any(|t| t == "schedule"));

        let formatted = report.format_summary();
        assert!(formatted.contains("Duplicate Facts Removed:  1"));
        assert!(formatted.contains("Active Knowledge Clusters"));
    }

    #[test]
    fn test_memory_upkeep_stale_observation_expiry() {
        let mem = PersonalMemoryStore::in_memory().unwrap();

        // Record a low-importance observation
        let obs = mem
            .record_observation(
                Some("Window Check"),
                "User had Safari open at tab 3",
                &["observation", "transient"],
                0.2,
            )
            .unwrap();

        // Record a high-importance fact
        mem.record_fact(
            Some("Important Key"),
            "Production server is accessible via VPN",
            &["security"],
        )
        .unwrap();

        // Upkeep with current time (not expired yet)
        let report_now = mem
            .perform_upkeep(&UpkeepConfig::default(), Utc::now())
            .unwrap();
        assert_eq!(report_now.stale_expired, 0);

        // Advance simulated time by 20 days (beyond 14 days threshold)
        let future_time = Utc::now() + Duration::days(20);
        let report_future = mem
            .perform_upkeep(&UpkeepConfig::default(), future_time)
            .unwrap();

        assert_eq!(report_future.stale_expired, 1);
        let remaining = mem.all_records(10).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_ne!(remaining[0].id, obs.id);
        assert_eq!(remaining[0].title.as_deref(), Some("Important Key"));
    }

    #[test]
    fn test_memory_upkeep_deduplication_winner_importance() {
        let mem = PersonalMemoryStore::in_memory().unwrap();
        let now = Utc::now();

        // Earlier record with lower importance
        let r1 = mem
            .engine
            .lock()
            .upsert(NewRecord {
                kind: RecordKind::Fact,
                title: Some("Initial Draft".into()),
                body: "Project roadmap deadline is December 15th".into(),
                tags: vec!["draft".into(), "v1".into()],
                importance: 0.3,
                source: Some("agent".into()),
            })
            .unwrap();

        // Later record with higher importance and additional tag
        let r2 = mem
            .engine
            .lock()
            .upsert(NewRecord {
                kind: RecordKind::Fact,
                title: Some("Final Roadmap".into()),
                body: "Project roadmap deadline is December 15th".into(),
                tags: vec!["confirmed".into()],
                importance: 0.95,
                source: Some("user".into()),
            })
            .unwrap();

        let report = mem.perform_upkeep(&UpkeepConfig::default(), now).unwrap();

        assert_eq!(report.records_scanned, 2);
        assert_eq!(report.duplicates_removed, 1);
        assert_eq!(report.total_records_retained, 1);

        let remaining = mem.all_records(10).unwrap();
        assert_eq!(remaining.len(), 1);
        // Winner should be r2 with importance 0.95 and merged tags
        assert_eq!(remaining[0].id, r2.id);
        assert_eq!(remaining[0].importance, 0.95);
        assert!(remaining[0].tags.contains(&"draft".to_string()));
        assert!(remaining[0].tags.contains(&"v1".to_string()));
        assert!(remaining[0].tags.contains(&"confirmed".to_string()));

        // r1 was deleted
        assert_ne!(remaining[0].id, r1.id);
    }
}
