use chrono::Utc;
use gm_core::database::Database;
use gm_core::error::{GmError, Result};
use parking_lot::Mutex;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub id: String,
    pub name: String,
    pub topic: String,
    #[serde(default)]
    pub instructions: String,
    pub owner_bot_id: Option<String>,
    pub participants: Vec<String>,
    pub paused: bool,
    pub archived: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelMessageRecord {
    pub id: String,
    pub channel_id: String,
    pub author: String,
    pub bot_id: Option<String>,
    pub content: String,
    pub mentioned_bots: Vec<String>,
    pub timestamp: String,
}

/// Extract `@bot_id` mentions from a channel message against known bot IDs.
pub fn extract_bot_mentions(text: &str, valid_bot_ids: &[String]) -> Vec<String> {
    let mut mentioned = Vec::new();
    for token in text.split(|c: char| !c.is_alphanumeric() && c != '@' && c != '_' && c != '-') {
        if let Some(handle) = token.strip_prefix('@') {
            let lower = handle.to_lowercase();
            if valid_bot_ids.iter().any(|b| b == &lower) && !mentioned.contains(&lower) {
                mentioned.push(lower);
            }
        }
    }
    mentioned
}

/// Use one canonical identifier for HTTP, CLI, and database access.
pub fn normalize_channel_id(id: &str) -> String {
    id.trim().trim_start_matches('#').to_lowercase()
}

/// Persistent SQLite store for OpenBot multi-agent group channels (`#general`, `#research`, `#ops`)
/// with single-task owner controls (`Stop`, `Resume`, `Reassign`, `Archive`, `Restore`).
#[derive(Clone)]
pub struct ChannelStore {
    conn: Arc<Mutex<Database>>,
}

impl ChannelStore {
    pub fn open(db_path: impl AsRef<Path>) -> Result<Self> {
        let conn = crate::db_recovery::open_sqlite_with_recovery(db_path.as_ref())?;
        Self::init_conn(conn)
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Database::in_memory()?;
        Self::init_conn(conn)
    }

    fn init_conn(mut conn: Database) -> Result<Self> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
            .map_err(|e| GmError::Platform(e.to_string()))?;
        conn.migrate("openbot_channels", 1, |conn| {
            conn.execute_batch(
                r#"
            CREATE TABLE IF NOT EXISTS channels (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                topic TEXT NOT NULL,
                instructions TEXT NOT NULL DEFAULT '',
                owner_bot_id TEXT,
                participants_json TEXT NOT NULL,
                paused INTEGER NOT NULL DEFAULT 0,
                archived INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS channel_messages (
                id TEXT PRIMARY KEY,
                channel_id TEXT NOT NULL,
                author TEXT NOT NULL,
                bot_id TEXT,
                content TEXT NOT NULL,
                mentioned_bots_json TEXT NOT NULL,
                timestamp TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_channel_msgs_chan
                ON channel_messages(channel_id, timestamp ASC);
            "#,
            )
            .map_err(|e| GmError::Platform(format!("Channel schema init failed: {e}")))?;

            // Idempotent column addition for existing databases
            let has_instructions: bool = conn
                .prepare("PRAGMA table_info(channels)")
                .and_then(|mut stmt| {
                    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
                    for col in columns {
                        if col? == "instructions" {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                })
                .map_err(|e| GmError::Platform(e.to_string()))?;

            if !has_instructions {
                conn.execute_batch(
                    "ALTER TABLE channels ADD COLUMN instructions TEXT NOT NULL DEFAULT '';",
                )
                .map_err(|e| GmError::Platform(e.to_string()))?;
            }

            Ok(())
        })?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.seed_default_channels()?;
        Ok(store)
    }

    fn seed_default_channels(&self) -> Result<()> {
        let defaults = [
            (
                "general",
                "#general",
                "Main workspace channel for coordinating tasks across @muse, @researcher, @coder, and @guardian",
                Some("muse"),
                vec!["muse", "researcher", "coder", "guardian"],
            ),
            (
                "research",
                "#research",
                "Deep research, synthesis, and briefing channel led by @researcher",
                Some("researcher"),
                vec!["researcher", "muse"],
            ),
            (
                "ops",
                "#ops",
                "Engineering, workspace shell/diffs, and governance audit channel led by @coder and @guardian",
                Some("coder"),
                vec!["coder", "guardian", "muse"],
            ),
        ];

        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        for (id, name, topic, owner, parts) in defaults {
            let parts_json = serde_json::to_string(&parts)?;
            conn.execute(
                r#"INSERT OR IGNORE INTO channels (
                    id, name, topic, instructions, owner_bot_id, participants_json, paused, archived, created_at
                ) VALUES (?1, ?2, ?3, '', ?4, ?5, 0, 0, ?6)"#,
                params![id, name, topic, owner, parts_json, now],
            )
            .map_err(|e| GmError::Platform(e.to_string()))?;
        }
        Ok(())
    }

    pub fn create_channel_with_instructions(
        &self,
        id: &str,
        topic: &str,
        instructions: &str,
        owner_bot_id: Option<&str>,
        participants: &[String],
    ) -> Result<ChannelInfo> {
        let clean_id = normalize_channel_id(id);
        if clean_id.is_empty()
            || clean_id.len() > 64
            || !clean_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(GmError::Config(
                "Channel ID must be 1–64 letters, digits, underscores or hyphens".into(),
            ));
        }
        let name = format!("#{clean_id}");
        let now = Utc::now().to_rfc3339();
        let parts_json = serde_json::to_string(participants)?;
        self.conn
            .lock()
            .execute(
                r#"INSERT INTO channels (
                    id, name, topic, instructions, owner_bot_id, participants_json, paused, archived, created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0, ?7)"#,
                params![clean_id, name, topic, instructions, owner_bot_id, parts_json, now],
            )
            .map_err(|e| GmError::Platform(e.to_string()))?;

        self.get_channel(&clean_id)?
            .ok_or_else(|| GmError::Platform("Channel not found after insert".into()))
    }

