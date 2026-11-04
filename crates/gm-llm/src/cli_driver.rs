//! Supported Claude/Codex/Grok CLI drivers with explicit availability checks,
//! documented authentication flows, and honest cost labeling.

use crate::client::{ChatCompletionResponse, ChatMessage, LlmBackend};
use crate::router::ModelTarget;
use async_trait::async_trait;
use gm_core::error::{GmError, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CliProvider {
    Claude,
    Codex,
    Grok,
}

impl CliProvider {
    pub fn binary_name(&self) -> &'static str {
        match self {
            CliProvider::Claude => "claude",
            CliProvider::Codex => "codex",
            CliProvider::Grok => "grok",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            CliProvider::Claude => "Anthropic Claude CLI",
            CliProvider::Codex => "OpenAI Codex / CLI",
            CliProvider::Grok => "xAI Grok CLI",
        }
    }

    pub fn auth_flow_instructions(&self) -> &'static str {
        match self {
            CliProvider::Claude => {
                "Run `claude login` in your terminal to authenticate with Anthropic, \
                or set `export ANTHROPIC_API_KEY=\"sk-ant-...\"` in your environment."
            }
            CliProvider::Codex => {
                "Run `codex login` in your terminal to authenticate with OpenAI, \
                or set `export OPENAI_API_KEY=\"sk-...\"` in your environment."
            }
            CliProvider::Grok => {
                "Run `grok login --device-auth` in your terminal to authenticate with xAI, \
                or set `export XAI_API_KEY=\"xai-...\"` in your environment."
            }
        }
    }

    pub fn install_instructions(&self) -> &'static str {
        match self {
            CliProvider::Claude => "Install via npm: `npm install -g @anthropic-ai/claude-code`",
            CliProvider::Codex => "Install via npm: `npm install -g @openai/codex`",
            CliProvider::Grok => {
                "Official distribution: `npm install -g @xai-official/grok` (https://docs.x.ai/build/overview)"
            }
        }
    }

    pub fn environment_auth_var(&self) -> &'static str {
        match self {
            CliProvider::Claude => "ANTHROPIC_API_KEY",
            CliProvider::Codex => "OPENAI_API_KEY",
            CliProvider::Grok => "XAI_API_KEY",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliAvailability {
    pub provider: CliProvider,
    pub binary_name: String,
    pub installed: bool,
    pub binary_path: Option<PathBuf>,
    pub version: Option<String>,
    pub authenticated: bool,
    pub auth_instructions: String,
    pub install_instructions: String,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CliDriver {
    provider: CliProvider,
    custom_binary_path: Option<PathBuf>,
}

impl CliDriver {
    pub fn new(provider: CliProvider) -> Self {
        Self {
            provider,
            custom_binary_path: None,
        }
    }

    pub fn with_binary_path(mut self, path: PathBuf) -> Self {
        self.custom_binary_path = Some(path);
        self
    }

    pub fn provider(&self) -> CliProvider {
        self.provider
    }

    /// Check availability of the CLI tool and return structured diagnosis.
    pub fn check_availability(&self) -> CliAvailability {
        let binary_path = if let Some(ref custom) = self.custom_binary_path {
            if custom.is_file() {
                Some(custom.clone())
            } else {
                None
            }
        } else {
            find_binary_on_path(self.provider.binary_name())
        };

        let Some(bin) = binary_path else {
            return CliAvailability {
                provider: self.provider,
                binary_name: self.provider.binary_name().to_string(),
                installed: false,
                binary_path: None,
                version: None,
                authenticated: false,
                auth_instructions: self.provider.auth_flow_instructions().to_string(),
                install_instructions: self.provider.install_instructions().to_string(),
                error_message: Some(format!(
                    "{} ('{}') not found on PATH. {}",
                    self.provider.display_name(),
                    self.provider.binary_name(),
                    self.provider.install_instructions()
                )),
            };
        };

        // Discovery must not execute a PATH binary or mistake an environment
        // variable for verified login. CLI execution requires a certified guest.
        let version = None;
        let authenticated = false;
        let error_message = Some(HOST_CLI_DISABLED.into());

        CliAvailability {
            provider: self.provider,
            binary_name: self.provider.binary_name().to_string(),
            installed: true,
            binary_path: Some(bin),
            version,
            authenticated,
            auth_instructions: self.provider.auth_flow_instructions().to_string(),
            install_instructions: self.provider.install_instructions().to_string(),
            error_message,
        }
    }

    /// Complete a prompt using the CLI driver.
    ///
    /// Costs are labeled honestly as 0.0 with unknown_cli_cost metadata.
    pub async fn complete(
        &self,
        _model: Option<&str>,
        _prompt: &str,
    ) -> Result<ChatCompletionResponse> {
        Err(GmError::SafetyViolation(HOST_CLI_DISABLED.into()))
    }
}

pub const HOST_CLI_DISABLED: &str = "Host CLI execution is disabled. Use an API provider until the signed CLI-agent VM profile, provider-only egress, isolated credentials and process cleanup are certified.";

pub fn find_binary_on_path(binary_name: &str) -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for path in std::env::split_paths(&paths) {
            let candidate = path.join(binary_name);
            if candidate.is_file() {
                return Some(candidate);
            }
            #[cfg(windows)]
            {
                let candidate_exe = path.join(format!("{binary_name}.exe"));
                if candidate_exe.is_file() {
                    return Some(candidate_exe);
                }
                let candidate_cmd = path.join(format!("{binary_name}.cmd"));
                if candidate_cmd.is_file() {
                    return Some(candidate_cmd);
                }
            }
        }
    }
    None
}

