use gm_core::error::{GmError, Result};
use tracing::{debug, info};

pub const DEFAULT_KEYCHAIN_SERVICE: &str = "co.anysphere.grok-muse";
pub const META_KEY_ACCOUNT: &str = "meta_api_key";
pub const XAI_KEY_ACCOUNT: &str = "xai_api_key";

/// macOS Keychain Secret Manager for Meta and xAI API Keys.
#[derive(Debug, Clone)]
pub struct KeychainSecrets {
    service: String,
}

impl Default for KeychainSecrets {
    fn default() -> Self {
        Self::new(DEFAULT_KEYCHAIN_SERVICE)
    }
}

impl KeychainSecrets {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    /// Retrieve secret from OS keychain, falling back to environment variable if absent.
    pub fn get_secret(&self, account: &str) -> Result<String> {
        #[cfg(target_os = "macos")]
        {
            match security_framework::passwords::get_generic_password(&self.service, account) {
                Ok(bytes) => {
                    let secret = String::from_utf8(bytes).map_err(|e| {
                        GmError::Config(format!("Secret for {account} is invalid UTF-8: {e}"))
                    })?;
                    debug!("Loaded secret '{account}' from macOS Keychain");
                    return Ok(secret);
                }
                Err(e) => {
                    debug!("Keychain lookup for '{account}' failed: {e}");
                }
            }
        }

        // Fallback to environment variables
        let env_var_name = account.to_uppercase();
        if let Ok(val) = std::env::var(&env_var_name) {
            debug!("Loaded secret '{account}' from environment variable {env_var_name}");
            return Ok(val);
        }

        // Check provider-specific aliases (e.g. GROK_API_KEY, MUSE_API_KEY)
        let alias = match account {
            META_KEY_ACCOUNT => Some("MUSE_API_KEY"),
            XAI_KEY_ACCOUNT => Some("GROK_API_KEY"),
            _ => None,
        };
        if let Some(alt) = alias
            && let Ok(val) = std::env::var(alt)
        {
            debug!("Loaded secret '{account}' from environment variable alias {alt}");
            return Ok(val);
        }

        Err(GmError::Config(format!(
            "Secret '{account}' not found in macOS Keychain or environment"
        )))
    }

    /// Save secret to macOS Keychain (overwrites existing key if present).
    pub fn set_secret(&self, account: &str, secret: &str) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            // Delete existing secret first if present so that set_generic_password
            // (which calls SecKeychainAddGenericPassword) does not fail with errSecDuplicateItem (-25299).
            let _ = security_framework::passwords::delete_generic_password(&self.service, account);

            security_framework::passwords::set_generic_password(
                &self.service,
                account,
                secret.as_bytes(),
            )
            .map_err(|e| {
                GmError::Config(format!("Failed to save '{account}' to macOS Keychain: {e}"))
            })?;
            info!(
                "Stored secret '{account}' in macOS Keychain (service: {})",
                self.service
            );
            Ok(())
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (account, secret);
            Err(GmError::Config(
                "Keychain storage is only supported on macOS".into(),
            ))
        }
    }

    /// Delete secret from macOS Keychain.
    pub fn delete_secret(&self, account: &str) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            security_framework::passwords::delete_generic_password(&self.service, account)
                .map_err(|e| {
                    GmError::Config(format!(
                        "Failed to delete '{account}' from macOS Keychain: {e}"
                    ))
                })?;
            info!("Deleted secret '{account}' from macOS Keychain");
            Ok(())
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = account;
            Err(GmError::Config(
                "Keychain storage is only supported on macOS".into(),
            ))
        }
    }

    pub fn get_meta_key(&self) -> Result<String> {
        self.get_secret(META_KEY_ACCOUNT)
    }

    pub fn set_meta_key(&self, key: &str) -> Result<()> {
        self.set_secret(META_KEY_ACCOUNT, key)
    }

    pub fn get_xai_key(&self) -> Result<String> {
        self.get_secret(XAI_KEY_ACCOUNT)
    }

    pub fn set_xai_key(&self, key: &str) -> Result<()> {
        self.set_secret(XAI_KEY_ACCOUNT, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keychain_env_fallback() {
        let test_account = "TEST_TEMP_API_KEY";
        unsafe {
            std::env::set_var(test_account, "test-key-val-123");
        }

        let secrets = KeychainSecrets::new("co.anysphere.grok-muse.test");
        let val = secrets.get_secret(test_account).unwrap();
        assert_eq!(val, "test-key-val-123");

        unsafe {
            std::env::remove_var(test_account);
        }
    }

    #[test]
    fn test_keychain_env_alias_fallback() {
        unsafe {
            std::env::set_var("GROK_API_KEY", "grok-secret-abc");
        }

        let secrets = KeychainSecrets::new("co.anysphere.grok-muse.test-alias");
        let val = secrets.get_xai_key().unwrap();
        assert_eq!(val, "grok-secret-abc");

        unsafe {
            std::env::remove_var("GROK_API_KEY");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_keychain_roundtrip_macos() {
        let secrets = KeychainSecrets::new("co.anysphere.grok-muse.unit-test");
        let account = "unit_test_key";
        let secret_val1 = "sk-test-super-secret-12345";
        let secret_val2 = "sk-test-updated-secret-67890";

        // Cleanup any stale key from previous run
        let _ = secrets.delete_secret(account);

        // Store first value
        secrets
            .set_secret(account, secret_val1)
            .expect("store secret");

        // Retrieve
        let retrieved = secrets.get_secret(account).expect("retrieve secret");
        assert_eq!(retrieved, secret_val1);

        // Update (verify overwriting existing key works without errSecDuplicateItem)
        secrets
            .set_secret(account, secret_val2)
            .expect("overwrite secret");

        let retrieved_updated = secrets.get_secret(account).expect("retrieve updated");
        assert_eq!(retrieved_updated, secret_val2);

        // Delete
        secrets.delete_secret(account).expect("delete secret");

        // Verify gone (when env var not set)
        assert!(secrets.get_secret(account).is_err());
    }
}
