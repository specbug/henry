//! Update checking for Henry daemon.
//!
//! Checks for new versions via GitHub releases API or crates.io.

use crate::types::UpdateInfo;
use chrono::Utc;
use serde::Deserialize;
use thiserror::Error;
use tracing::{debug, info, warn};

#[derive(Error, Debug)]
pub enum UpdateError {
    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("failed to parse version: {0}")]
    VersionParse(String),

    #[error("update check disabled")]
    Disabled,
}

/// Update checker configuration.
#[derive(Debug, Clone)]
pub struct UpdateCheckerConfig {
    /// GitHub repository (owner/repo format).
    pub github_repo: Option<String>,
    /// Current version.
    pub current_version: String,
}

impl Default for UpdateCheckerConfig {
    fn default() -> Self {
        Self {
            github_repo: Some("specbug/henry".to_string()),
            current_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// Update checker for Henry.
pub struct UpdateChecker {
    config: UpdateCheckerConfig,
    client: reqwest::Client,
}

impl UpdateChecker {
    /// Create a new update checker.
    pub fn new(config: UpdateCheckerConfig) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(format!("henry/{}", config.current_version))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self { config, client }
    }

    /// Check for updates via GitHub releases.
    pub async fn check_github(&self) -> Result<UpdateInfo, UpdateError> {
        let repo = self
            .config
            .github_repo
            .as_ref()
            .ok_or(UpdateError::Disabled)?;

        let url = format!(
            "https://api.github.com/repos/{}/releases/latest",
            repo
        );

        debug!("Checking for updates at: {}", url);

        let response = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .await?;

        if !response.status().is_success() {
            // No releases yet or rate limited
            return Ok(UpdateInfo {
                current_version: self.config.current_version.clone(),
                latest_version: self.config.current_version.clone(),
                update_available: false,
                release_url: None,
                checked_at: Utc::now(),
            });
        }

        let release: GitHubRelease = response.json().await?;
        let latest_version = release.tag_name.trim_start_matches('v').to_string();

        let update_available = is_newer_version(&self.config.current_version, &latest_version);

        if update_available {
            info!(
                "Update available: {} -> {}",
                self.config.current_version, latest_version
            );
        } else {
            debug!("Current version {} is up to date", self.config.current_version);
        }

        Ok(UpdateInfo {
            current_version: self.config.current_version.clone(),
            latest_version,
            update_available,
            release_url: Some(release.html_url),
            checked_at: Utc::now(),
        })
    }

    /// Get the current version.
    pub fn current_version(&self) -> &str {
        &self.config.current_version
    }
}

/// GitHub release response.
#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
}

/// Compare two semantic versions.
/// Returns true if `latest` is newer than `current`.
fn is_newer_version(current: &str, latest: &str) -> bool {
    let parse_version = |s: &str| -> Option<(u32, u32, u32)> {
        let s = s.trim_start_matches('v');
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() >= 3 {
            let major = parts[0].parse().ok()?;
            let minor = parts[1].parse().ok()?;
            // Handle prerelease suffixes like "0.1.0-beta"
            let patch_str = parts[2].split('-').next()?;
            let patch = patch_str.parse().ok()?;
            Some((major, minor, patch))
        } else if parts.len() == 2 {
            let major = parts[0].parse().ok()?;
            let minor = parts[1].parse().ok()?;
            Some((major, minor, 0))
        } else {
            None
        }
    };

    match (parse_version(current), parse_version(latest)) {
        (Some((cm, cn, cp)), Some((lm, ln, lp))) => {
            (lm, ln, lp) > (cm, cn, cp)
        }
        _ => {
            warn!(
                "Could not parse versions for comparison: {} vs {}",
                current, latest
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_newer_version() {
        assert!(is_newer_version("0.1.0", "0.1.1"));
        assert!(is_newer_version("0.1.0", "0.2.0"));
        assert!(is_newer_version("0.1.0", "1.0.0"));
        assert!(!is_newer_version("0.1.0", "0.1.0"));
        assert!(!is_newer_version("0.2.0", "0.1.0"));
        assert!(!is_newer_version("1.0.0", "0.9.9"));
    }

    #[test]
    fn test_is_newer_version_with_v_prefix() {
        assert!(is_newer_version("v0.1.0", "v0.1.1"));
        assert!(is_newer_version("0.1.0", "v0.2.0"));
        assert!(is_newer_version("v0.1.0", "0.2.0"));
    }

    #[test]
    fn test_is_newer_version_with_prerelease() {
        assert!(is_newer_version("0.1.0-beta", "0.1.1"));
        assert!(is_newer_version("0.1.0", "0.1.1-beta"));
    }

    #[test]
    fn test_update_checker_config_default() {
        let config = UpdateCheckerConfig::default();
        assert!(config.github_repo.is_some());
        assert!(!config.current_version.is_empty());
    }

    #[test]
    fn test_update_checker_creation() {
        let config = UpdateCheckerConfig::default();
        let checker = UpdateChecker::new(config);
        assert!(!checker.current_version().is_empty());
    }
}
