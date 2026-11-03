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
        match action {
            ComputerAction::Wait { .. } => ActionTier::Read,
            ComputerAction::FocusApp { .. } => ActionTier::Reversible,
            ComputerAction::Scroll { .. } => ActionTier::Reversible,
            ComputerAction::Done { .. } => ActionTier::Read,
            ComputerAction::AskUser { .. } => ActionTier::Read,

            ComputerAction::Click { .. } => {
                if let Some(lbl) = target_label {
                    let lower = lbl.to_lowercase();
                    if lower.contains("delete")
                        || lower.contains("remove")
                        || lower.contains("trash")
                        || lower.contains("send")
                        || lower.contains("buy")
                        || lower.contains("purchase")
                        || lower.contains("pay")
                        || lower.contains("confirm order")
                    {
                        return ActionTier::Irreversible;
                    }
                }
                ActionTier::Reversible
            }

            ComputerAction::Type { text } => {
                let lower = text.to_lowercase();
                if lower.contains("rm -rf")
                    || lower.contains("sudo")
                    || lower.contains("git push --force")
                    || lower.contains("format")
                {
                    ActionTier::Irreversible
                } else {
                    ActionTier::Reversible
                }
            }

            ComputerAction::KeyCombo { .. } => ActionTier::Reversible,

            ComputerAction::Drag { .. } => ActionTier::Reversible,
            ComputerAction::Menu { path } => {
                if path.iter().any(|p| {
                    let l = p.to_lowercase();
                    l.contains("delete") || l.contains("close all") || l.contains("quit")
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
    }
}