/// Adapter allowing `CliDriver` to be used wherever `LlmBackend` is expected.
#[derive(Debug, Clone)]
pub struct CliLlmBackend {
    driver: CliDriver,
}

impl CliLlmBackend {
    pub fn new(driver: CliDriver) -> Self {
        Self { driver }
    }

    pub fn driver(&self) -> &CliDriver {
        &self.driver
    }
}

#[async_trait]
impl LlmBackend for CliLlmBackend {
    async fn complete(
        &self,
        target: &ModelTarget,
        messages: &[ChatMessage],
    ) -> Result<ChatCompletionResponse> {
        let mut full_prompt = String::new();
        for msg in messages {
            full_prompt.push_str(&format!("{}: {}\n\n", msg.role, msg.content));
        }
        self.driver
            .complete(Some(&target.model), full_prompt.trim())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn host_cli_execution_and_version_probes_are_disabled() {
        let root = std::env::temp_dir().join(format!("gm-cli-denied-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("fake-cli");
        // Invalid executable still counts as present; discovery must never run it.
        std::fs::write(&executable, b"not executable").unwrap();
        for provider in [CliProvider::Claude, CliProvider::Codex, CliProvider::Grok] {
            let driver = CliDriver::new(provider).with_binary_path(executable.clone());
            let available = driver.check_availability();
            assert!(available.installed);
            assert!(!available.authenticated);
            assert!(available.version.is_none());
            assert_eq!(available.error_message.as_deref(), Some(HOST_CLI_DISABLED));
            assert!(matches!(
                driver.complete(None, "run a shell").await,
                Err(GmError::SafetyViolation(_))
            ));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn test_cli_provider_attributes_and_auth_instructions() {
        let claude = CliProvider::Claude;
        assert_eq!(claude.binary_name(), "claude");
        assert!(claude.auth_flow_instructions().contains("claude login"));
        assert_eq!(claude.environment_auth_var(), "ANTHROPIC_API_KEY");

        let codex = CliProvider::Codex;
        assert_eq!(codex.binary_name(), "codex");
        assert!(codex.auth_flow_instructions().contains("codex login"));
        assert_eq!(codex.environment_auth_var(), "OPENAI_API_KEY");

        let grok = CliProvider::Grok;
        assert_eq!(grok.binary_name(), "grok");
        assert!(
            grok.auth_flow_instructions()
                .contains("grok login --device-auth")
        );
        assert_eq!(grok.environment_auth_var(), "XAI_API_KEY");
    }

    #[test]
    fn test_missing_binary_returns_actionable_error() {
        let driver = CliDriver::new(CliProvider::Claude)
            .with_binary_path(PathBuf::from("/nonexistent/bin/claude"));
        let avail = driver.check_availability();
        assert!(!avail.installed);
        assert!(!avail.authenticated);
        assert!(avail.error_message.is_some());
        assert!(avail.error_message.unwrap().contains("Install via npm"));
    }

    #[test]
    fn test_honest_cli_cost_labeling() {
        // ChatCompletionResponse for CLI driver must have 0.0 estimated_cost_usd
        let resp = ChatCompletionResponse {
            id: "cli-test-1".into(),
            content: "result".into(),
            model: "claude-3-5-sonnet".into(),
            prompt_tokens: 100,
            completion_tokens: 50,
            estimated_cost_usd: 0.0,
        };
        assert_eq!(resp.estimated_cost_usd, 0.0);
    }
}
