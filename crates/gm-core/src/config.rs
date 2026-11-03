use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GmConfig {
    pub socket_path: PathBuf,
    pub storage_dir: PathBuf,
    pub providers: ProvidersConfig,
    pub safety: SafetyConfig,
    pub budget: BudgetConfig,
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
            let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
            let socket = PathBuf::from(runtime_dir).join("grok-muse-cu-helper.sock");
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

        Self {
            socket_path,
            storage_dir,
            providers: ProvidersConfig::default(),
            safety: SafetyConfig::default(),
            budget: BudgetConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
            allow_screenshot_egress_meta: true,
            allow_screenshot_egress_xai: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyConfig {
    pub blocklisted_apps: Vec<String>,
    pub redact_secure_fields: bool,
    pub kill_switch_shortcut: String,
    pub user_active_pause_seconds: f64,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            blocklisted_apps: vec![
                "1Password".into(),
                "Keychain Access".into(),
                "Bitwarden".into(),
                "KeePassXC".into(),
                "com.apple.keychainaccess".into(),
            ],
            redact_secure_fields: true,
            kill_switch_shortcut: "Cmd+Alt+Escape".into(),
            user_active_pause_seconds: 1.5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
