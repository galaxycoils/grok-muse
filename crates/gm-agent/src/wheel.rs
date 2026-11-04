use crate::workspace_tools::WorkspaceToolCall;
use gm_core::types::AxSnapshot;
use serde::{Deserialize, Serialize};

/// Request emitted when an agent encounters a blocking wall (CAPTCHA, 2FA/MFA, login prompt)
/// and hands control back to a human operator ("Take the Wheel").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WheelHandoffRequest {
    pub task_id: String,
    pub bot_id: Option<String>,
    pub step: u32,
    pub blocker: String,
    pub instructions: String,
}

/// Extended OpenBot step actions emitted by the model alongside standard GUI and MCP actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OpenBotStepAction {
    TakeTheWheel {
        blocker: String,
        instructions: String,
    },
    FillSecret {
        token: Option<String>,
        secret_ref: String,
    },
    WorkspaceTool(WorkspaceToolCall),
    DelegateToBot {
        target_bot_id: String,
        goal: String,
        #[serde(default)]
        artifacts: Vec<String>,
    },
}

/// Detects CAPTCHA, Turnstile, MFA/2FA, or SSO login walls in a sanitized AX snapshot.
pub struct BlockerDetector;

impl BlockerDetector {
    pub fn detect_in_snapshot(snapshot: &AxSnapshot) -> Option<String> {
        let title_lower = snapshot.window_title.to_lowercase();
        let captcha_keywords = [
            "verify you are human",
            "cloudflare turnstile",
            "recaptcha",
            "hcaptcha",
            "security challenge",
        ];
        for kw in captcha_keywords {
            if title_lower.contains(kw) {
                return Some(format!(
                    "CAPTCHA / human verification wall detected in window '{}'",
                    snapshot.window_title
                ));
            }
        }

        for el in &snapshot.elements {
            let lbl = el.label.as_deref().unwrap_or("").to_lowercase();
            let val = el.value.as_deref().unwrap_or("").to_lowercase();
            let combined = format!("{lbl} {val}");

            for kw in captcha_keywords {
                if combined.contains(kw) {
                    return Some(format!(
                        "CAPTCHA challenge element ('{}') requires human interaction",
                        el.label.as_deref().unwrap_or(&el.token)
                    ));
                }
            }

            let mfa_keywords = [
                "two-factor authentication",
                "enter 6-digit code",
                "authenticator code",
                "hardware security key",
                "touch id to continue",
            ];
            for kw in mfa_keywords {
                if combined.contains(kw) {
                    return Some(format!(
                        "MFA / 2FA verification wall ('{}') requires human interaction",
                        el.label.as_deref().unwrap_or(&el.token)
                    ));
                }
            }
        }

        None
    }
}

/// Parses an OpenBot-specific JSON action (`TakeTheWheel`, `FillSecret`, `WorkspaceTool`, `DelegateToBot`)
/// from the raw model completion string before falling back to `ComputerAction`.
pub fn try_parse_openbot_step_action(raw: &str) -> Option<OpenBotStepAction> {
    let trimmed = raw.trim();
    let json_str = if let Some(start) = trimmed.find('{')
        && let Some(end) = trimmed.rfind('}')
        && end >= start
    {
        &trimmed[start..=end]
    } else {
        trimmed
    };

    let val: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let action_name = val.get("action")?.as_str()?;
    let params = val.get("params").cloned().unwrap_or(serde_json::json!({}));

    match action_name {
        "TakeTheWheel" | "take_the_wheel" => {
            let blocker = params
                .get("blocker")
                .and_then(|v| v.as_str())
                .unwrap_or("Human intervention required")
                .to_string();
            let instructions = params
                .get("instructions")
                .and_then(|v| v.as_str())
                .unwrap_or("Please complete the blocking step and click 'Hand Back to Bot'")
                .to_string();
            Some(OpenBotStepAction::TakeTheWheel {
                blocker,
                instructions,
            })
        }
        "FillSecret" | "fill_secret" => {
            let secret_ref = params.get("secret_ref")?.as_str()?.to_string();
            let token = params
                .get("token")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some(OpenBotStepAction::FillSecret { token, secret_ref })
        }
        "WorkspaceReadFile" | "workspace_read_file" | "read_file" => {
            let path = params.get("path")?.as_str()?.to_string();
            Some(OpenBotStepAction::WorkspaceTool(
                WorkspaceToolCall::ReadFile { path },
            ))
        }
        "WorkspaceWriteFile" | "workspace_write_file" | "write_file" => {
            let path = params.get("path")?.as_str()?.to_string();
            let content = params
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(OpenBotStepAction::WorkspaceTool(
                WorkspaceToolCall::WriteFile { path, content },
            ))
        }
        "WorkspaceListFiles" | "workspace_list_files" | "list_files" => {
            let path = params
                .get("path")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some(OpenBotStepAction::WorkspaceTool(
                WorkspaceToolCall::ListFiles { path },
            ))
        }
        "WorkspaceShell" | "workspace_shell" | "shell" => {
            let command = params.get("command")?.as_str()?.to_string();
            let timeout_ms = params.get("timeout_ms").and_then(|v| v.as_u64());
            Some(OpenBotStepAction::WorkspaceTool(WorkspaceToolCall::Shell {
                command,
                timeout_ms,
            }))
        }
        "DelegateToBot" | "delegate_to_bot" => {
            let target_bot_id = params
                .get("target_bot_id")
                .or_else(|| params.get("bot_id"))?
                .as_str()?
                .to_string();
            let goal = params.get("goal")?.as_str()?.to_string();
            let artifacts = params
                .get("artifacts")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            Some(OpenBotStepAction::DelegateToBot {
                target_bot_id,
                goal,
                artifacts,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gm_core::types::{AxElement, Rect};

    #[test]
    fn test_blocker_detector_finds_captcha_and_mfa() {
        let snap = AxSnapshot {
            window_id: 1,
            app_name: "Safari".into(),
            window_title: "Sign In".into(),
            bounds: Rect::new(0.0, 0.0, 800.0, 600.0),
            elements: vec![AxElement {
                token: "el_1".into(),
                role: "AXStaticText".into(),
                label: Some("Enter 6-digit code from your authenticator".into()),
                value: None,
                frame: Rect::new(10.0, 10.0, 200.0, 30.0),
                is_enabled: true,
                is_secure: false,
            }],
            timestamp_ms: 1,
        };
        let detected = BlockerDetector::detect_in_snapshot(&snap);
        assert!(detected.is_some());
        assert!(detected.unwrap().contains("MFA / 2FA"));
    }

    #[test]
    fn test_parse_openbot_step_actions() {
        let w = try_parse_openbot_step_action(
            r#"{"action": "TakeTheWheel", "params": {"blocker": "SSO MFA", "instructions": "Approve Duo push"}}"#,
        )
        .unwrap();
        match w {
            OpenBotStepAction::TakeTheWheel {
                blocker,
                instructions,
            } => {
                assert_eq!(blocker, "SSO MFA");
                assert_eq!(instructions, "Approve Duo push");
            }
            _ => panic!("Expected TakeTheWheel"),
        }

        let s = try_parse_openbot_step_action(
            r#"{"action": "FillSecret", "params": {"token": "pwd_box", "secret_ref": "secret://keychain/corp_sso"}}"#,
        )
        .unwrap();
        match s {
            OpenBotStepAction::FillSecret { token, secret_ref } => {
                assert_eq!(token.as_deref(), Some("pwd_box"));
                assert_eq!(secret_ref, "secret://keychain/corp_sso");
            }
            _ => panic!("Expected FillSecret"),
        }
    }
}
