use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub timestamp: String,
    pub event_type: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub tokens: Option<u32>,
    pub cost_usd: Option<f64>,
    pub action_summary: Option<String>,
    pub details: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct AuditLogger {
    log_path: Option<PathBuf>,
    in_memory: Arc<Mutex<Vec<AuditEntry>>>,
}

impl AuditLogger {
    pub fn in_memory() -> Self {
        Self {
            log_path: None,
            in_memory: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn with_file(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let p = path.as_ref().to_path_buf();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut loaded = Vec::new();
        if p.exists()
            && let Ok(file) = std::fs::File::open(&p)
        {
            let reader = std::io::BufReader::new(file);
            for line in std::io::BufRead::lines(reader).map_while(Result::ok) {
                let trimmed = line.trim();
                if !trimmed.is_empty()
                    && let Ok(entry) = serde_json::from_str::<AuditEntry>(trimmed)
                {
                    loaded.push(entry);
                }
            }
        }

        Ok(Self {
            log_path: Some(p),
            in_memory: Arc::new(Mutex::new(loaded)),
        })
    }

    pub fn default_logger() -> std::io::Result<Self> {
        let dir = if let Ok(custom) = std::env::var("GROK_MUSE_DATA_DIR") {
            PathBuf::from(custom)
        } else if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join("Library/Application Support/GrokMuse")
        } else {
            PathBuf::from(".")
        };
        Self::with_file(dir.join("audit.jsonl"))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn log(
        &self,
        event_type: impl Into<String>,
        provider: Option<String>,
        model: Option<String>,
        tokens: Option<u32>,
        cost_usd: Option<f64>,
        action_summary: Option<String>,
        details: serde_json::Value,
    ) {
        let entry = AuditEntry {
            timestamp: Utc::now().to_rfc3339(),
            event_type: event_type.into(),
            provider,
            model,
            tokens,
            cost_usd,
            action_summary,
            details,
        };

        self.in_memory.lock().push(entry.clone());

        if let Some(path) = &self.log_path {
            let _ = (|| -> std::io::Result<()> {
                let line = serde_json::to_string(&entry)?;
                let mut f = OpenOptions::new().create(true).append(true).open(path)?;
                writeln!(f, "{line}")?;
                Ok(())
            })();
        }
    }

    pub fn entries(&self) -> Vec<AuditEntry> {
        self.in_memory.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audit_logging() {
        let logger = AuditLogger::in_memory();
        logger.log(
            "model_call",
            Some("meta".into()),
            Some("muse-spark".into()),
            Some(120),
            Some(0.002),
            None,
            serde_json::json!({"prompt_length": 85}),
        );
        let entries = logger.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event_type, "model_call");
        assert_eq!(entries[0].tokens, Some(120));
    }

    #[test]
    fn test_audit_logging_file_persistence() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("gm_audit_test_{nanos}"));
        let file_path = temp_dir.join("audit.jsonl");

        let logger1 = AuditLogger::with_file(&file_path).unwrap();
        logger1.log(
            "task_done",
            None,
            None,
            Some(50),
            Some(0.001),
            Some("Task A completed".into()),
            serde_json::json!({}),
        );
        assert_eq!(logger1.entries().len(), 1);

        // Reopen same file
        let logger2 = AuditLogger::with_file(&file_path).unwrap();
        let entries2 = logger2.entries();
        assert_eq!(entries2.len(), 1);
        assert_eq!(
            entries2[0].action_summary.as_deref(),
            Some("Task A completed")
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }
}
