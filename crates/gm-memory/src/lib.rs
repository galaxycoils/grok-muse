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

    pub fn search(&self, query_str: &str, limit: usize) -> Result<Vec<Record>> {
        let query = SearchQuery {
            text: Some(query_str.to_string()),
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
}
