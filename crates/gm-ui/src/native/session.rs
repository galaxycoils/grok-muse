//! Private, bounded native conversation state. A second UI cannot overwrite it.
use gm_core::{
    error::{GmError, Result},
    storage::{atomic_write_private, ensure_private_dir},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionData {
    version: u32,
    pub messages: Vec<(String, String)>,
    pub draft: String,
    pub bot: String,
    pub task_mode: bool,
}
impl Default for SessionData {
    fn default() -> Self {
        Self {
            version: 1,
            messages: vec![],
            draft: String::new(),
            bot: "muse".into(),
            task_mode: false,
        }
    }
}
pub(super) struct SessionStore {
    pub data: Mutex<SessionData>,
    path: PathBuf,
    _lease: File,
    write_lock: Mutex<()>,
    changed: tokio::sync::watch::Sender<u64>,
}
impl SessionStore {
    pub fn open(root: &Path) -> Result<(Arc<Self>, tokio::sync::watch::Receiver<u64>)> {
        let root = root.join("native-ui");
        ensure_private_dir(&root)?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let lease = options.open(root.join("session.lock"))?;
        lease.try_lock().map_err(|_|GmError::Config("Another native window owns conversation history. This window will not save history.".into()))?;
        let path = root.join("session-v1.json");
        let data = match std::fs::read(&path) {
            Ok(bytes) => {
                if bytes.len() > 4 * 1024 * 1024 {
                    return Err(GmError::Config(
                        "Conversation file exceeds the size limit; original preserved".into(),
                    ));
                }
                let data: SessionData = serde_json::from_slice(&bytes).map_err(|e| {
                    GmError::Config(format!("Cannot read conversation; original preserved: {e}"))
                })?;
                if data.version != 1 {
                    return Err(GmError::Config(
                        "Conversation version is unsupported; original preserved".into(),
                    ));
                }
                data
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => SessionData::default(),
            Err(e) => return Err(e.into()),
        };
        let (changed, receiver) = tokio::sync::watch::channel(0);
        Ok((
            Arc::new(Self {
                data: Mutex::new(data),
                path,
                _lease: lease,
                write_lock: Mutex::new(()),
                changed,
            }),
            receiver,
        ))
    }
    pub fn edit(&self, edit: impl FnOnce(&mut SessionData)) {
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        edit(&mut data);
        for (_, text) in &mut data.messages {
            if text.len() > 128 * 1024 {
                text.truncate(text.floor_char_boundary(128 * 1024));
            }
        }
        if data.messages.len() > 500 {
            let n = data.messages.len() - 500;
            data.messages.drain(..n);
        }
        while data
            .messages
            .iter()
            .map(|(r, t)| r.len() + t.len())
            .sum::<usize>()
            > 2 * 1024 * 1024
        {
            data.messages.remove(0);
        }
        if data.draft.len() > 128 * 1024 {
            let n = data.draft.floor_char_boundary(128 * 1024);
            data.draft.truncate(n);
        }
        while !data.messages.is_empty()
            && serde_json::to_vec(&*data).is_ok_and(|bytes| bytes.len() > 3 * 1024 * 1024)
        {
            data.messages.remove(0);
        }
        self.changed
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
    pub fn flush(&self) -> Result<()> {
        let _serial = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let bytes = serde_json::to_vec(&*self.data.lock().unwrap_or_else(|e| e.into_inner()))?;
        atomic_write_private(&self.path, &bytes)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restores_unicode_drafts_and_messages_and_locks_other_writers() {
        let root = std::env::temp_dir().join(format!("gm-ui-history-{}", uuid::Uuid::new_v4()));
        let (store, _) = SessionStore::open(&root).unwrap();
        store.edit(|d| {
            d.messages.push(("You".into(), "你好 👋".into()));
            d.draft = "unsent draft".into();
        });
        store.flush().unwrap();
        assert!(SessionStore::open(&root).is_err());
        drop(store);
        let (store, _) = SessionStore::open(&root).unwrap();
        assert_eq!(store.data.lock().unwrap().draft, "unsent draft");
        assert_eq!(store.data.lock().unwrap().messages[0].1, "你好 👋");
        drop(store);
        std::fs::write(root.join("native-ui/session-v1.json"), "broken").unwrap();
        assert!(SessionStore::open(&root).is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("native-ui/session-v1.json")).unwrap(),
            "broken"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
