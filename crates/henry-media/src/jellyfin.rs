//! Jellyfin API client for Henry media module.

use crate::types::{
    JellyfinSession, JellyfinSystemInfo, JellyfinVirtualFolder, MediaLibrary, PlaybackSession,
    ServerInfo,
};
use reqwest::Client;
use std::time::Duration;
use thiserror::Error;
use tracing::debug;

#[derive(Error, Debug)]
pub enum JellyfinError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Jellyfin API returned error: {status} {body}")]
    Api { status: u16, body: String },

    #[error("no API key configured")]
    NoApiKey,
}

/// Client for the Jellyfin REST API.
pub struct JellyfinClient {
    client: Client,
    base_url: String,
    api_key: String,
}

impl JellyfinClient {
    /// Create a new Jellyfin API client.
    pub fn new(base_url: &str, api_key: &str) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");

        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
        }
    }

    /// Check if the Jellyfin server is reachable.
    pub async fn ping(&self) -> bool {
        let url = format!("{}/System/Ping", self.base_url);
        match self.client.get(&url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }

    /// Get server system information.
    pub async fn system_info(&self) -> Result<ServerInfo, JellyfinError> {
        let url = format!("{}/System/Info", self.base_url);
        debug!("Fetching Jellyfin system info from {}", url);

        let resp = self
            .client
            .get(&url)
            .header("X-Emby-Token", &self.api_key)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(JellyfinError::Api { status, body });
        }

        let info: JellyfinSystemInfo = resp.json().await?;

        let hw_available = info
            .transcoding_info
            .as_ref()
            .and_then(|t| t.hardware_acceleration_type.as_deref())
            .is_some_and(|t| !t.is_empty() && t != "none");

        Ok(ServerInfo {
            name: info.server_name.unwrap_or_default(),
            version: info.version.unwrap_or_default(),
            id: info.id.unwrap_or_default(),
            os: info.operating_system.unwrap_or_default(),
            hw_transcode_available: hw_available,
        })
    }

    /// Get all media libraries (virtual folders).
    pub async fn libraries(&self) -> Result<Vec<MediaLibrary>, JellyfinError> {
        let url = format!("{}/Library/VirtualFolders", self.base_url);
        debug!("Fetching Jellyfin libraries from {}", url);

        let resp = self
            .client
            .get(&url)
            .header("X-Emby-Token", &self.api_key)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(JellyfinError::Api { status, body });
        }

        let folders: Vec<JellyfinVirtualFolder> = resp.json().await?;

        Ok(folders
            .into_iter()
            .map(|f| MediaLibrary {
                name: f.name.unwrap_or_default(),
                content_type: f.collection_type.unwrap_or_else(|| "unknown".to_string()),
                paths: f.locations,
                item_count: None,
            })
            .collect())
    }

    /// Trigger a full library scan.
    pub async fn scan_all_libraries(&self) -> Result<(), JellyfinError> {
        let url = format!("{}/Library/Refresh", self.base_url);
        debug!("Triggering Jellyfin library scan");

        let resp = self
            .client
            .post(&url)
            .header("X-Emby-Token", &self.api_key)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(JellyfinError::Api { status, body });
        }

        Ok(())
    }

    /// Get active playback sessions.
    pub async fn sessions(&self) -> Result<Vec<PlaybackSession>, JellyfinError> {
        let url = format!("{}/Sessions", self.base_url);
        debug!("Fetching Jellyfin sessions from {}", url);

        let resp = self
            .client
            .get(&url)
            .header("X-Emby-Token", &self.api_key)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(JellyfinError::Api { status, body });
        }

        let sessions: Vec<JellyfinSession> = resp.json().await?;

        // Only return sessions that are actively playing something
        Ok(sessions
            .into_iter()
            .filter_map(|s| {
                let item = s.now_playing_item?;
                let play_state = s.play_state.as_ref();

                let item_name = if let Some(ref series) = item.series_name {
                    format!("{} - {}", series, item.name.as_deref().unwrap_or("?"))
                } else {
                    item.name.unwrap_or_default()
                };

                let transcode_info = s.transcoding_info.as_ref().map(|t| {
                    if t.is_video_direct_stream {
                        "Direct Play".to_string()
                    } else {
                        format!(
                            "Transcoding ({}->{})",
                            t.video_codec.as_deref().unwrap_or("?"),
                            t.audio_codec.as_deref().unwrap_or("?")
                        )
                    }
                });

                Some(PlaybackSession {
                    user: s.user_name.unwrap_or_default(),
                    client: s.client.unwrap_or_default(),
                    item_name,
                    item_type: item.item_type.unwrap_or_default(),
                    is_paused: play_state.is_some_and(|p| p.is_paused),
                    position_ticks: play_state.and_then(|p| p.position_ticks),
                    duration_ticks: item.run_time_ticks,
                    transcode_info,
                })
            })
            .collect())
    }
}

/// Format ticks (10M ticks = 1 second) to a human-readable duration.
pub fn format_ticks(ticks: i64) -> String {
    let total_seconds = ticks / 10_000_000;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
    } else {
        format!("{:02}:{:02}", minutes, seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_ticks() {
        assert_eq!(format_ticks(0), "00:00");
        assert_eq!(format_ticks(600_000_000), "01:00");
        assert_eq!(format_ticks(36_000_000_000), "01:00:00");
        assert_eq!(format_ticks(54_615_000_000), "01:31:01");
    }

    #[test]
    fn test_jellyfin_client_url_trimming() {
        let client = JellyfinClient::new("http://localhost:8096/", "test-key");
        assert_eq!(client.base_url, "http://localhost:8096");
    }
}
