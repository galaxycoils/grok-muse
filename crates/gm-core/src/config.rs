use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn default_daemon_socket_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        PathBuf::from(r"\\.\pipe\grok-muse-daemon")
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home)
            .join("Library/Application Support/GrokMuse")
            .join("daemon.sock")
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let base = std::env::var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let data = std::env::var("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| {
                        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                            .join(".local/share")
                    });
                data.join("grok-muse")
            });
        base.join("grok-muse-daemon.sock")
    }
}

pub fn default_config_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join("Library/Application Support/GrokMuse/config.json")
    }
    #[cfg(not(target_os = "macos"))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".config/grok-muse/config.json")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GmConfig {
    pub socket_path: PathBuf,
    #[serde(default = "default_daemon_socket_path")]
    pub daemon_socket_path: PathBuf,
    pub storage_dir: PathBuf,
    pub providers: ProvidersConfig,
    pub safety: SafetyConfig,
    pub budget: BudgetConfig,
    pub openbot: OpenBotConfig,
    pub workspace_vm: WorkspaceVmConfig,
}

impl GmConfig {
    /// Missing default configuration uses defaults. Explicit paths and malformed files fail closed.
    pub fn load(custom_path: Option<&Path>) -> crate::error::Result<Self> {
        let path = custom_path
            .map(Path::to_path_buf)
            .unwrap_or_else(default_config_path);
        let mut config: Self = match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).map_err(|e| {
                crate::error::GmError::Config(format!(
                    "Invalid configuration {}: {e}",
                    path.display()
                ))
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && custom_path.is_none() => {
                Self::default()
            }
            Err(e) => {
                return Err(crate::error::GmError::Config(format!(
                    "Cannot read configuration {}: {e}",
                    path.display()
                )));
            }
        };

        if let Ok(socket) = std::env::var("GM_SOCKET_PATH")
            && !socket.trim().is_empty()
        {
            config.socket_path = PathBuf::from(socket);
        }
        if let Ok(daemon_socket) = std::env::var("GM_DAEMON_SOCKET_PATH")
            && !daemon_socket.trim().is_empty()
        {
            config.daemon_socket_path = PathBuf::from(daemon_socket);
        }

        if !config.budget.daily_spend_cap_usd.is_finite()
            || config.budget.daily_spend_cap_usd < 0.0
            || !config.budget.per_job_spend_cap_usd.is_finite()
            || config.budget.per_job_spend_cap_usd < 0.0
        {
            return Err(crate::error::GmError::Config(
                "Budget limits must be finite and nonnegative".into(),
            ));
        }
        Ok(config)
    }

    pub fn load_or_default(custom_path: Option<&Path>) -> Self {
        Self::load(custom_path)
            .expect("Configuration could not be loaded; the original file has been preserved")
    }
}

