use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct BlocklistManager {
    blocked_patterns: HashSet<String>,
}

impl Default for BlocklistManager {
    fn default() -> Self {
        let mut set = HashSet::new();
        for app in [
            "1password",
            "keychain access",
            "bitwarden",
            "keepassxc",
            "dashlane",
            "lastpass",
            "authenticator",
            "com.apple.keychainaccess",
        ] {
            set.insert(app.to_string());
        }
        Self {
            blocked_patterns: set,
        }
    }
}

impl BlocklistManager {
    pub fn new(additional: &[String]) -> Self {
        let mut mgr = Self::default();
        for item in additional {
            mgr.blocked_patterns.insert(item.to_lowercase());
        }
        mgr
    }

    /// Check if an app or window title belongs to a blocklisted sensitive application.
    pub fn is_blocked(&self, app_name: &str, window_title: &str) -> bool {
        let lower_app = app_name.to_lowercase();
        let lower_title = window_title.to_lowercase();

        for pattern in &self.blocked_patterns {
            if lower_app.contains(pattern) || lower_title.contains(pattern) {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blocklist() {
        let mgr = BlocklistManager::default();
        assert!(mgr.is_blocked("1Password", "Vault"));
        assert!(mgr.is_blocked("Safari", "Keychain Access - Passwords"));
        assert!(!mgr.is_blocked("Notes", "Shopping list"));
    }
}