    pub fn create_channel(
        &self,
        id: &str,
        topic: &str,
        owner_bot_id: Option<&str>,
        participants: &[String],
    ) -> Result<ChannelInfo> {
        self.create_channel_with_instructions(id, topic, "", owner_bot_id, participants)
    }

    pub fn update_channel_instructions(
        &self,
        channel_id: &str,
        instructions: &str,
    ) -> Result<bool> {
        let clean = normalize_channel_id(channel_id);
        let rows = self
            .conn
            .lock()
            .execute(
                "UPDATE channels SET instructions = ?1 WHERE id = ?2",
                params![instructions, clean],
            )
            .map_err(|e| GmError::Platform(e.to_string()))?;
        Ok(rows > 0)
    }

    pub fn get_channel(&self, channel_id: &str) -> Result<Option<ChannelInfo>> {
        let clean = normalize_channel_id(channel_id);
        self.conn
            .lock()
            .query_row(
                r#"SELECT id, name, topic, instructions, owner_bot_id, participants_json, paused, archived, created_at
                   FROM channels WHERE id = ?1"#,
                params![clean],
                Self::map_channel_row,
            )
            .optional()
            .map_err(|e| GmError::Platform(e.to_string()))
    }

    pub fn list_channels(&self, include_archived: bool) -> Result<Vec<ChannelInfo>> {
        let conn = self.conn.lock();
        let sql = if include_archived {
            "SELECT id, name, topic, instructions, owner_bot_id, participants_json, paused, archived, created_at FROM channels ORDER BY created_at ASC"
        } else {
            "SELECT id, name, topic, instructions, owner_bot_id, participants_json, paused, archived, created_at FROM channels WHERE archived = 0 ORDER BY created_at ASC"
        };
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| GmError::Platform(e.to_string()))?;
        let rows = stmt
            .query_map([], Self::map_channel_row)
            .map_err(|e| GmError::Platform(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| GmError::Platform(e.to_string()))?);
        }
        Ok(out)
    }

    pub fn post_message(
        &self,
        channel_id: &str,
        author: &str,
        bot_id: Option<&str>,
        content: &str,
        valid_bot_ids: &[String],
    ) -> Result<ChannelMessageRecord> {
        let clean_chan = normalize_channel_id(channel_id);
        let channel = self
            .get_channel(&clean_chan)?
            .ok_or_else(|| GmError::Config(format!("Unknown channel '{clean_chan}'")))?;
        if channel.archived {
            return Err(GmError::Config(format!(
                "Channel '{clean_chan}' is archived"
            )));
        }
        if content.trim().is_empty() {
            return Err(GmError::Config("Message cannot be empty".into()));
        }
        let mentioned = extract_bot_mentions(content, valid_bot_ids);
        let mentioned_json = serde_json::to_string(&mentioned)?;
        let id = format!("msg-{}", uuid::Uuid::new_v4());
        let now = Utc::now().to_rfc3339();

        self.conn
            .lock()
            .execute(
                r#"INSERT INTO channel_messages (
                    id, channel_id, author, bot_id, content, mentioned_bots_json, timestamp
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
                params![id, clean_chan, author, bot_id, content, mentioned_json, now],
            )
            .map_err(|e| GmError::Platform(e.to_string()))?;

        Ok(ChannelMessageRecord {
            id,
            channel_id: clean_chan.to_string(),
            author: author.to_string(),
            bot_id: bot_id.map(|s| s.to_string()),
            content: content.to_string(),
            mentioned_bots: mentioned,
            timestamp: now,
        })
    }

    pub fn list_messages(
        &self,
        channel_id: &str,
        limit: usize,
    ) -> Result<Vec<ChannelMessageRecord>> {
        let clean_chan = normalize_channel_id(channel_id);
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                r#"SELECT id, channel_id, author, bot_id, content, mentioned_bots_json, timestamp
                   FROM channel_messages
                   WHERE channel_id = ?1
                   ORDER BY timestamp DESC, rowid DESC
                   LIMIT ?2"#,
            )
            .map_err(|e| GmError::Platform(e.to_string()))?;
        let rows = stmt
            .query_map(params![clean_chan, limit as i64], |row| {
                let m_json: String = row.get(5)?;
                let mentioned: Vec<String> = serde_json::from_str(&m_json).unwrap_or_default();
                Ok(ChannelMessageRecord {
                    id: row.get(0)?,
                    channel_id: row.get(1)?,
                    author: row.get(2)?,
                    bot_id: row.get(3)?,
                    content: row.get(4)?,
                    mentioned_bots: mentioned,
                    timestamp: row.get(6)?,
                })
            })
            .map_err(|e| GmError::Platform(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| GmError::Platform(e.to_string()))?);
        }
        out.reverse();
        Ok(out)
    }

    pub fn set_channel_paused(&self, channel_id: &str, paused: bool) -> Result<bool> {
        let clean = normalize_channel_id(channel_id);
        let rows = self
            .conn
            .lock()
            .execute(
                "UPDATE channels SET paused = ?1 WHERE id = ?2",
                params![paused as i32, clean],
            )
            .map_err(|e| GmError::Platform(e.to_string()))?;
        Ok(rows > 0)
    }

    pub fn reassign_owner(&self, channel_id: &str, new_owner_bot_id: &str) -> Result<bool> {
        let clean = normalize_channel_id(channel_id);
        let rows = self
            .conn
            .lock()
            .execute(
                "UPDATE channels SET owner_bot_id = ?1 WHERE id = ?2",
                params![new_owner_bot_id, clean],
            )
            .map_err(|e| GmError::Platform(e.to_string()))?;
        Ok(rows > 0)
    }

    pub fn set_archived(&self, channel_id: &str, archived: bool) -> Result<bool> {
        let clean = normalize_channel_id(channel_id);
        let rows = self
            .conn
            .lock()
            .execute(
                "UPDATE channels SET archived = ?1 WHERE id = ?2",
                params![archived as i32, clean],
            )
            .map_err(|e| GmError::Platform(e.to_string()))?;
        Ok(rows > 0)
    }

    fn map_channel_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChannelInfo> {
        let instructions: String = row.get(3).unwrap_or_default();
        let parts_json: String = row.get(5)?;
        let participants: Vec<String> = serde_json::from_str(&parts_json).unwrap_or_default();
        let paused_int: i32 = row.get(6)?;
        let archived_int: i32 = row.get(7)?;
        Ok(ChannelInfo {
            id: row.get(0)?,
            name: row.get(1)?,
            topic: row.get(2)?,
            instructions,
            owner_bot_id: row.get(4)?,
            participants,
            paused: paused_int != 0,
            archived: archived_int != 0,
            created_at: row.get(8)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_instructions_persistence_and_update() {
        let store = ChannelStore::in_memory().unwrap();
        let chan = store
            .create_channel_with_instructions(
                "devops",
                "DevOps channel",
                "Always run cargo test before submitting diffs.",
                Some("coder"),
                &["coder".into()],
            )
            .unwrap();

        assert_eq!(
            chan.instructions,
            "Always run cargo test before submitting diffs."
        );
        let fetched = store.get_channel("devops").unwrap().unwrap();
        assert_eq!(
            fetched.instructions,
            "Always run cargo test before submitting diffs."
        );

        assert!(
            store
                .update_channel_instructions(
                    "devops",
                    "Updated instructions: ensure clippy is clean."
                )
                .unwrap()
        );
        let updated = store.get_channel("devops").unwrap().unwrap();
        assert_eq!(
            updated.instructions,
            "Updated instructions: ensure clippy is clean."
        );
    }
}
