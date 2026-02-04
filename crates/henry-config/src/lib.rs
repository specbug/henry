//! Configuration management for Henry daemon.
//!
//! Supports TOML configuration with hot reload via file watching.

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info, warn};

mod error;
pub use error::ConfigError;

/// Main configuration structure for Henry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// General daemon settings.
    pub daemon: DaemonConfig,
    /// Network binding settings.
    pub network: NetworkConfig,
    /// Telegram bot settings.
    pub telegram: TelegramConfig,
    /// HTTP API settings.
    pub http: HttpConfig,
    /// Container management settings.
    pub containers: ContainerConfig,
    /// Media server settings.
    pub media: MediaConfig,
    /// Claude/AI integration settings.
    pub claude: ClaudeConfig,
    /// Maintenance settings.
    pub maint: MaintConfig,
    /// Logging settings.
    pub logging: LoggingConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            daemon: DaemonConfig::default(),
            network: NetworkConfig::default(),
            telegram: TelegramConfig::default(),
            http: HttpConfig::default(),
            containers: ContainerConfig::default(),
            media: MediaConfig::default(),
            claude: ClaudeConfig::default(),
            maint: MaintConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Data directory for Henry runtime files.
    pub data_dir: PathBuf,
    /// PID file location.
    pub pid_file: PathBuf,
    /// Lock port for single-instance detection.
    pub lock_port: u16,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let data_dir = home.join(".henry");
        Self {
            pid_file: data_dir.join("henry.pid"),
            data_dir,
            lock_port: 18790,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    /// Address to bind services to.
    pub bind_address: String,
    /// Static IP for documentation/healthchecks.
    pub static_ip: Option<String>,
    /// Tailscale settings.
    pub tailscale: TailscaleConfig,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            bind_address: "0.0.0.0".to_string(),
            static_ip: None,
            tailscale: TailscaleConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TailscaleConfig {
    pub enabled: bool,
    pub hostname: String,
    pub serve_https: bool,
}

impl Default for TailscaleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            hostname: "henry".to_string(),
            serve_https: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TelegramConfig {
    pub enabled: bool,
    /// 1Password secret reference for bot token.
    pub token_ref: String,
    /// Allowed user IDs (allowlist).
    pub allowed_users: Vec<i64>,
    /// Allowed usernames (allowlist).
    pub allowed_usernames: Vec<String>,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            token_ref: "op://Private/Henry Telegram Bot/credential".to_string(),
            allowed_users: vec![],
            allowed_usernames: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpConfig {
    pub enabled: bool,
    pub port: u16,
    /// 1Password secret reference for API token.
    pub token_ref: Option<String>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 18791,
            token_ref: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContainerConfig {
    pub enabled: bool,
    /// Use Podman instead of Docker.
    pub use_podman: bool,
    /// Socket path (auto-detected if empty).
    pub socket_path: Option<String>,
    /// Allowlist of containers Henry can manage (empty = all).
    pub allowed_containers: Vec<String>,
}

impl Default for ContainerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            use_podman: true,
            socket_path: None,
            allowed_containers: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaConfig {
    pub enabled: bool,
    /// Media library paths.
    pub library_paths: Vec<PathBuf>,
    /// Jellyfin container name.
    pub jellyfin_container: String,
    /// Jellyfin server URL (e.g., http://localhost:8096).
    pub jellyfin_url: String,
    /// 1Password secret reference for Jellyfin API key.
    pub jellyfin_api_key_ref: Option<String>,
    /// Enable hardware transcoding (VideoToolbox on macOS).
    pub hw_transcode: bool,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            library_paths: vec![],
            jellyfin_container: "jellyfin".to_string(),
            jellyfin_url: "http://localhost:8096".to_string(),
            jellyfin_api_key_ref: None,
            hw_transcode: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeConfig {
    pub enabled: bool,
    /// 1Password secret reference for Anthropic API key.
    pub api_key_ref: String,
    /// Maximum concurrent Claude Code sessions.
    pub max_sessions: usize,
    /// Workspace directory for Claude sessions.
    pub workspace_dir: PathBuf,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Self {
            enabled: false,
            api_key_ref: "op://Private/Anthropic API/credential".to_string(),
            max_sessions: 4,
            workspace_dir: home.join("claude-workspaces"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// Log level (trace, debug, info, warn, error).
    pub level: String,
    /// Log directory.
    pub dir: PathBuf,
    /// Enable JSON logging.
    pub json: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Self {
            level: "info".to_string(),
            dir: home.join(".henry").join("logs"),
            json: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MaintConfig {
    pub enabled: bool,
    /// Enable automated backups.
    pub backup_enabled: bool,
    /// Backup cron schedule (default: 3 AM daily).
    pub backup_cron: String,
    /// Backup directory.
    pub backup_dir: PathBuf,
    /// Maximum number of backups to retain.
    pub max_backups: usize,
    /// Enable log rotation.
    pub log_rotation_enabled: bool,
    /// Log rotation cron schedule (default: midnight daily).
    pub log_rotation_cron: String,
    /// Log directory for rotation.
    pub log_dir: PathBuf,
    /// Maximum number of rotated log files.
    pub max_log_files: usize,
    /// Maximum log file size before rotation (bytes).
    pub max_log_size: u64,
    /// Enable update checking.
    pub update_check_enabled: bool,
    /// Update check cron schedule (default: noon daily).
    pub update_check_cron: String,
    /// GitHub repository for update checks (owner/repo).
    pub github_repo: Option<String>,
    /// Data directory (for backup sources).
    pub data_dir: PathBuf,
}

impl Default for MaintConfig {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let data_dir = home.join(".henry");
        Self {
            enabled: true,
            backup_enabled: true,
            backup_cron: "0 0 3 * * *".to_string(), // 3 AM daily
            backup_dir: data_dir.join("backups"),
            max_backups: 7,
            log_rotation_enabled: true,
            log_rotation_cron: "0 0 0 * * *".to_string(), // Midnight daily
            log_dir: data_dir.join("logs"),
            max_log_files: 10,
            max_log_size: 10 * 1024 * 1024, // 10 MB
            update_check_enabled: true,
            update_check_cron: "0 0 12 * * *".to_string(), // Noon daily
            github_repo: Some("specbug/henry".to_string()),
            data_dir,
        }
    }
}

impl Config {
    /// Load configuration from a TOML file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let config: Config = toml::from_str(&content).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            source: e,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Load configuration from default locations.
    pub fn load_default() -> Result<Self, ConfigError> {
        let paths = Self::default_paths();
        for path in &paths {
            if path.exists() {
                info!("Loading config from {}", path.display());
                return Self::load(path);
            }
        }
        warn!("No config file found, using defaults");
        Ok(Config::default())
    }

    /// Get default configuration file paths in order of precedence.
    pub fn default_paths() -> Vec<PathBuf> {
        let mut paths = vec![];

        // Current directory
        paths.push(PathBuf::from("config.toml"));
        paths.push(PathBuf::from("henry.toml"));

        // Home directory
        if let Some(home) = dirs::home_dir() {
            paths.push(home.join(".henry").join("config.toml"));
            paths.push(home.join(".config").join("henry").join("config.toml"));
        }

        // System config
        paths.push(PathBuf::from("/etc/henry/config.toml"));

        paths
    }

    /// Validate configuration values.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.daemon.lock_port == 0 {
            return Err(ConfigError::Validation("lock_port cannot be 0".to_string()));
        }
        if self.http.enabled && self.http.port == 0 {
            return Err(ConfigError::Validation(
                "http.port cannot be 0 when enabled".to_string(),
            ));
        }
        if self.telegram.enabled && self.telegram.allowed_users.is_empty()
            && self.telegram.allowed_usernames.is_empty()
        {
            return Err(ConfigError::Validation(
                "telegram requires at least one allowed_users or allowed_usernames entry"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Serialize config to TOML string.
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self).map_err(ConfigError::Serialize)
    }
}

/// Shared configuration with hot reload support.
pub struct ConfigManager {
    config: Arc<RwLock<Config>>,
    path: PathBuf,
    _watcher: Option<RecommendedWatcher>,
}

impl ConfigManager {
    /// Create a new config manager with hot reload.
    pub async fn new(path: impl AsRef<Path>) -> Result<(Self, mpsc::Receiver<()>), ConfigError> {
        let path = path.as_ref().to_path_buf();
        let config = if path.exists() {
            Config::load(&path)?
        } else {
            Config::default()
        };

        let config = Arc::new(RwLock::new(config));
        let (tx, rx) = mpsc::channel(1);

        let watcher = Self::setup_watcher(&path, config.clone(), tx)?;

        Ok((
            Self {
                config,
                path,
                _watcher: Some(watcher),
            },
            rx,
        ))
    }

    fn setup_watcher(
        path: &Path,
        config: Arc<RwLock<Config>>,
        tx: mpsc::Sender<()>,
    ) -> Result<RecommendedWatcher, ConfigError> {
        let path_for_closure = path.to_path_buf();
        let path_for_watch = path.to_path_buf();
        let mut watcher =
            notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                match res {
                    Ok(event) => {
                        if event.kind.is_modify() {
                            debug!("Config file modified, reloading");
                            match Config::load(&path_for_closure) {
                                Ok(new_config) => {
                                    let config = config.clone();
                                    let tx = tx.clone();
                                    tokio::spawn(async move {
                                        *config.write().await = new_config;
                                        let _ = tx.send(()).await;
                                        info!("Configuration reloaded");
                                    });
                                }
                                Err(e) => {
                                    error!("Failed to reload config: {}", e);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Config watcher error: {}", e);
                    }
                }
            })
            .map_err(ConfigError::Watch)?;

        if let Some(parent) = path_for_watch.parent() {
            watcher
                .watch(parent, RecursiveMode::NonRecursive)
                .map_err(ConfigError::Watch)?;
        }

        Ok(watcher)
    }

    /// Get current configuration (read-only).
    pub async fn get(&self) -> Config {
        self.config.read().await.clone()
    }

    /// Get the configuration path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Manually trigger a reload.
    pub async fn reload(&self) -> Result<(), ConfigError> {
        let new_config = Config::load(&self.path)?;
        *self.config.write().await = new_config;
        info!("Configuration manually reloaded");
        Ok(())
    }
}

// Add dirs crate for home directory detection
mod dirs {
    use std::path::PathBuf;

    pub fn home_dir() -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert!(!config.telegram.enabled);
        assert!(config.http.enabled);
        assert_eq!(config.http.port, 18791);
    }

    #[test]
    fn test_config_validation() {
        let mut config = Config::default();
        config.telegram.enabled = true;
        assert!(config.validate().is_err());

        config.telegram.allowed_users = vec![12345];
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_serialization() {
        let config = Config::default();
        let toml = config.to_toml().unwrap();
        assert!(toml.contains("[daemon]"));
        assert!(toml.contains("[network]"));
    }
}
