use chrono::Utc;
use gm_agent::BotWorkspaceSandbox;
use gm_core::error::{GmError, Result};
use gm_memory::PersonalMemoryStore;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotProfile {
    pub id: String,
    pub display_name: String,
    pub avatar_emoji: String,
    pub role_title: String,
    pub system_prompt: String,
    pub preferred_provider: Option<String>,
    pub preferred_model: Option<String>,
    pub allow_gui: bool,
    pub allow_shell: bool,
    pub allow_network: bool,
    pub created_at: String,
}

/// Persistent roster of OpenBot AI teammates with isolated workspace folders and per-bot memory.
#[derive(Clone)]
pub struct BotRoster {
    bots_dir: PathBuf,
    roster_file: PathBuf,
    bots: Arc<Mutex<BTreeMap<String, BotProfile>>>,
}

impl BotRoster {
    pub fn open(bots_dir: impl AsRef<Path>) -> Result<Self> {
        let dir = bots_dir.as_ref().to_path_buf();
        gm_core::storage::ensure_private_dir(&dir)?;
        let _lease = Self::lock_directory(&dir)?;
        let roster_file = dir.join("roster.json");

        let mut loaded: BTreeMap<String, BotProfile> = if roster_file.exists() {
            let content = std::fs::read_to_string(&roster_file)?;
            // A damaged roster must never be replaced by defaults on the next read.
            serde_json::from_str(&content)?
        } else {
            BTreeMap::new()
        };

        for (key, profile) in &loaded {
            if key != &profile.id
                || key.is_empty()
                || key.len() > 64
                || !key
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
            {
                return Err(GmError::Config(
                    "Roster contains an invalid bot identifier; original file preserved".into(),
                ));
            }
        }

        if loaded.is_empty() {
            for profile in Self::default_coworkers() {
                loaded.insert(profile.id.clone(), profile);
            }
        }

        let roster = Self {
            bots_dir: dir,
            roster_file,
            bots: Arc::new(Mutex::new(loaded)),
        };
        roster.persist()?;
        roster.provision_all_workspaces()?;
        Ok(roster)
    }

    pub fn default_coworkers() -> Vec<BotProfile> {
        let now = Utc::now().to_rfc3339();
        vec![
            BotProfile {
                id: "muse".into(),
                display_name: "Muse Operator".into(),
                avatar_emoji: "⚡".into(),
                role_title: "General Desktop Operator & Coordinator".into(),
                system_prompt: "You are Muse, the lead desktop operator. Coordinate tasks, navigate macOS apps via Accessibility and Set-of-Marks vision, and delegate coding or deep research to @coder or @researcher when appropriate.".into(),
                preferred_provider: None,
                preferred_model: None,
                allow_gui: true,
                allow_shell: true,
                allow_network: true,
                created_at: now.clone(),
            },
            BotProfile {
                id: "researcher".into(),
                display_name: "Nova Researcher".into(),
                avatar_emoji: "🔭".into(),
                role_title: "Deep Knowledge & Web Researcher".into(),
                system_prompt: "You are Nova, a meticulous research specialist. Synthesize documents, search memory and MCP sources, verify citations, and write structured briefs into your workspace/outbox.".into(),
                preferred_provider: None,
                preferred_model: None,
                allow_gui: false,
                allow_shell: true,
                allow_network: true,
                created_at: now.clone(),
            },
            BotProfile {
                id: "coder".into(),
                display_name: "Atlas Engineer".into(),
                avatar_emoji: "🛠️".into(),
                role_title: "Sandboxed Workspace Software Engineer".into(),
                system_prompt: "You are Atlas, a software engineer operating in an isolated project workspace. Write clean code, generate unified diff previews, run tests via WorkspaceShell, and place deliverables in outbox/.".into(),
                preferred_provider: None,
                preferred_model: None,
                allow_gui: false,
                allow_shell: true,
                allow_network: true,
                created_at: now.clone(),
            },
            BotProfile {
                id: "guardian".into(),
                display_name: "Aegis Auditor".into(),
                avatar_emoji: "🛡️".into(),
                role_title: "Risk, Security & Governance Analyst".into(),
                system_prompt: "You are Aegis, a security and compliance analyst. Inspect audit logs, review workspace diffs, flag irreversible or risky operations, and never mutate external systems.".into(),
                preferred_provider: None,
                preferred_model: None,
                allow_gui: false,
                allow_shell: false,
                allow_network: false,
                created_at: now,
            },
        ]
    }

