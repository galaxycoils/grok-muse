//! User-initiated native file imports. Exposed only over local authenticated IPC.
use crate::OpenBotSupervisor;
use gm_core::error::{GmError, Result};
use std::{io::Read, path::Path};
impl OpenBotSupervisor {
    pub fn import_attachment(&self, bot_id: &str, source: &Path) -> Result<String> {
        let sandbox = self.roster().workspace_sandbox(bot_id)?;
        if std::fs::symlink_metadata(source)?.file_type().is_symlink() {
            return Err(GmError::SafetyViolation(
                "Attachment imports reject symbolic links".into(),
            ));
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options.open(source)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > 8 * 1024 * 1024 {
            return Err(GmError::Config(
                "Attachments must be regular files under 8 MiB".into(),
            ));
        }
        let mut bytes = vec![];
        file.take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err(GmError::Config("Attachment exceeds 8 MiB".into()));
        }
        let original = source
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");
        let clean: String = original
            .chars()
            .take(80)
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let name = format!("attachment-{}-{clean}", uuid::Uuid::new_v4());
        sandbox.receive_artifact(std::ffi::OsStr::new(&name), &bytes)?;
        Ok(format!("inbox/{name}"))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn user_attachment_import_is_unique_and_rejects_special_paths() {
        let root = std::env::temp_dir().join(format!("gm-attachment-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let config = gm_core::config::GmConfig {
            storage_dir: root.join("data"),
            ..Default::default()
        };
        let (tx, _) = tokio::sync::broadcast::channel(8);
        let supervisor = OpenBotSupervisor::open(
            config,
            std::sync::Arc::new(gm_safety::AuditLogger::in_memory()),
            tx,
        )
        .unwrap();
        let source = root.join("report.txt");
        std::fs::write(&source, b"user report").unwrap();
        let first = supervisor.import_attachment("researcher", &source).unwrap();
        let second = supervisor.import_attachment("researcher", &source).unwrap();
        assert_ne!(first, second);
        let sandbox = supervisor.roster().workspace_sandbox("researcher").unwrap();
        assert_eq!(sandbox.preview_shared_file(&first).unwrap(), b"user report");
        assert!(sandbox.preview_shared_file("outbox/../../escape").is_err());
        assert_eq!(sandbox.shared_files().unwrap().len(), 2);
        assert!(supervisor.import_attachment("unknown", &source).is_err());
        assert!(supervisor.import_attachment("researcher", &root).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&source, root.join("link")).unwrap();
            assert!(
                supervisor
                    .import_attachment("researcher", &root.join("link"))
                    .is_err()
            );
        }
        drop(sandbox);
        drop(supervisor);
        let _ = std::fs::remove_dir_all(root);
    }
}
