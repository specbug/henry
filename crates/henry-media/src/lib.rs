//! Media server management for Henry daemon.
//!
//! Provides Jellyfin media server management including container lifecycle,
//! library scanning, playback monitoring, and hardware transcoding configuration.

use henry_config::MediaConfig;
use henry_server::ContainerManager;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{info, warn};

pub mod jellyfin;
mod types;

pub use jellyfin::{format_ticks, JellyfinClient, JellyfinError};
pub use types::{MediaLibrary, MediaStatus, PlaybackSession, ServerInfo};

#[derive(Error, Debug)]
pub enum MediaError {
    #[error("Jellyfin API error: {0}")]
    Jellyfin(#[from] JellyfinError),

    #[error("container error: {0}")]
    Container(#[from] henry_server::ContainerError),

    #[error("media module not configured")]
    NotConfigured,

    #[error("library path does not exist: {0}")]
    InvalidPath(String),
}

/// Media module manager.
///
/// Manages the Jellyfin container via `ContainerManager` and provides
/// a Jellyfin API client for library scans, playback status, etc.
pub struct MediaManager {
    config: MediaConfig,
    containers: Option<Arc<RwLock<ContainerManager>>>,
    client: Option<JellyfinClient>,
}

impl MediaManager {
    /// Create a new media manager.
    pub fn new(
        config: MediaConfig,
        containers: Option<Arc<RwLock<ContainerManager>>>,
        api_key: Option<String>,
    ) -> Self {
        let client = api_key.map(|key| JellyfinClient::new(&config.jellyfin_url, &key));

        Self {
            config,
            containers,
            client,
        }
    }

    /// Validate configured library paths exist on disk.
    pub fn validate_paths(&self) -> Vec<(String, bool)> {
        self.config
            .library_paths
            .iter()
            .map(|p| {
                let path_str = p.display().to_string();
                let exists = p.exists();
                if !exists {
                    warn!("Media library path does not exist: {}", path_str);
                }
                (path_str, exists)
            })
            .collect()
    }

    /// Check if the Jellyfin container is running.
    pub async fn is_container_running(&self) -> bool {
        let Some(ref containers) = self.containers else {
            return false;
        };

        let manager = containers.read().await;
        match manager.list(false).await {
            Ok(list) => list
                .iter()
                .any(|c| c.name == self.config.jellyfin_container && c.status.is_running()),
            Err(_) => false,
        }
    }

    /// Start the Jellyfin container.
    pub async fn start_container(&self) -> Result<(), MediaError> {
        let Some(ref containers) = self.containers else {
            return Err(MediaError::NotConfigured);
        };

        let manager = containers.read().await;
        manager.start(&self.config.jellyfin_container).await?;
        info!("Started Jellyfin container: {}", self.config.jellyfin_container);
        Ok(())
    }

    /// Stop the Jellyfin container.
    pub async fn stop_container(&self) -> Result<(), MediaError> {
        let Some(ref containers) = self.containers else {
            return Err(MediaError::NotConfigured);
        };

        let manager = containers.read().await;
        manager.stop(&self.config.jellyfin_container).await?;
        info!("Stopped Jellyfin container: {}", self.config.jellyfin_container);
        Ok(())
    }

    /// Restart the Jellyfin container.
    pub async fn restart_container(&self) -> Result<(), MediaError> {
        let Some(ref containers) = self.containers else {
            return Err(MediaError::NotConfigured);
        };

        let manager = containers.read().await;
        manager.restart(&self.config.jellyfin_container).await?;
        info!("Restarted Jellyfin container: {}", self.config.jellyfin_container);
        Ok(())
    }

    /// Get Jellyfin container logs.
    pub async fn container_logs(&self, tail: usize) -> Result<String, MediaError> {
        let Some(ref containers) = self.containers else {
            return Err(MediaError::NotConfigured);
        };

        let manager = containers.read().await;
        let logs = manager.logs(&self.config.jellyfin_container, tail).await?;
        Ok(logs)
    }

    /// Get Jellyfin server information.
    pub async fn server_info(&self) -> Result<ServerInfo, MediaError> {
        let Some(ref client) = self.client else {
            return Err(MediaError::NotConfigured);
        };
        Ok(client.system_info().await?)
    }

    /// Get all media libraries.
    pub async fn libraries(&self) -> Result<Vec<MediaLibrary>, MediaError> {
        let Some(ref client) = self.client else {
            return Err(MediaError::NotConfigured);
        };
        Ok(client.libraries().await?)
    }

    /// Trigger a full library scan.
    pub async fn scan_libraries(&self) -> Result<(), MediaError> {
        let Some(ref client) = self.client else {
            return Err(MediaError::NotConfigured);
        };
        client.scan_all_libraries().await?;
        info!("Library scan triggered");
        Ok(())
    }

    /// Get active playback sessions.
    pub async fn active_sessions(&self) -> Result<Vec<PlaybackSession>, MediaError> {
        let Some(ref client) = self.client else {
            return Err(MediaError::NotConfigured);
        };
        Ok(client.sessions().await?)
    }

    /// Get full media module status.
    pub async fn status(&self) -> MediaStatus {
        let container_running = self.is_container_running().await;

        let api_reachable = if let Some(ref client) = self.client {
            client.ping().await
        } else {
            false
        };

        let server = if api_reachable {
            self.server_info().await.ok()
        } else {
            None
        };

        let library_count = if api_reachable {
            self.libraries().await.map(|l| l.len()).unwrap_or(0)
        } else {
            0
        };

        let active_sessions = if api_reachable {
            self.active_sessions().await.map(|s| s.len()).unwrap_or(0)
        } else {
            0
        };

        let library_paths = self
            .config
            .library_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect();

        MediaStatus {
            container_running,
            api_reachable,
            server,
            library_count,
            active_sessions,
            hw_transcode_enabled: self.config.hw_transcode,
            library_paths,
        }
    }

    /// Get the container name.
    pub fn container_name(&self) -> &str {
        &self.config.jellyfin_container
    }

    /// Get the config.
    pub fn config(&self) -> &MediaConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_media_manager_no_containers() {
        let config = MediaConfig::default();
        let manager = MediaManager::new(config, None, None);
        assert!(manager.client.is_none());
    }

    #[test]
    fn test_media_manager_with_api_key() {
        let config = MediaConfig::default();
        let manager = MediaManager::new(config, None, Some("test-key".to_string()));
        assert!(manager.client.is_some());
    }

    #[test]
    fn test_validate_paths_empty() {
        let config = MediaConfig::default();
        let manager = MediaManager::new(config, None, None);
        let result = manager.validate_paths();
        assert!(result.is_empty());
    }

    #[test]
    fn test_validate_paths_nonexistent() {
        let mut config = MediaConfig::default();
        config.library_paths = vec!["/nonexistent/media/path".into()];
        let manager = MediaManager::new(config, None, None);
        let result = manager.validate_paths();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "/nonexistent/media/path");
        assert!(!result[0].1);
    }

    #[test]
    fn test_container_name() {
        let config = MediaConfig::default();
        let manager = MediaManager::new(config, None, None);
        assert_eq!(manager.container_name(), "jellyfin");
    }

    #[tokio::test]
    async fn test_status_without_services() {
        let config = MediaConfig::default();
        let manager = MediaManager::new(config, None, None);
        let status = manager.status().await;
        assert!(!status.container_running);
        assert!(!status.api_reachable);
        assert!(status.server.is_none());
        assert_eq!(status.library_count, 0);
        assert_eq!(status.active_sessions, 0);
    }
}
