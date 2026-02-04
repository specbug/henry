//! Media types for the Henry media module.

use serde::{Deserialize, Serialize};

/// Jellyfin server information.
#[derive(Debug, Clone, Serialize)]
pub struct ServerInfo {
    /// Server name.
    pub name: String,
    /// Server version.
    pub version: String,
    /// Server ID.
    pub id: String,
    /// Operating system.
    pub os: String,
    /// Whether hardware transcoding is available.
    pub hw_transcode_available: bool,
}

/// A media library (virtual folder) in Jellyfin.
#[derive(Debug, Clone, Serialize)]
pub struct MediaLibrary {
    /// Library name.
    pub name: String,
    /// Content type (e.g., "movies", "tvshows", "music").
    pub content_type: String,
    /// Filesystem paths for this library.
    pub paths: Vec<String>,
    /// Item count (if available).
    pub item_count: Option<u64>,
}

/// An active playback session.
#[derive(Debug, Clone, Serialize)]
pub struct PlaybackSession {
    /// User currently playing.
    pub user: String,
    /// Client name (e.g., "Jellyfin Web", "Swiftfin").
    pub client: String,
    /// Item being played.
    pub item_name: String,
    /// Item type (e.g., "Movie", "Episode").
    pub item_type: String,
    /// Whether the session is currently paused.
    pub is_paused: bool,
    /// Playback position in ticks (10M ticks = 1 second).
    pub position_ticks: Option<i64>,
    /// Total duration in ticks.
    pub duration_ticks: Option<i64>,
    /// Transcoding info (if transcoding).
    pub transcode_info: Option<String>,
}

/// Overall media module status.
#[derive(Debug, Clone, Serialize)]
pub struct MediaStatus {
    /// Whether Jellyfin container is running.
    pub container_running: bool,
    /// Whether Jellyfin API is reachable.
    pub api_reachable: bool,
    /// Server info (if available).
    pub server: Option<ServerInfo>,
    /// Number of libraries.
    pub library_count: usize,
    /// Number of active playback sessions.
    pub active_sessions: usize,
    /// Hardware transcoding enabled in config.
    pub hw_transcode_enabled: bool,
    /// Library paths configured.
    pub library_paths: Vec<String>,
}

/// Jellyfin API response for /System/Info.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct JellyfinSystemInfo {
    pub server_name: Option<String>,
    pub version: Option<String>,
    pub id: Option<String>,
    pub operating_system: Option<String>,
    #[serde(default)]
    pub has_pending_restart: bool,
    pub transcoding_info: Option<JellyfinTranscodingInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct JellyfinTranscodingInfo {
    #[serde(default)]
    pub hardware_acceleration_type: Option<String>,
}

/// Jellyfin API response for /Library/VirtualFolders.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct JellyfinVirtualFolder {
    pub name: Option<String>,
    pub collection_type: Option<String>,
    #[serde(default)]
    pub locations: Vec<String>,
    pub library_options: Option<JellyfinLibraryOptions>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct JellyfinLibraryOptions {
    // Placeholder for future use
}

/// Jellyfin API response for /Sessions.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct JellyfinSession {
    pub user_name: Option<String>,
    pub client: Option<String>,
    pub now_playing_item: Option<JellyfinNowPlayingItem>,
    pub play_state: Option<JellyfinPlayState>,
    pub transcoding_info: Option<JellyfinSessionTranscoding>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct JellyfinNowPlayingItem {
    pub name: Option<String>,
    #[serde(rename = "Type")]
    pub item_type: Option<String>,
    pub run_time_ticks: Option<i64>,
    pub series_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct JellyfinPlayState {
    #[serde(default)]
    pub is_paused: bool,
    pub position_ticks: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct JellyfinSessionTranscoding {
    #[serde(default)]
    pub is_video_direct_stream: bool,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub transcode_reasons: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_media_status_defaults() {
        let status = MediaStatus {
            container_running: false,
            api_reachable: false,
            server: None,
            library_count: 0,
            active_sessions: 0,
            hw_transcode_enabled: true,
            library_paths: vec![],
        };
        assert!(!status.container_running);
        assert!(!status.api_reachable);
        assert!(status.server.is_none());
    }

    #[test]
    fn test_playback_session_fields() {
        let session = PlaybackSession {
            user: "test".to_string(),
            client: "Web".to_string(),
            item_name: "Movie".to_string(),
            item_type: "Movie".to_string(),
            is_paused: false,
            position_ticks: Some(600_000_000),
            duration_ticks: Some(72_000_000_000),
            transcode_info: None,
        };
        assert_eq!(session.user, "test");
        assert!(!session.is_paused);
    }

    #[test]
    fn test_jellyfin_system_info_deserialize() {
        let json = r#"{
            "ServerName": "Henry Media",
            "Version": "10.9.0",
            "Id": "abc123",
            "OperatingSystem": "Linux",
            "HasPendingRestart": false
        }"#;
        let info: JellyfinSystemInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.server_name.as_deref(), Some("Henry Media"));
        assert_eq!(info.version.as_deref(), Some("10.9.0"));
    }

    #[test]
    fn test_jellyfin_virtual_folder_deserialize() {
        let json = r#"{
            "Name": "Movies",
            "CollectionType": "movies",
            "Locations": ["/media/movies"],
            "LibraryOptions": {}
        }"#;
        let folder: JellyfinVirtualFolder = serde_json::from_str(json).unwrap();
        assert_eq!(folder.name.as_deref(), Some("Movies"));
        assert_eq!(folder.locations, vec!["/media/movies"]);
    }

    #[test]
    fn test_jellyfin_session_deserialize() {
        let json = r#"{
            "UserName": "admin",
            "Client": "Jellyfin Web",
            "NowPlayingItem": {
                "Name": "Inception",
                "Type": "Movie",
                "RunTimeTicks": 88560000000
            },
            "PlayState": {
                "IsPaused": false,
                "PositionTicks": 12340000000
            }
        }"#;
        let session: JellyfinSession = serde_json::from_str(json).unwrap();
        assert_eq!(session.user_name.as_deref(), Some("admin"));
        assert!(session.now_playing_item.is_some());
        assert!(!session.play_state.unwrap().is_paused);
    }
}
