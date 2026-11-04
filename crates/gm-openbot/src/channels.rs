use chrono::Utc;
use gm_core::error::{GmError, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub id: String,
    pub name: String,
    pub topic: String,
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
    for token in text.split_whitespace() {
        let trimmed =
            token.trim_matches(|c: char| !c.is_alphanumeric() && c != '@' && c != '_' && c != '-');
        if let Some(handle) = trimmed.strip_prefix('@') {
            let lower = handle.to_lowercase();
            if valid_bot_ids.iter().any(|b| b == &lower) && !mentioned.contains(&lower) {
                mentioned.push(lower);
            }
        }
    }
    mentioned
}

/// Persistent SQLite store for OpenBot multi-agent group channels (`#general`, `#research`, `#ops`)
/// with single-task owner controls (`Stop`, `Resume`, `Reassign`, `Archive`, `Restore`).
#[derive(Clone)]
pub struct ChannelStore {
    conn: Arc<Mutex<Connection>>,
}

impl ChannelStore {
    pub fn open(db_path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = db_path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(db_path.as_ref())
            .map_err(|e| GmError::Platform(format!("Failed to open channel SQLite DB: {e}")))?;
        Self::init_conn(conn)
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| GmError::Platform(format!("Failed to open in-memory channel DB: {e}")))?;
        Self::init_conn(conn)
    }

    fn init_conn(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            CREATE TABLE IF NOT EXISTS channels (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                topic TEXT NOT NULL,
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
                    id, name, topic, owner_bot_id, participants_json, paused, archived, created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, ?6)"#,
                params![id, name, topic, owner, parts_json, now],
            )
            .map_err(|e| GmError::Platform(e.to_string()))?;
        }
        Ok(())
    }

    pub fn create_channel(
        &self,
        id: &str,
        topic: &str,
        owner_bot_id: Option<&str>,
        participants: &[String],
    ) -> Result<ChannelInfo> {
        let clean_id = id.trim().trim_start_matches('#').to_lowercase();
        let name = format!("#{clean_id}");
        let now = Utc::now().to_rfc3339();
        let parts_json = serde_json::to_string(participants)?;
        self.conn
            .lock()
            .execute(
                r#"INSERT OR REPLACE INTO channels (
                    id, name, topic, owner_bot_id, participants_json, paused, archived, created_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, ?6)"#,
                params![clean_id, name, topic, owner_bot_id, parts_json, now],
            )
            .map_err(|e| GmError::Platform(e.to_string()))?;

        self.get_channel(&clean_id)?
            .ok_or_else(|| GmError::Platform("Channel not found after insert".into()))
    }

    pub fn get_channel(&self, channel_id: &str) -> Result<Option<ChannelInfo>> {
        let clean = channel_id.trim().trim_start_matches('#');
        self.conn
            .lock()
            .query_row(
                r#"SELECT id, name, topic, owner_bot_id, participants_json, paused, archived, created_at
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
            "SELECT id, name, topic, owner_bot_id, participants_json, paused, archived, created_at FROM channels ORDER BY created_at ASC"
        } else {
            "SELECT id, name, topic, owner_bot_id, participants_json, paused, archived, created_at FROM channels WHERE archived = 0 ORDER BY created_at ASC"
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
        let clean_chan = channel_id.trim().trim_start_matches('#');
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
        let clean_chan = channel_id.trim().trim_start_matches('#');
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
        let clean = channel_id.trim().trim_start_matches('#');
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
        let clean = channel_id.trim().trim_start_matches('#');
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
        let clean = channel_id.trim().trim_start_matches('#');
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
        let parts_json: String = row.get(4)?;
        let participants: Vec<String> = serde_json::from_str(&parts_json).unwrap_or_default();
        let paused_int: i32 = row.get(5)?;
        let archived_int: i32 = row.get(6)?;
        Ok(ChannelInfo {
            id: row.get(0)?,
            name: row.get(1)?,
            topic: row.get(2)?,
            owner_bot_id: row.get(3)?,
            participants,
            paused: paused_int != 0,
            archived: archived_int != 0,
            created_at: row.get(7)?,
        })
    }
}