impl Default for GmConfig {
    fn default() -> Self {
        #[cfg(target_os = "windows")]
        let (socket_path, storage_dir) = {
            let app_data = std::env::var("APPDATA")
                .or_else(|_| std::env::var("LOCALAPPDATA"))
                .or_else(|_| std::env::var("USERPROFILE"))
                .unwrap_or_else(|_| ".".into());
            let storage = PathBuf::from(app_data).join("GrokMuse");
            let socket = PathBuf::from(r"\\.\pipe\grok-muse-cu-helper");
            (socket, storage)
        };

        #[cfg(target_os = "linux")]
        let (socket_path, storage_dir) = {
            let data_home = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                format!("{home}/.local/share")
            });
            let storage = PathBuf::from(data_home).join("grok-muse");
            let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| storage.clone());
            let socket = runtime_dir.join("grok-muse-cu-helper.sock");
            (socket, storage)
        };

        #[cfg(target_os = "macos")]
        let (socket_path, storage_dir) = {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            let app_support = PathBuf::from(home).join("Library/Application Support/GrokMuse");
            let socket = app_support.join("cu-helper.sock");
            (socket, app_support)
        };

        #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
        let (socket_path, storage_dir) = {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            let base = PathBuf::from(home).join(".grok-muse");
            (base.join("cu-helper.sock"), base)
        };

        let openbot = OpenBotConfig {
            bots_dir: storage_dir.join("bots"),
            ..OpenBotConfig::default()
        };

        Self {
            socket_path,
            daemon_socket_path: default_daemon_socket_path(),
            storage_dir,
            providers: ProvidersConfig::default(),
            safety: SafetyConfig::default(),
            budget: BudgetConfig::default(),
            openbot,
            workspace_vm: WorkspaceVmConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProvidersConfig {
    pub meta_api_base: String,
    pub meta_cu_model: String,
    pub meta_reasoning_model: String,
    pub xai_api_base: String,
    pub xai_model: String,
    pub allow_screenshot_egress_meta: bool,
    pub allow_screenshot_egress_xai: bool,
}

impl Default for ProvidersConfig {
    fn default() -> Self {
        Self {
            meta_api_base: "https://api.meta.ai/v1".into(),
            meta_cu_model: "muse-spark".into(),
            meta_reasoning_model: "muse-wonder".into(),
            xai_api_base: "https://api.x.ai/v1".into(),
            xai_model: "grok-4".into(),
            allow_screenshot_egress_meta: false,
            allow_screenshot_egress_xai: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyConfig {
    /// Explicit opt-in for agents to observe or control the host desktop.
    pub allow_host_desktop: bool,
    /// Experimental credential injection requires separate opt-in and per-action approval.
    pub allow_secret_injection: bool,
    pub blocklisted_apps: Vec<String>,
    pub redact_secure_fields: bool,
    pub kill_switch_shortcut: String,
    pub user_active_pause_seconds: f64,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            allow_host_desktop: false,
            allow_secret_injection: false,
            blocklisted_apps: vec![
                "1Password".into(),
                "Keychain Access".into(),
                "Bitwarden".into(),
                "KeePassXC".into(),
                "Dashlane".into(),
                "LastPass".into(),
                "Authenticator".into(),
                "Passwords".into(),
                "com.apple.keychainaccess".into(),
                "com.apple.Passwords".into(),
            ],
            redact_secure_fields: true,
            kill_switch_shortcut: "Cmd+Alt+Escape".into(),
            user_active_pause_seconds: 1.5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetConfig {
    pub daily_spend_cap_usd: f64,
    pub per_job_spend_cap_usd: f64,
    pub max_daily_notifications: u32,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            daily_spend_cap_usd: 5.0,
            per_job_spend_cap_usd: 0.50,
            max_daily_notifications: 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenBotConfig {
    pub enabled: bool,
    pub bots_dir: PathBuf,
    pub default_bot_id: String,
    pub companion_enabled: bool,
    pub companion_bind_addr: String,
    pub max_context_tokens: usize,
    pub compaction_trigger_ratio: f32,
    pub max_handoff_depth: u8,
    pub bridges: ChatBridgesConfig,
}

impl Default for OpenBotConfig {
    fn default() -> Self {
        #[cfg(target_os = "macos")]
        let bots_dir = {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join("Library/Application Support/GrokMuse/bots")
        };
        #[cfg(not(target_os = "macos"))]
        let bots_dir = {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".grok-muse/bots")
        };

        Self {
            enabled: true,
            bots_dir,
            default_bot_id: "muse".into(),
            companion_enabled: true,
            companion_bind_addr: "127.0.0.1:7433".into(),
            max_context_tokens: 24_000,
            compaction_trigger_ratio: 0.80,
            max_handoff_depth: 4,
            bridges: ChatBridgesConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatBridgesConfig {
    pub telegram_enabled: bool,
    pub telegram_allowed_chat_ids: Vec<String>,
    pub slack_enabled: bool,
    pub slack_allowed_channel_ids: Vec<String>,
    pub discord_enabled: bool,
    pub discord_allowed_channel_ids: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = GmConfig::default();
        assert!(!config.socket_path.as_os_str().is_empty());
        assert!(!config.daemon_socket_path.as_os_str().is_empty());
        assert!(
            config
                .safety
                .blocklisted_apps
                .contains(&"Passwords".to_string())
        );
        assert!(
            config
                .safety
                .blocklisted_apps
                .contains(&"com.apple.Passwords".to_string())
        );
        assert_eq!(config.safety.blocklisted_apps.len(), 10);
    }

    #[test]
    fn test_load_or_default_custom_file() {
        let path = std::env::temp_dir().join(format!("test_config_{}.json", uuid::Uuid::new_v4()));
        let json = r#"{
            "socket_path": "/tmp/custom-helper.sock",
            "daemon_socket_path": "/tmp/custom-daemon.sock",
            "storage_dir": "/tmp/custom-storage",
            "providers": {
                "meta_api_base": "https://api.meta.ai/v1",
                "meta_cu_model": "muse-spark",
                "meta_reasoning_model": "muse-wonder",
                "xai_api_base": "https://api.x.ai/v1",
                "xai_model": "grok-4",
                "allow_screenshot_egress_meta": false,
                "allow_screenshot_egress_xai": false
            },
            "safety": {
                "blocklisted_apps": ["CustomApp"],
                "redact_secure_fields": false,
                "kill_switch_shortcut": "Escape",
                "user_active_pause_seconds": 2.0
            },
            "budget": {
                "daily_spend_cap_usd": 10.0,
                "per_job_spend_cap_usd": 1.0,
                "max_daily_notifications": 5
            }
        }"#;
        std::fs::write(&path, json).unwrap();

        let loaded = GmConfig::load_or_default(Some(&path));
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded.socket_path, PathBuf::from("/tmp/custom-helper.sock"));
        assert_eq!(
            loaded.daemon_socket_path,
            PathBuf::from("/tmp/custom-daemon.sock")
        );
        assert_eq!(loaded.safety.blocklisted_apps, vec!["CustomApp"]);
    }

    #[test]
    fn test_load_or_default_partial_file() {
        let path =
            std::env::temp_dir().join(format!("test_partial_config_{}.json", uuid::Uuid::new_v4()));
        let json = r#"{
            "daemon_socket_path": "/tmp/custom-partial-daemon.sock",
            "budget": {
                "daily_spend_cap_usd": 25.0
            }
        }"#;
        std::fs::write(&path, json).unwrap();

        let loaded = GmConfig::load_or_default(Some(&path));
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            loaded.daemon_socket_path,
            PathBuf::from("/tmp/custom-partial-daemon.sock")
        );
        assert_eq!(loaded.budget.daily_spend_cap_usd, 25.0);
        // Defaults preserved
        assert_eq!(loaded.budget.per_job_spend_cap_usd, 0.50);
        assert_eq!(loaded.safety.blocklisted_apps.len(), 10);
    }

    #[test]
    fn test_env_var_overrides() {
        unsafe {
            std::env::set_var("GM_SOCKET_PATH", "/tmp/env-helper.sock");
            std::env::set_var("GM_DAEMON_SOCKET_PATH", "/tmp/env-daemon.sock");
        }

        let config = GmConfig::load_or_default(None);
        assert_eq!(config.socket_path, PathBuf::from("/tmp/env-helper.sock"));
        assert_eq!(
            config.daemon_socket_path,
            PathBuf::from("/tmp/env-daemon.sock")
        );

        unsafe {
            std::env::remove_var("GM_SOCKET_PATH");
            std::env::remove_var("GM_DAEMON_SOCKET_PATH");
        }
    }

    #[test]
    fn test_openbot_config_defaults_and_overrides() {
        let cfg = GmConfig::default();
        assert!(cfg.openbot.enabled);
        assert_eq!(cfg.openbot.default_bot_id, "muse");
        assert_eq!(cfg.openbot.companion_bind_addr, "127.0.0.1:7433");
        assert_eq!(cfg.openbot.max_context_tokens, 24_000);
        assert!((cfg.openbot.compaction_trigger_ratio - 0.80).abs() < 1e-5);
        assert_eq!(cfg.openbot.max_handoff_depth, 4);
        assert!(!cfg.openbot.bridges.telegram_enabled);
        assert!(!cfg.openbot.bridges.slack_enabled);
        assert!(!cfg.openbot.bridges.discord_enabled);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceVmConfig {
    pub enabled: bool,
    pub qemu_path: PathBuf,
    pub image_path: PathBuf,
    pub manifest_path: PathBuf,
    pub signature_path: PathBuf,
    pub trusted_public_key_hex: String,
}
impl Default for WorkspaceVmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            qemu_path: if cfg!(target_arch = "aarch64") {
                "qemu-system-aarch64"
            } else {
                "qemu-system-x86_64"
            }
            .into(),
            image_path: PathBuf::new(),
            manifest_path: PathBuf::new(),
            signature_path: PathBuf::new(),
            trusted_public_key_hex: String::new(),
        }
    }
}
