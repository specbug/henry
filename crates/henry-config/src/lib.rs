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
    /// Reactive control loop settings.
    pub reactive: ReactiveConfig,
    /// Agent task execution settings.
    pub agent: AgentConfig,
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
            reactive: ReactiveConfig::default(),
            agent: AgentConfig::default(),
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

/// Reactive control loop configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReactiveConfig {
    /// Whether the reactive engine is enabled.
    pub enabled: bool,
    /// Check interval in seconds.
    pub check_interval_secs: u64,
    /// Maximum actions per hour.
    pub max_actions_per_hour: u32,
    /// Maximum concurrent actions.
    pub max_concurrent_actions: usize,
    /// Threshold settings.
    pub thresholds: ReactiveThresholds,
    /// Remediation rules.
    pub rules: Vec<ReactiveRule>,
    /// Notification settings.
    pub notifications: ReactiveNotifications,
    /// Quiet hours (start hour, end hour) in 24h format.
    pub quiet_hours_start: u8,
    pub quiet_hours_end: u8,
}

impl Default for ReactiveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_interval_secs: 10,
            max_actions_per_hour: 20,
            max_concurrent_actions: 3,
            thresholds: ReactiveThresholds::default(),
            rules: vec![],
            notifications: ReactiveNotifications::default(),
            quiet_hours_start: 2,
            quiet_hours_end: 6,
        }
    }
}

/// Threshold configuration for anomaly detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReactiveThresholds {
    /// Disk usage warning threshold (percent).
    pub disk_warning_percent: f32,
    /// Disk usage critical threshold (percent).
    pub disk_critical_percent: f32,
    /// Memory usage warning threshold (percent).
    pub memory_warning_percent: f32,
    /// Memory usage critical threshold (percent).
    pub memory_critical_percent: f32,
    /// CPU usage warning threshold (percent).
    pub cpu_warning_percent: f32,
    /// Duration of high CPU to trigger alert (seconds).
    pub cpu_alert_duration_secs: u64,
}

impl Default for ReactiveThresholds {
    fn default() -> Self {
        Self {
            disk_warning_percent: 80.0,
            disk_critical_percent: 95.0,
            memory_warning_percent: 85.0,
            memory_critical_percent: 95.0,
            cpu_warning_percent: 90.0,
            cpu_alert_duration_secs: 300,
        }
    }
}

/// A remediation rule configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactiveRule {
    /// Unique name for this rule.
    pub name: String,
    /// Event pattern to match (e.g., "container_crashed").
    pub event_pattern: String,
    /// Action to take (e.g., "restart_container").
    pub action: String,
    /// Whether this action requires human approval.
    #[serde(default)]
    pub requires_approval: bool,
    /// Cooldown period after triggering (seconds).
    #[serde(default = "default_cooldown_secs")]
    pub cooldown_secs: u64,
    /// Maximum times this rule can trigger per day.
    #[serde(default = "default_max_triggers")]
    pub max_triggers_per_day: u32,
    /// Optional pre-action to run before the main action.
    pub pre_action: Option<String>,
}

fn default_cooldown_secs() -> u64 {
    300
}

fn default_max_triggers() -> u32 {
    10
}

/// Notification settings for the reactive engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReactiveNotifications {
    /// Verbosity level: "all", "milestones", "critical".
    pub verbosity: String,
    /// Include routine fixes in notifications.
    pub include_routine_fixes: bool,
    /// Batch interval for notifications (0 = immediate).
    pub batch_interval_secs: u64,
}

impl Default for ReactiveNotifications {
    fn default() -> Self {
        Self {
            verbosity: "all".to_string(),
            include_routine_fixes: true,
            batch_interval_secs: 0,
        }
    }
}

/// Agent task execution configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    /// Whether the agent is enabled.
    pub enabled: bool,
    /// Maximum concurrent tasks.
    pub max_concurrent_tasks: usize,
    /// Maximum steps per task.
    pub max_steps_per_task: usize,
    /// Maximum tokens per task.
    pub max_tokens_per_task: u32,
    /// Maximum iterations per step.
    pub max_iterations_per_step: u32,
    /// Default step timeout in seconds.
    pub step_timeout_secs: u64,
    /// Model to use for planning.
    pub planning_model: String,
    /// Safety configuration.
    pub safety: AgentSafetyConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_concurrent_tasks: 1,
            max_steps_per_task: 10,
            max_tokens_per_task: 50000,
            max_iterations_per_step: 5,
            step_timeout_secs: 300,
            planning_model: "claude-sonnet-4-20250514".to_string(),
            safety: AgentSafetyConfig::default(),
        }
    }
}

/// Safety configuration for the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSafetyConfig {
    /// Commands allowed to be auto-executed.
    pub command_allowlist: Vec<String>,
    /// Commands that are never allowed.
    pub command_blocklist: Vec<String>,
    /// Actions that require human approval.
    pub require_approval_for: Vec<String>,
}

impl Default for AgentSafetyConfig {
    fn default() -> Self {
        Self {
            command_allowlist: vec![
                "ls".to_string(),
                "cat".to_string(),
                "git".to_string(),
                "npm".to_string(),
                "cargo".to_string(),
            ],
            command_blocklist: vec![
                "rm -rf".to_string(),
                "sudo".to_string(),
                "chmod 777".to_string(),
            ],
            require_approval_for: vec![
                "file_deletion".to_string(),
                "system_changes".to_string(),
            ],
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
