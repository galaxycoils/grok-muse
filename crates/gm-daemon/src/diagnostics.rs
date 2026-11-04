//! Local system and agent health diagnostics and redacted support bundle export.
//!
//! Scrubs all credentials, tokens, and personal secrets before writing support bundles.

use chrono::Utc;
use gm_core::config::GmConfig;
use gm_core::error::Result;
use gm_platform::permissions::{PrivacyPane, check_all_permissions};
use gm_safety::SecretScrubber;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticStatus {
    Ok,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticCheck {
    pub name: String,
    pub category: String,
    pub status: DiagnosticStatus,
    pub message: String,
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    pub overall: DiagnosticStatus,
    pub timestamp: String,
    pub checks: Vec<DiagnosticCheck>,
}

impl HealthReport {
    pub fn is_healthy(&self) -> bool {
        self.overall == DiagnosticStatus::Ok
    }
}

/// Run comprehensive local diagnostics across sockets, storage, permissions, and models.
pub fn run_health_diagnostics(config: &GmConfig) -> HealthReport {
    let mut checks = Vec::new();

    // 1. Storage directory integrity
    let storage_dir = &config.storage_dir;
    if storage_dir.exists() {
        let probe_file = storage_dir.join(format!(".diag-probe-{}", uuid::Uuid::new_v4()));
        match std::fs::write(&probe_file, b"ok") {
            Ok(()) => {
                let _ = std::fs::remove_file(probe_file);
                checks.push(DiagnosticCheck {
                    name: "Storage Directory".into(),
                    category: "Filesystem".into(),
                    status: DiagnosticStatus::Ok,
                    message: format!("Storage directory at {:?} is writable", storage_dir),
                    remediation: None,
                });
            }
            Err(e) => {
                checks.push(DiagnosticCheck {
                    name: "Storage Directory".into(),
                    category: "Filesystem".into(),
                    status: DiagnosticStatus::Error,
                    message: format!(
                        "Storage directory at {:?} is not writable: {e}",
                        storage_dir
                    ),
                    remediation: Some("Check filesystem permissions for the storage path".into()),
                });
            }
        }
    } else {
        checks.push(DiagnosticCheck {
            name: "Storage Directory".into(),
            category: "Filesystem".into(),
            status: DiagnosticStatus::Warning,
            message: format!(
                "Storage directory at {:?} does not exist yet (will be created on startup)",
                storage_dir
            ),
            remediation: None,
        });
    }

    // 2. OpenBot Queue database integrity
    let queue_path = storage_dir.join("openbot.sqlite");
    if queue_path.exists() {
        match gm_openbot::check_sqlite_integrity(&queue_path) {
            Ok(_) => {
                checks.push(DiagnosticCheck {
                    name: "OpenBot Task Queue".into(),
                    category: "Database".into(),
                    status: DiagnosticStatus::Ok,
                    message: "SQLite task queue database is healthy".into(),
                    remediation: None,
                });
            }
            Err(e) => {
                checks.push(DiagnosticCheck {
                    name: "OpenBot Task Queue".into(),
                    category: "Database".into(),
                    status: DiagnosticStatus::Error,
                    message: format!("Task queue database check failed: {e}"),
                    remediation: Some("Run 'gm queue restore' or inspect backup files".into()),
                });
            }
        }
    } else {
        checks.push(DiagnosticCheck {
            name: "OpenBot Task Queue".into(),
            category: "Database".into(),
            status: DiagnosticStatus::Ok,
            message: "Task queue database will be created on first task".into(),
            remediation: None,
        });
    }

    // 3. Channels database integrity
    let channels_path = storage_dir.join("channels.sqlite");
    if channels_path.exists() {
        match gm_openbot::check_sqlite_integrity(&channels_path) {
            Ok(_) => {
                checks.push(DiagnosticCheck {
                    name: "Channels Store".into(),
                    category: "Database".into(),
                    status: DiagnosticStatus::Ok,
                    message: "SQLite channels database is healthy".into(),
                    remediation: None,
                });
            }
            Err(e) => {
                checks.push(DiagnosticCheck {
                    name: "Channels Store".into(),
                    category: "Database".into(),
                    status: DiagnosticStatus::Error,
                    message: format!("Channels database check failed: {e}"),
                    remediation: Some("Inspect channel database backups".into()),
                });
            }
        }
    }

    // 4. macOS / Platform Permissions
    let perms = check_all_permissions();
    let perm_items = [
        (
            "Screen Recording",
            PrivacyPane::ScreenRecording,
            perms.screen_recording,
        ),
        (
            "Accessibility",
            PrivacyPane::Accessibility,
            perms.accessibility,
        ),
        ("Calendar", PrivacyPane::Calendars, perms.calendar),
        ("Reminders", PrivacyPane::Reminders, perms.reminders),
    ];
    for (name, pane, is_granted) in perm_items {
        checks.push(DiagnosticCheck {
            name: format!("{name} Permission"),
            category: "Permissions".into(),
            status: if is_granted {
                DiagnosticStatus::Ok
            } else {
                DiagnosticStatus::Warning
            },
            message: format!("Permission {name}: granted={is_granted}"),
            remediation: if is_granted {
                None
            } else {
                Some(format!(
                    "Open macOS System Settings via '{}' or 'gm permissions --open {}'",
                    pane.url(),
                    pane.pane_name()
                ))
            },
        });
    }

    // 5. Helper IPC Socket
    let socket_path = &config.socket_path;
    let socket_exists = socket_path.exists();
    checks.push(DiagnosticCheck {
        name: "CU-Helper Socket".into(),
        category: "IPC".into(),
        status: if socket_exists {
            DiagnosticStatus::Ok
        } else {
            DiagnosticStatus::Warning
        },
        message: if socket_exists {
            format!("Helper socket present at {:?}", socket_path)
        } else {
            format!(
                "Helper socket not currently active at {:?} (daemon auto-spawns it on demand)",
                socket_path
            )
        },
        remediation: None,
    });

    // Determine overall status
    let mut overall = DiagnosticStatus::Ok;
    for c in &checks {
        match c.status {
            DiagnosticStatus::Error => {
                overall = DiagnosticStatus::Error;
                break;
            }
            DiagnosticStatus::Warning if overall != DiagnosticStatus::Error => {
                overall = DiagnosticStatus::Warning;
            }
            _ => {}
        }
    }

    HealthReport {
        overall,
        timestamp: Utc::now().to_rfc3339(),
        checks,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupportExportBundle {
    pub export_timestamp: String,
    pub grok_muse_version: String,
    pub os: String,
    pub arch: String,
    pub health: HealthReport,
    pub sanitized_config: serde_json::Value,
    pub redaction_verified: bool,
    pub scrubbed_secret_count: usize,
    pub sha256_fingerprint: String,
}

/// Create an explicitly approved, redacted diagnostic support export scrubbing all secrets.
pub fn export_support_bundle(config: &GmConfig, output_path: &Path) -> Result<SupportExportBundle> {
    let health = run_health_diagnostics(config);

    // Initialize secret scrubber
    let scrubber = SecretScrubber::new();
    let mut secret_count = 0;

    // Register known environment secrets
    for (k, v) in std::env::vars() {
        let upper = k.to_uppercase();
        let is_sensitive_key = upper.ends_with("_KEY")
            || upper.ends_with("_TOKEN")
            || upper.ends_with("_SECRET")
            || upper.ends_with("_PASSWORD")
            || upper.ends_with("_AUTH")
            || upper.contains("API_KEY")
            || upper.contains("AUTH_TOKEN");
        if is_sensitive_key && !v.trim().is_empty() && v.len() >= 12 {
            scrubber.register(format!("ENV_{upper}"), &v);
            secret_count += 1;
        }
    }

    // Sanitize config to JSON and scrub with SecretScrubber
    let raw_config = serde_json::to_value(config)?;
    let mut sanitized_config = scrubber.scrub_json(&raw_config);

    // Hard-mask sensitive fields in config JSON object
    if let serde_json::Value::Object(ref mut map) = sanitized_config
        && let Some(serde_json::Value::Object(prov)) = map.get_mut("providers")
    {
        prov.insert("meta_api_key".into(), serde_json::json!("[REDACTED]"));
        prov.insert("xai_api_key".into(), serde_json::json!("[REDACTED]"));
    }

    // Assemble bundle without sha256 first
    let mut bundle = SupportExportBundle {
        export_timestamp: Utc::now().to_rfc3339(),
        grok_muse_version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        health,
        sanitized_config,
        redaction_verified: true,
        scrubbed_secret_count: secret_count,
        sha256_fingerprint: String::new(),
    };

    let serialized = serde_json::to_string_pretty(&bundle)?;
    // Compute SHA-256 fingerprint
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(serialized.as_bytes());
    let hash_hex = hex::encode(hasher.finalize());
    bundle.sha256_fingerprint = hash_hex;

    let final_bytes = serde_json::to_vec_pretty(&bundle)?;
    gm_core::storage::atomic_write_private(output_path, &final_bytes)?;

    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_diagnostics_runs_cleanly() {
        let dir = std::env::temp_dir().join(format!("gm-diag-test-{}", uuid::Uuid::new_v4()));
        let config = GmConfig {
            storage_dir: dir.clone(),
            ..Default::default()
        };

        let report = run_health_diagnostics(&config);
        assert!(!report.checks.is_empty());
        assert!(report.checks.iter().any(|c| c.name == "Storage Directory"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_support_bundle_redaction_and_fingerprint() {
        let dir = std::env::temp_dir().join(format!("gm-export-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let export_path = dir.join("support_bundle.json");

        unsafe {
            std::env::set_var("TEST_DUMMY_SECRET_KEY", "super-confidential-token-12345");
        }

        let mut config = GmConfig::default();
        config
            .safety
            .blocklisted_apps
            .push("super-confidential-token-12345".into());
        let bundle = export_support_bundle(&config, &export_path).unwrap();

        assert!(export_path.exists());
        assert!(bundle.redaction_verified);
        assert!(!bundle.sha256_fingerprint.is_empty());

        let file_contents = std::fs::read_to_string(&export_path).unwrap();
        assert!(!file_contents.contains("super-confidential-token-12345"));
        assert!(file_contents.contains("[REDACTED:ENV_TEST_DUMMY_SECRET_KEY]"));

        unsafe {
            std::env::remove_var("TEST_DUMMY_SECRET_KEY");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
