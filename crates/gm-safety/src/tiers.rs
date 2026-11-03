use gm_core::types::ComputerAction;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ActionTier {
    Read,
    Reversible,
    Irreversible,
}

impl ActionTier {
    pub fn requires_confirmation(&self) -> bool {
        matches!(self, ActionTier::Irreversible)
    }

    /// Classify an action and its context into an approval tier.
    pub fn classify(action: &ComputerAction, target_label: Option<&str>) -> Self {
        let is_destructive_str = |text: &str| -> bool {
            let lower = text.to_lowercase();
            lower.contains("delete")
                || lower.contains("remove")
                || lower.contains("trash")
                || lower.contains("empty trash")
                || lower.contains("erase")
                || (lower.contains("format")
                    && !lower.contains("format document")
                    && !lower.contains("format code")
                    && !lower.contains("format selection")
                    && !lower.contains("format file")
                    && !lower.contains("format text"))
                || lower.contains("send")
                || lower.contains("buy")
                || lower.contains("purchase")
                || lower.contains("pay")
                || lower.contains("checkout")
                || lower.contains("transfer")
                || lower.contains("confirm order")
                || lower.contains("close account")
        };

        match action {
            ComputerAction::Wait { .. } => ActionTier::Read,
            ComputerAction::FocusApp { .. } => ActionTier::Reversible,
            ComputerAction::Scroll { .. } => ActionTier::Reversible,
            ComputerAction::Done { .. } => ActionTier::Read,
            ComputerAction::AskUser { .. } => ActionTier::Read,

            ComputerAction::Click { token, .. } => {
                if let Some(lbl) = target_label
                    && is_destructive_str(lbl)
                {
                    return ActionTier::Irreversible;
                }
                // Fallback check on token name if label wasn't matched
                if let Some(tok) = token
                    && is_destructive_str(tok)
                {
                    return ActionTier::Irreversible;
                }
                ActionTier::Reversible
            }

            ComputerAction::Type { text } => {
                let lower = text.to_lowercase();
                if lower.contains("rm -rf")
                    || lower.contains("rm -r ")
                    || lower.contains("sudo")
                    || lower.contains("git push --force")
                    || lower.contains("git push -f")
                    || lower.contains("format ")
                    || lower.contains("dd if=")
                    || lower.contains("mkfs")
                    || lower.contains("drop database")
                    || lower.contains("drop table")
                {
                    ActionTier::Irreversible
                } else {
                    ActionTier::Reversible
                }
            }

            ComputerAction::KeyCombo { modifiers, key } => {
                let key_lower = key.to_lowercase();
                let is_cmd = modifiers.iter().any(|m| {
                    let ml = m.to_lowercase();
                    ml == "command" || ml == "cmd" || ml == "meta"
                });
                let is_shift = modifiers.iter().any(|m| m.to_lowercase() == "shift");

                // Cmd+Delete or Cmd+Backspace (Move to Trash / Delete)
                if is_cmd && (key_lower == "delete" || key_lower == "backspace") {
                    return ActionTier::Irreversible;
                }
                // Cmd+Shift+D or Cmd+Return (Send message / email in Mail / Slack)
                if is_cmd
                    && (key_lower == "return"
                        || key_lower == "enter"
                        || (is_shift && key_lower == "d"))
                {
                    return ActionTier::Irreversible;
                }

                ActionTier::Reversible
            }

            ComputerAction::Drag { .. } => ActionTier::Reversible,
            ComputerAction::Menu { path } => {
                if path.iter().any(|p| {
                    let l = p.to_lowercase();
                    is_destructive_str(&l) || l.contains("close all") || l.contains("quit")
                }) {
                    ActionTier::Irreversible
                } else {
                    ActionTier::Reversible
                }
            }
            ComputerAction::LaunchApp { .. } => ActionTier::Reversible,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tier_classification() {
        let scroll = ComputerAction::Scroll { dx: 0, dy: -100 };
        assert_eq!(ActionTier::classify(&scroll, None), ActionTier::Reversible);

        let click_cancel = ComputerAction::Click {
            token: Some("btn_cancel".into()),
            coords: None,
            button: None,
            click_count: None,
        };
        assert_eq!(
            ActionTier::classify(&click_cancel, Some("Cancel")),
            ActionTier::Reversible
        );

        let click_delete = ComputerAction::Click {
            token: Some("btn_del".into()),
            coords: None,
            button: None,
            click_count: None,
        };
        assert_eq!(
            ActionTier::classify(&click_delete, Some("Delete All Files")),
            ActionTier::Irreversible
        );
        assert!(
            ActionTier::classify(&click_delete, Some("Delete All Files")).requires_confirmation()
        );

        // Token fallback check when label is None
        let click_send = ComputerAction::Click {
            token: Some("send_btn".into()),
            coords: None,
            button: None,
            click_count: None,
        };
        assert_eq!(
            ActionTier::classify(&click_send, None),
            ActionTier::Irreversible
        );

        // Key combos (Cmd+Delete, Cmd+Return, Cmd+Shift+D)
        let key_cmd_delete = ComputerAction::KeyCombo {
            modifiers: vec!["Command".into()],
            key: "Delete".into(),
        };
        assert_eq!(
            ActionTier::classify(&key_cmd_delete, None),
            ActionTier::Irreversible
        );

        let key_send_email = ComputerAction::KeyCombo {
            modifiers: vec!["Command".into(), "Shift".into()],
            key: "d".into(),
        };
        assert_eq!(
            ActionTier::classify(&key_send_email, None),
            ActionTier::Irreversible
        );

        // Menu items
        let menu_empty_trash = ComputerAction::Menu {
            path: vec!["Finder".into(), "Empty Trash".into()],
        };
        assert_eq!(
            ActionTier::classify(&menu_empty_trash, None),
            ActionTier::Irreversible
        );

        // Dangerous typing
        let type_rm = ComputerAction::Type {
            text: "rm -rf /Users/cmd/important".into(),
        };
        assert_eq!(
            ActionTier::classify(&type_rm, None),
            ActionTier::Irreversible
        );
    }
}
