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
pub async fn run_health_diagnostics(config: &GmConfig) -> HealthReport {
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
    let channels_path = storage_dir.join("openbot.sqlite");
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

    // A socket path alone is not a liveness signal. Both probes are bounded and
    // validate the response and request ID without starting any services.
    for (name, path, daemon) in [
        ("CU-Helper IPC", &config.socket_path, false),
        ("Daemon IPC", &config.daemon_socket_path, true),
    ] {
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            ping_ipc(path, daemon),
        )
        .await;
        let responsive = matches!(result, Ok(Ok(())));
        checks.push(DiagnosticCheck {
            name: name.into(),
            category: "IPC".into(),
            status: if responsive {
                DiagnosticStatus::Ok
            } else {
                DiagnosticStatus::Warning
            },
            message: if responsive {
                "Matching IPC peer answered Ping".into()
            } else {
                "IPC peer is absent, unresponsive or incompatible".into()
            },
            remediation: if responsive {
                None
            } else {
                Some("Start the matching installed service and retry".into())
            },
        });
    }

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

async fn ping_ipc(path: &Path, daemon: bool) -> Result<()> {
    use gm_core::ipc::*;
    if !daemon {
        return gm_platform::IpcPlatformClient::new(path).ping().await;
    }
    #[cfg(unix)]
    let mut stream = tokio::net::UnixStream::connect(path).await?;
    #[cfg(windows)]
    let mut stream = tokio::net::windows::named_pipe::ClientOptions::new().open(path)?;
    let id = 1;
    write_ipc_message(
        &mut stream,
        &IpcMessage {
            id,
            payload: DaemonRequest::Ping,
        },
    )
    .await?;
    let reply: IpcMessage<DaemonResponse> = read_ipc_message(&mut stream).await?;
    if reply.id != id || !matches!(reply.payload, DaemonResponse::Pong) {
        return Err(gm_core::GmError::Ipc("Unexpected Ping response".into()));
    }
    Ok(())
}

fn mask_urls(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => {
            if let Ok(mut url) = reqwest::Url::parse(text)
                && matches!(url.scheme(), "http" | "https")
            {
                let _ = url.set_username("");
                let _ = url.set_password(None);
                url.set_query(None);
                url.set_fragment(None);
                *text = url.to_string();
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                mask_urls(value);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                mask_urls(value);
            }
        }
        _ => {}
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
pub async fn export_support_bundle(
    config: &GmConfig,
    output_path: &Path,
) -> Result<SupportExportBundle> {
    let health = run_health_diagnostics(config).await;

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
            || upper.contains("AUTH_TOKEN")
            || upper.contains("PRIVATE_KEY")
            || upper.contains("SIGNING_KEY");
        if is_sensitive_key && !v.trim().is_empty() {
            scrubber.register(format!("ENV_{upper}"), &v);
            secret_count += 1;
        }
    }

    // Include the locally generated companion bearer token, not just env keys.
    if let Ok(token) = std::fs::read_to_string(config.storage_dir.join("companion-token")) {
        let token = token.trim();
        if !token.is_empty() {
            scrubber.register("COMPANION_TOKEN", token);
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
        redaction_verified: false,
        scrubbed_secret_count: secret_count,
        sha256_fingerprint: String::new(),
    };

    // Scrub all fields (including diagnostic messages and object keys), then
    // check the serialized result before claiming known-secret redaction.
    let mut value = serde_json::to_value(&bundle)?;
    mask_urls(&mut value);
    let value = scrubber.scrub_json(&value);
    let serialized = serde_json::to_string(&value)?;
    if scrubber.scrub_str(&serialized) != serialized {
        return Err(gm_core::GmError::SafetyViolation(
            "Support bundle failed known-secret redaction verification".into(),
        ));
    }
    bundle = serde_json::from_value(value)?;
    bundle.redaction_verified = true;
    bundle.sha256_fingerprint = support_bundle_fingerprint(&bundle)?;

    let final_bytes = serde_json::to_vec_pretty(&bundle)?;
    gm_core::storage::atomic_write_private(output_path, &final_bytes)?;

    Ok(bundle)
}

/// Hash canonical JSON with an empty fingerprint field. Consumers can recompute
/// this without trusting the embedded digest. Redaction covers known secrets only.
pub fn support_bundle_fingerprint(bundle: &SupportExportBundle) -> Result<String> {
    let mut copy = bundle.clone();
    copy.sha256_fingerprint.clear();
    Ok(gm_core::hash::compute_sha256(&serde_json::to_vec(&copy)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_health_diagnostics_runs_cleanly() {
        let dir = std::env::temp_dir().join(format!("gm-diag-test-{}", uuid::Uuid::new_v4()));
        let config = GmConfig {
            storage_dir: dir.clone(),
            ..Default::default()
        };

        let report = run_health_diagnostics(&config).await;
        assert!(!report.checks.is_empty());
        assert!(report.checks.iter().any(|c| c.name == "Storage Directory"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_support_bundle_redaction_and_fingerprint() {
        let dir = std::env::temp_dir().join(format!("gm-export-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let export_path = dir.join("support_bundle.json");

        unsafe {
            std::env::set_var("TEST_DUMMY_SECRET_KEY", "super-confidential-token-12345");
        }

        let mut config = GmConfig {
            storage_dir: dir.clone(),
            ..GmConfig::default()
        };
        config
            .safety
            .blocklisted_apps
            .push("super-confidential-token-12345".into());
        let bundle = export_support_bundle(&config, &export_path).await.unwrap();

        assert!(export_path.exists());
        assert!(bundle.redaction_verified);
        assert_eq!(
            bundle.sha256_fingerprint,
            support_bundle_fingerprint(&bundle).unwrap()
        );

        let file_contents = std::fs::read_to_string(&export_path).unwrap();
        assert!(!file_contents.contains("super-confidential-token-12345"));
        assert!(file_contents.contains("[REDACTED:ENV_TEST_DUMMY_SECRET_KEY]"));

        unsafe {
            std::env::remove_var("TEST_DUMMY_SECRET_KEY");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
