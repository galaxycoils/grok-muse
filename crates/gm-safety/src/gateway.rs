use crate::blocklist::BlocklistManager;
use crate::tiers::ActionTier;
use gm_core::types::ComputerAction;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyOutcome {
    Permitted,
    NeedsApproval,
    Refused,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyDecision {
    pub outcome: PolicyOutcome,
    pub tier: ActionTier,
    pub rule_id: &'static str,
    pub citation: String,
}

/// Central pre-execution security gateway (OpenBot Gateway Security Pattern).
/// Every GUI action, workspace file access, shell command, and secret lease is evaluated
/// before execution and stamped with an explicit governance rule citation.
#[derive(Debug, Clone)]
pub struct SecurityGateway {
    blocklist: BlocklistManager,
}

impl SecurityGateway {
    pub fn new(blocklist: BlocklistManager) -> Self {
        Self { blocklist }
    }

    /// Evaluate a GUI / ComputerAction against blocklist and tier policies.
    pub fn evaluate_gui_action(
        &self,
        app_name: Option<&str>,
        action: &ComputerAction,
        target_label: Option<&str>,
    ) -> PolicyDecision {
        if let Some(app) = app_name
            && self.blocklist.is_blocked(app, target_label.unwrap_or(""))
        {
            return PolicyDecision {
                outcome: PolicyOutcome::Refused,
                tier: ActionTier::Irreversible,
                rule_id: "RULE-BLK-001",
                citation: format!(
                    "Target application '{app}' is in SafetyConfig.blocklisted_apps and cannot be accessed"
                ),
            };
        }

        match action {
            ComputerAction::FocusApp { app_name } if self.blocklist.is_blocked(app_name, "") => {
                return PolicyDecision {
                    outcome: PolicyOutcome::Refused,
                    tier: ActionTier::Irreversible,
                    rule_id: "RULE-BLK-001",
                    citation: format!(
                        "Focusing blocked application '{app_name}' is prohibited by RULE-BLK-001"
                    ),
                };
            }
            ComputerAction::LaunchApp { bundle_id } if self.blocklist.is_blocked(bundle_id, "") => {
                return PolicyDecision {
                    outcome: PolicyOutcome::Refused,
                    tier: ActionTier::Irreversible,
                    rule_id: "RULE-BLK-001",
                    citation: format!(
                        "Launching blocked bundle '{bundle_id}' is prohibited by RULE-BLK-001"
                    ),
                };
            }
            _ => {}
        }

        let tier = ActionTier::classify(action, target_label);
        if tier.requires_confirmation() {
            PolicyDecision {
                outcome: PolicyOutcome::NeedsApproval,
                tier,
                rule_id: "RULE-GUI-005",
                citation: format!(
                    "Destructive or external GUI action ({:?} on {:?}) requires explicit operator confirmation",
                    action, target_label
                ),
            }
        } else {
            PolicyDecision {
                outcome: PolicyOutcome::Permitted,
                tier,
                rule_id: "RULE-STD-000",
                citation: "Standard read or reversible GUI action permitted".into(),
            }
        }
    }

    /// Evaluate a workspace file path access against bot root confinement.
    pub fn evaluate_workspace_path(
        &self,
        bot_root: &Path,
        candidate_path: &Path,
        is_write: bool,
    ) -> PolicyDecision {
        let canonical_root = bot_root
            .canonicalize()
            .unwrap_or_else(|_| bot_root.to_path_buf());

        // Check lexical traversal (`..`) or absolute escape outside bot_root
        let mut normalized = std::path::PathBuf::new();
        let joined = if candidate_path.is_absolute() {
            candidate_path.to_path_buf()
        } else {
            canonical_root.join(candidate_path)
        };

        for comp in joined.components() {
            match comp {
                std::path::Component::ParentDir => {
                    normalized.pop();
                }
                std::path::Component::CurDir => {}
                other => normalized.push(other.as_os_str()),
            }
        }

        // Also check real canonical path if parent exists
        let effective = if normalized.exists() {
            normalized.canonicalize().unwrap_or(normalized)
        } else if let Some(parent) = normalized.parent()
            && parent.exists()
        {
            if let Ok(canon_parent) = parent.canonicalize() {
                if let Some(name) = normalized.file_name() {
                    canon_parent.join(name)
                } else {
                    canon_parent
                }
            } else {
                normalized
            }
        } else {
            normalized
        };

        if !effective.starts_with(&canonical_root) {
            return PolicyDecision {
                outcome: PolicyOutcome::Refused,
                tier: ActionTier::Irreversible,
                rule_id: "RULE-PATH-002",
                citation: format!(
                    "Path '{}' escapes confined bot root '{}'",
                    candidate_path.display(),
                    canonical_root.display()
                ),
            };
        }

        PolicyDecision {
            outcome: PolicyOutcome::Permitted,
            tier: if is_write {
                ActionTier::Reversible
            } else {
                ActionTier::Read
            },
            rule_id: "RULE-STD-000",
            citation: "Confined bot workspace file operation permitted".into(),
        }
    }

    /// Evaluate a workspace shell command before execution.
    pub fn evaluate_shell_command(&self, command: &str) -> PolicyDecision {
        let lower = command.to_lowercase();

        // Hard refusal on privilege escalation, keychain dumping, or sandbox bypass
        let forbidden = [
            "sudo ",
            "sudo\t",
            "su -",
            "security dump-keychain",
            "security find-generic-password",
            "launchctl ",
            "csrutil ",
            "dscl ",
        ];
        for pat in forbidden {
            if lower.contains(pat) || lower == "sudo" {
                return PolicyDecision {
                    outcome: PolicyOutcome::Refused,
                    tier: ActionTier::Irreversible,
                    rule_id: "RULE-SH-003",
                    citation: format!(
                        "Privilege escalation or credential-dumping pattern '{pat}' is refused by RULE-SH-003"
                    ),
                };
            }
        }

        // Mutating / destructive / outbound push patterns require interactive confirmation
        let needs_approval = [
            "rm -rf",
            "rm -r ",
            "git push",
            "curl -x post",
            "curl -d ",
            "curl --data",
            "dd if=",
            "mkfs",
            "kill -9",
            "pkill ",
        ];
        for pat in needs_approval {
            if lower.contains(pat) {
                return PolicyDecision {
                    outcome: PolicyOutcome::NeedsApproval,
                    tier: ActionTier::Irreversible,
                    rule_id: "RULE-SH-004",
                    citation: format!(
                        "Potentially destructive or external shell command matching '{pat}' requires confirmation"
                    ),
                };
            }
        }

        PolicyDecision {
            outcome: PolicyOutcome::Permitted,
            tier: ActionTier::Reversible,
            rule_id: "RULE-STD-000",
            citation: "Seatbelt-confined workspace shell command permitted".into(),
        }
    }

    /// Evaluate an opaque vault secret lease (`secret://...`).
    pub fn evaluate_secret_lease(
        &self,
        secret_ref: &str,
        approved_in_session: bool,
    ) -> PolicyDecision {
        if !secret_ref.starts_with("secret://keychain/")
            && !secret_ref.starts_with("secret://1password/")
        {
            return PolicyDecision {
                outcome: PolicyOutcome::Refused,
                tier: ActionTier::Irreversible,
                rule_id: "RULE-SEC-006",
                citation: format!(
                    "Invalid or unsupported secret URI '{secret_ref}' refused by RULE-SEC-006"
                ),
            };
        }

        if approved_in_session {
            PolicyDecision {
                outcome: PolicyOutcome::Permitted,
                tier: ActionTier::Reversible,
                rule_id: "RULE-SEC-006",
                citation: format!("Opaque secret lease '{secret_ref}' permitted within session"),
            }
        } else {
            PolicyDecision {
                outcome: PolicyOutcome::NeedsApproval,
                tier: ActionTier::Irreversible,
                rule_id: "RULE-SEC-006",
                citation: format!(
                    "First-use lease of opaque vault credential '{secret_ref}' requires operator approval"
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gateway_refuses_blocklisted_app_and_cites_rule() {
        let gw = SecurityGateway::new(BlocklistManager::new(&["1Password".into()]));
        let action = ComputerAction::Click {
            token: Some("btn_1".into()),
            coords: None,
            button: None,
            click_count: None,
        };
        let dec = gw.evaluate_gui_action(Some("1Password"), &action, Some("Unlock"));
        assert_eq!(dec.outcome, PolicyOutcome::Refused);
        assert_eq!(dec.rule_id, "RULE-BLK-001");
    }

    #[test]
    fn test_gateway_workspace_path_confinement() {
        let gw = SecurityGateway::new(BlocklistManager::new(&[]));
        let root = std::env::temp_dir().join(format!("gw_bot_test_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();

        let ok_dec = gw.evaluate_workspace_path(&root, Path::new("workspace/hello.txt"), true);
        assert_eq!(ok_dec.outcome, PolicyOutcome::Permitted);
        assert_eq!(ok_dec.rule_id, "RULE-STD-000");

        let escape_dec = gw.evaluate_workspace_path(&root, Path::new("../../etc/passwd"), false);
        assert_eq!(escape_dec.outcome, PolicyOutcome::Refused);
        assert_eq!(escape_dec.rule_id, "RULE-PATH-002");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_gateway_shell_rules() {
        let gw = SecurityGateway::new(BlocklistManager::new(&[]));

        let r1 = gw.evaluate_shell_command("sudo rm -rf /");
        assert_eq!(r1.outcome, PolicyOutcome::Refused);
        assert_eq!(r1.rule_id, "RULE-SH-003");

        let r2 = gw.evaluate_shell_command("git push origin main");
        assert_eq!(r2.outcome, PolicyOutcome::NeedsApproval);
        assert_eq!(r2.rule_id, "RULE-SH-004");

        let r3 = gw.evaluate_shell_command("cargo test --quiet");
        assert_eq!(r3.outcome, PolicyOutcome::Permitted);
        assert_eq!(r3.rule_id, "RULE-STD-000");
    }
}