    fn lock_directory(directory: &Path) -> Result<std::fs::File> {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(directory.join(".roster.lock"))?;
        file.lock()?;
        Ok(file)
    }
    pub fn refresh(&self) -> Result<()> {
        let raw = std::fs::read_to_string(&self.roster_file)?;
        let map: BTreeMap<String, BotProfile> = serde_json::from_str(&raw)?;
        for (key, profile) in &map {
            if key != &profile.id
                || key.is_empty()
                || key.len() > 64
                || !key
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
            {
                return Err(GmError::Config("Invalid bot identifier in roster".into()));
            }
        }
        *self.bots.lock() = map;
        Ok(())
    }

    fn persist(&self) -> Result<()> {
        let map = self.bots.lock();
        let json = serde_json::to_string_pretty(&*map)?;
        gm_core::storage::atomic_write_private(&self.roster_file, json.as_bytes())?;
        Ok(())
    }

    fn provision_all_workspaces(&self) -> Result<()> {
        let ids: Vec<(String, bool)> = self
            .bots
            .lock()
            .values()
            .map(|b| (b.id.clone(), b.allow_network))
            .collect();
        for (id, allow_net) in ids {
            let bot_root = self.bot_root(&id);
            let _ = BotWorkspaceSandbox::new(&id, &bot_root)?.with_network(allow_net);
        }
        Ok(())
    }

    pub fn bots_dir(&self) -> &Path {
        &self.bots_dir
    }

    pub fn bot_root(&self, bot_id: &str) -> PathBuf {
        self.bots_dir.join(bot_id)
    }

    pub fn workspace_sandbox(&self, bot_id: &str) -> Result<BotWorkspaceSandbox> {
        let profile = self
            .get_bot(bot_id)
            .ok_or_else(|| GmError::Config(format!("Unknown bot '{bot_id}'")))?;
        let sandbox = BotWorkspaceSandbox::new(&profile.id, self.bot_root(&profile.id))?
            .with_network(profile.allow_network);
        Ok(sandbox)
    }

    pub fn memory_store(&self, bot_id: &str) -> Result<PersonalMemoryStore> {
        let root = self.bot_root(bot_id);
        std::fs::create_dir_all(&root)?;
        PersonalMemoryStore::open(root.join("memory.sqlite"))
    }

    pub fn list_bots(&self) -> Vec<BotProfile> {
        if let Err(error) = self.refresh() {
            tracing::error!(%error,"Roster could not be refreshed; admission disabled");
            return Vec::new();
        }
        self.bots.lock().values().cloned().collect()
    }

    pub fn get_bot(&self, bot_id: &str) -> Option<BotProfile> {
        self.refresh().ok()?;
        self.bots.lock().get(bot_id).cloned()
    }

    pub fn upsert_bot(&self, profile: BotProfile) -> Result<BotProfile> {
        let _lease = Self::lock_directory(&self.bots_dir)?;
        self.refresh()?;
        let id = profile.id.trim().to_lowercase();
        if id.is_empty()
            || id.len() > 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        {
            return Err(GmError::Config(format!("Invalid bot ID '{}'", profile.id)));
        }
        let mut normalized = profile;
        normalized.id = id.clone();
        self.bots.lock().insert(id.clone(), normalized.clone());
        self.persist()?;
        let _ = BotWorkspaceSandbox::new(&id, self.bot_root(&id))?
            .with_network(normalized.allow_network);
        Ok(normalized)
    }

    pub fn remove_bot(&self, bot_id: &str) -> Result<bool> {
        let _lease = Self::lock_directory(&self.bots_dir)?;
        self.refresh()?;
        if bot_id == "muse" {
            return Err(GmError::Config(
                "Cannot remove the default 'muse' operator bot".into(),
            ));
        }
        let removed = self.bots.lock().remove(bot_id).is_some();
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }
}
