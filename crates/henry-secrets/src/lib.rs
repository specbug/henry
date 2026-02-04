//! 1Password CLI integration for secret management.
//!
//! Uses the `op` CLI to retrieve secrets from 1Password vaults.
//! Secrets are referenced using 1Password secret references:
//! `op://vault/item/field`

use std::process::Stdio;
use thiserror::Error;
use tokio::process::Command;
use tracing::debug;

#[derive(Error, Debug)]
pub enum SecretsError {
    #[error("1Password CLI not found. Install with: brew install 1password-cli")]
    OpNotFound,

    #[error("1Password CLI error: {0}")]
    OpError(String),

    #[error("failed to execute op command: {0}")]
    Io(#[source] std::io::Error),

    #[error("invalid secret reference format: {0}")]
    InvalidReference(String),

    #[error("secret not found: {0}")]
    NotFound(String),

    #[error("1Password session expired. Run: op signin")]
    SessionExpired,
}

/// Secret manager using 1Password CLI.
pub struct SecretManager {
    /// Cache secrets in memory (cleared on drop).
    cache: std::collections::HashMap<String, String>,
    /// Whether to use caching.
    use_cache: bool,
}

impl SecretManager {
    /// Create a new secret manager.
    pub fn new() -> Self {
        Self {
            cache: std::collections::HashMap::new(),
            use_cache: true,
        }
    }

    /// Create a secret manager without caching.
    pub fn without_cache() -> Self {
        Self {
            cache: std::collections::HashMap::new(),
            use_cache: false,
        }
    }

    /// Check if 1Password CLI is available and authenticated.
    pub async fn check_availability(&self) -> Result<bool, SecretsError> {
        match Command::new("op")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
        {
            Ok(status) => Ok(status.success()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(SecretsError::OpNotFound),
            Err(e) => Err(SecretsError::Io(e)),
        }
    }

    /// Get a secret by its 1Password reference.
    ///
    /// Reference format: `op://vault/item/field`
    pub async fn get(&mut self, reference: &str) -> Result<String, SecretsError> {
        // Validate reference format
        if !reference.starts_with("op://") {
            return Err(SecretsError::InvalidReference(reference.to_string()));
        }

        // Check cache
        if self.use_cache {
            if let Some(cached) = self.cache.get(reference) {
                debug!("Secret cache hit for {}", Self::redact_reference(reference));
                return Ok(cached.clone());
            }
        }

        // Fetch from 1Password
        let secret = self.fetch_secret(reference).await?;

        // Cache the result
        if self.use_cache {
            self.cache.insert(reference.to_string(), secret.clone());
        }

        Ok(secret)
    }

    /// Get a secret, returning None if not found instead of error.
    pub async fn get_optional(&mut self, reference: &str) -> Result<Option<String>, SecretsError> {
        match self.get(reference).await {
            Ok(secret) => Ok(Some(secret)),
            Err(SecretsError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn fetch_secret(&self, reference: &str) -> Result<String, SecretsError> {
        debug!("Fetching secret: {}", Self::redact_reference(reference));

        let output = Command::new("op")
            .args(["read", reference])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    SecretsError::OpNotFound
                } else {
                    SecretsError::Io(e)
                }
            })?;

        if output.status.success() {
            let secret = String::from_utf8_lossy(&output.stdout)
                .trim()
                .to_string();
            Ok(secret)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            if stderr.contains("not signed in") || stderr.contains("session expired") {
                Err(SecretsError::SessionExpired)
            } else if stderr.contains("isn't an item") || stderr.contains("not found") {
                Err(SecretsError::NotFound(reference.to_string()))
            } else {
                Err(SecretsError::OpError(stderr))
            }
        }
    }

    /// Clear the secret cache (secrets are zeroed).
    pub fn clear_cache(&mut self) {
        // Zero out cached secrets before dropping
        for (_, secret) in self.cache.iter_mut() {
            secret.clear();
            secret.shrink_to_fit();
        }
        self.cache.clear();
    }

    /// Redact a secret reference for logging (shows vault/item but not field).
    fn redact_reference(reference: &str) -> String {
        // op://vault/item/field -> op://vault/item/*****
        if let Some(stripped) = reference.strip_prefix("op://") {
            // Split: vault/item/field
            let parts: Vec<&str> = stripped.splitn(3, '/').collect();
            if parts.len() >= 3 {
                return format!("op://{}/{}/*****", parts[0], parts[1]);
            }
        }
        "[invalid reference]".to_string()
    }
}

impl Default for SecretManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SecretManager {
    fn drop(&mut self) {
        self.clear_cache();
    }
}

/// Inject secrets into environment variables.
///
/// # Safety
/// This function modifies environment variables which can cause undefined behavior
/// if called in a multi-threaded context. Ensure it's only called during single-threaded
/// initialization.
pub async fn inject_env(
    manager: &mut SecretManager,
    mappings: &[(&str, &str)],
) -> Result<(), SecretsError> {
    for (env_var, reference) in mappings {
        let secret = manager.get(reference).await?;
        // SAFETY: This should only be called during single-threaded initialization
        unsafe {
            std::env::set_var(env_var, &secret);
        }
        debug!("Injected secret into {}", env_var);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reference_validation() {
        let manager = SecretManager::new();
        assert!(SecretManager::redact_reference("op://vault/item/field").contains("*****"));
    }

    #[test]
    fn test_redact_reference() {
        assert_eq!(
            SecretManager::redact_reference("op://Private/API Key/credential"),
            "op://Private/API Key/*****"
        );
    }

    #[tokio::test]
    async fn test_invalid_reference() {
        let mut manager = SecretManager::new();
        let result = manager.get("invalid-reference").await;
        assert!(matches!(result, Err(SecretsError::InvalidReference(_))));
    }
}
