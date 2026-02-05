//! Main daemon implementation.

use henry_claude::ClaudeManager;
use henry_config::Config;
use henry_health::{HealthManager, HealthStatus, ModuleHealth};
use henry_lock::InstanceLock;
use henry_maint::MaintenanceManager;
use henry_media::MediaManager;
use henry_agent::AgentEngine;
use henry_reactive::{ReactiveConfig, ReactiveEngine};
use henry_secrets::SecretManager;
use henry_server::ContainerManager;
use henry_state::StateManager;
use henry_tui::{ui, App, Event, EventHandler, Terminal};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::{error, info, warn};

use crate::signals::SignalHandler;

#[derive(Error, Debug)]
pub enum DaemonError {
    #[error("configuration error: {0}")]
    Config(#[from] henry_config::ConfigError),

    #[error("lock error: {0}")]
    Lock(#[from] henry_lock::LockError),

    #[error("state error: {0}")]
    State(#[from] henry_state::StateError),

    #[error("TUI error: {0}")]
    Tui(#[from] henry_tui::TuiError),

    #[error("secrets error: {0}")]
    Secrets(#[from] henry_secrets::SecretsError),

    #[error("telegram error: {0}")]
    Telegram(#[from] henry_telegram::TelegramError),

    #[error("http error: {0}")]
    Http(#[from] henry_http::HttpError),

    #[error("container error: {0}")]
    Container(#[from] henry_server::ContainerError),

    #[error("media error: {0}")]
    Media(#[from] henry_media::MediaError),

    #[error("claude error: {0}")]
    Claude(#[from] henry_claude::ClaudeError),

    #[error("maintenance error: {0}")]
    Maint(#[from] henry_maint::MaintError),

    #[error("reactive error: {0}")]
    Reactive(#[from] henry_reactive::ReactiveError),

    #[error("daemon is shutting down")]
    Shutdown,
}

/// Main daemon structure.
pub struct Daemon {
    config: Config,
    config_path: PathBuf,
    _lock: InstanceLock,
    state: Arc<RwLock<StateManager>>,
    health: Arc<RwLock<HealthManager>>,
    secrets: SecretManager,
    containers: Option<Arc<RwLock<ContainerManager>>>,
    media: Option<Arc<RwLock<MediaManager>>>,
    claude: Option<Arc<RwLock<ClaudeManager>>>,
    maint: Option<Arc<RwLock<MaintenanceManager>>>,
    reactive: Option<Arc<RwLock<ReactiveEngine>>>,
    agent: Option<Arc<RwLock<AgentEngine>>>,
    shutdown_tx: broadcast::Sender<()>,
}

impl Daemon {
    /// Create a new daemon instance.
    pub async fn new(config_path: Option<PathBuf>) -> Result<Self, DaemonError> {
        // Load configuration
        let config_path = config_path.unwrap_or_else(|| {
            Config::default_paths()
                .into_iter()
                .find(|p| p.exists())
                .unwrap_or_else(|| {
                    dirs::home_dir()
                        .unwrap_or_else(|| PathBuf::from("."))
                        .join(".henry")
                        .join("config.toml")
                })
        });

        let config = if config_path.exists() {
            Config::load(&config_path)?
        } else {
            info!("No config file found, using defaults");
            Config::default()
        };

        // Acquire instance lock
        let lock = InstanceLock::acquire(config.daemon.lock_port, &config.daemon.pid_file)?;
        info!("Instance lock acquired");

        // Initialize state database
        let db_path = config.daemon.data_dir.join("state").join("henry.db");
        let state = Arc::new(RwLock::new(StateManager::new(&db_path).await?));

        // Initialize health manager
        let health = Arc::new(RwLock::new(HealthManager::new(env!("CARGO_PKG_VERSION"))));

        // Initialize secrets manager
        let mut secrets = SecretManager::new();

        // Create shutdown broadcast channel
        let (shutdown_tx, _) = broadcast::channel(1);

        // Initialize container manager if enabled
        let containers = if config.containers.enabled {
            match ContainerManager::new(config.containers.clone()).await {
                Ok(manager) => {
                    info!("Container manager initialized");
                    Some(Arc::new(RwLock::new(manager)))
                }
                Err(e) => {
                    warn!("Failed to initialize container manager: {}", e);
                    None
                }
            }
        } else {
            None
        };

        // Initialize media manager if enabled
        let media = if config.media.enabled {
            let api_key = if let Some(ref key_ref) = config.media.jellyfin_api_key_ref {
                match secrets.get(key_ref).await {
                    Ok(key) => Some(key),
                    Err(e) => {
                        warn!("Failed to get Jellyfin API key: {}", e);
                        None
                    }
                }
            } else {
                None
            };

            let manager = MediaManager::new(
                config.media.clone(),
                containers.clone(),
                api_key,
            );
            info!("Media manager initialized");
            Some(Arc::new(RwLock::new(manager)))
        } else {
            None
        };

        // Initialize Claude manager if enabled
        let claude = if config.claude.enabled {
            let api_key = match secrets.get(&config.claude.api_key_ref).await {
                Ok(key) => Some(key),
                Err(e) => {
                    warn!("Failed to get Anthropic API key: {}", e);
                    None
                }
            };

            let manager = ClaudeManager::new(config.claude.clone(), api_key);
            info!("Claude manager initialized");
            Some(Arc::new(RwLock::new(manager)))
        } else {
            None
        };

        // Initialize maintenance manager if enabled
        let maint = if config.maint.enabled {
            let manager = MaintenanceManager::new(config.maint.clone());
            info!("Maintenance manager initialized");
            Some(Arc::new(RwLock::new(manager)))
        } else {
            None
        };

        // Initialize reactive engine if enabled
        let reactive = if config.reactive.enabled {
            let reactive_config = ReactiveConfig {
                enabled: config.reactive.enabled,
                check_interval_secs: config.reactive.check_interval_secs,
                max_actions_per_hour: config.reactive.max_actions_per_hour,
                max_concurrent_actions: config.reactive.max_concurrent_actions,
                thresholds: henry_reactive::ThresholdsConfig {
                    disk_warning_percent: config.reactive.thresholds.disk_warning_percent,
                    disk_critical_percent: config.reactive.thresholds.disk_critical_percent,
                    memory_warning_percent: config.reactive.thresholds.memory_warning_percent,
                    memory_critical_percent: config.reactive.thresholds.memory_critical_percent,
                    cpu_warning_percent: config.reactive.thresholds.cpu_warning_percent,
                    cpu_alert_duration_secs: config.reactive.thresholds.cpu_alert_duration_secs,
                },
                rules: config.reactive.rules.iter().map(|r| henry_reactive::policy::RemediationRule {
                    name: r.name.clone(),
                    event_pattern: r.event_pattern.clone(),
                    action: r.action.clone(),
                    min_severity: henry_reactive::Severity::Warning,
                    requires_approval: r.requires_approval,
                    cooldown_secs: r.cooldown_secs,
                    max_triggers_per_day: r.max_triggers_per_day,
                    pre_action: r.pre_action.clone(),
                    enabled: true,
                }).collect(),
                notifications: henry_reactive::NotificationsConfig {
                    verbosity: config.reactive.notifications.verbosity.clone(),
                    include_routine_fixes: config.reactive.notifications.include_routine_fixes,
                    batch_interval_secs: config.reactive.notifications.batch_interval_secs,
                },
                quiet_hours: Some((config.reactive.quiet_hours_start, config.reactive.quiet_hours_end)),
            };

            let engine = ReactiveEngine::new(
                reactive_config,
                health.clone(),
                containers.clone(),
                maint.clone(),
            );
            info!("Reactive engine initialized");
            Some(Arc::new(RwLock::new(engine)))
        } else {
            None
        };

        // Initialize agent engine if enabled
        let agent = if config.agent.enabled {
            if let (Some(ref claude_mgr), Some(ref container_mgr)) = (&claude, &containers) {
                let engine = AgentEngine::new(
                    config.agent.clone(),
                    state.clone(),
                    claude_mgr.clone(),
                    container_mgr.clone(),
                    shutdown_tx.subscribe(),
                );
                info!("Agent engine initialized");
                Some(Arc::new(RwLock::new(engine)))
            } else {
                warn!("Agent engine requires Claude and containers to be enabled");
                None
            }
        } else {
            None
        };

        Ok(Self {
            config,
            config_path,
            _lock: lock,
            state,
            health,
            secrets,
            containers,
            media,
            claude,
            maint,
            reactive,
            agent,
            shutdown_tx,
        })
    }

    /// Run the daemon with TUI.
    pub async fn run_with_tui(self) -> Result<(), DaemonError> {
        // Initialize terminal
        let mut terminal = Terminal::new()?;

        // Extract health manager for TUI (TUI mode doesn't need shared access)
        let health = Arc::try_unwrap(self.health)
            .map(|rw| rw.into_inner())
            .unwrap_or_else(|arc| {
                // This shouldn't happen since we haven't shared it yet
                let guard = arc.blocking_read();
                HealthManager::new(guard.version())
            });

        // Create app state with health manager
        let mut app = App::new(env!("CARGO_PKG_VERSION")).with_health_manager(health);

        // Register modules
        Self::register_modules(&self.config, &mut app).await;

        // Create event handler with 250ms tick rate
        let mut events = EventHandler::new(Duration::from_millis(250));

        // Set up signal handler
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        let (reload_tx, mut reload_rx) = mpsc::channel::<()>(1);

        #[cfg(unix)]
        {
            let signal_handler = SignalHandler::new(shutdown_tx.clone(), reload_tx);
            tokio::spawn(async move {
                signal_handler.run().await;
            });
        }

        // Initial metrics refresh
        app.refresh_metrics();
        app.log_info("Henry daemon started");

        // Main loop
        loop {
            // Draw UI
            terminal
                .inner_mut()
                .draw(|frame| ui::render(frame, &app))
                .map_err(henry_tui::TuiError::from)?;

            // Handle events with select
            tokio::select! {
                Some(event) = events.next() => {
                    match event {
                        Event::Tick => {
                            app.refresh_metrics();
                        }
                        Event::Key(key) => {
                            app.handle_key(key);
                        }
                        Event::Resize(_, _) => {
                            // Terminal will auto-redraw
                        }
                        Event::Quit => {
                            app.should_quit = true;
                        }
                    }
                }
                Some(_) = shutdown_rx.recv() => {
                    info!("Shutdown signal received");
                    app.log_warn("Shutdown signal received");
                    app.should_quit = true;
                }
                Some(_) = reload_rx.recv() => {
                    info!("Reload signal received");
                    app.log_info("Configuration reload triggered");
                    // Reload would happen here
                }
            }

            if app.should_quit {
                break;
            }
        }

        // Cleanup
        terminal.restore()?;
        info!("Henry daemon stopped");
        Ok(())
    }

    /// Run the daemon in headless mode (no TUI).
    pub async fn run_headless(mut self) -> Result<(), DaemonError> {
        info!("Starting Henry daemon in headless mode");

        // Set up signal handler
        let (signal_shutdown_tx, mut signal_shutdown_rx) = mpsc::channel::<()>(1);
        let (reload_tx, mut reload_rx) = mpsc::channel::<()>(1);

        #[cfg(unix)]
        {
            let signal_handler = SignalHandler::new(signal_shutdown_tx, reload_tx);
            tokio::spawn(async move {
                signal_handler.run().await;
            });
        }

        // Register modules
        self.register_modules_headless().await;

        // Start communication services
        self.start_services().await?;

        // Main loop
        let mut interval = tokio::time::interval(Duration::from_secs(10));

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // Periodic health check
                    let mut health = self.health.write().await;
                    let snapshot = health.get_snapshot().await;
                    if snapshot.overall_status != HealthStatus::Healthy {
                        warn!("System health: {:?}", snapshot.overall_status);
                    }
                }
                Some(_) = signal_shutdown_rx.recv() => {
                    info!("Shutdown signal received");
                    // Broadcast shutdown to all services
                    let _ = self.shutdown_tx.send(());
                    break;
                }
                Some(_) = reload_rx.recv() => {
                    info!("Reload signal received, reloading configuration");
                    match Config::load(&self.config_path) {
                        Ok(new_config) => {
                            self.config = new_config;
                            info!("Configuration reloaded successfully");
                        }
                        Err(e) => {
                            error!("Failed to reload configuration: {}", e);
                        }
                    }
                }
            }
        }

        info!("Henry daemon stopped");
        Ok(())
    }

    /// Start communication services (Telegram, HTTP).
    async fn start_services(&mut self) -> Result<(), DaemonError> {
        // Update container module health
        if self.config.containers.enabled {
            if self.containers.is_some() {
                self.health
                    .write()
                    .await
                    .update_module(ModuleHealth::new("server").healthy())
                    .await;
            } else {
                self.health
                    .write()
                    .await
                    .update_module(
                        ModuleHealth::new("server").unhealthy("No container runtime found"),
                    )
                    .await;
            }
        }

        // Update media module health
        if self.config.media.enabled {
            if let Some(ref media) = self.media {
                let manager = media.read().await;
                let status = manager.status().await;
                if status.api_reachable {
                    self.health
                        .write()
                        .await
                        .update_module(ModuleHealth::new("media").healthy())
                        .await;
                } else if status.container_running {
                    self.health
                        .write()
                        .await
                        .update_module(
                            ModuleHealth::new("media")
                                .unhealthy("Jellyfin API not reachable"),
                        )
                        .await;
                } else {
                    self.health
                        .write()
                        .await
                        .update_module(
                            ModuleHealth::new("media")
                                .unhealthy("Jellyfin container not running"),
                        )
                        .await;
                }
            } else {
                self.health
                    .write()
                    .await
                    .update_module(
                        ModuleHealth::new("media").unhealthy("Media manager not initialized"),
                    )
                    .await;
            }
        }

        // Update Claude module health
        if self.config.claude.enabled {
            if let Some(ref claude) = self.claude {
                let manager = claude.read().await;
                if manager.has_api_client() {
                    self.health
                        .write()
                        .await
                        .update_module(ModuleHealth::new("claude").healthy())
                        .await;
                } else {
                    self.health
                        .write()
                        .await
                        .update_module(
                            ModuleHealth::new("claude")
                                .unhealthy("API key not configured"),
                        )
                        .await;
                }
            } else {
                self.health
                    .write()
                    .await
                    .update_module(
                        ModuleHealth::new("claude").unhealthy("Claude manager not initialized"),
                    )
                    .await;
            }
        }

        // Start maintenance scheduler if enabled
        if self.config.maint.enabled {
            if let Some(ref maint) = self.maint {
                let mut manager = maint.write().await;
                match manager.start_scheduler().await {
                    Ok(()) => {
                        self.health
                            .write()
                            .await
                            .update_module(ModuleHealth::new("maint").healthy())
                            .await;
                        info!("Maintenance scheduler started");
                    }
                    Err(e) => {
                        warn!("Failed to start maintenance scheduler: {}", e);
                        self.health
                            .write()
                            .await
                            .update_module(
                                ModuleHealth::new("maint")
                                    .unhealthy(format!("Scheduler error: {}", e)),
                            )
                            .await;
                    }
                }
            } else {
                self.health
                    .write()
                    .await
                    .update_module(
                        ModuleHealth::new("maint").unhealthy("Maintenance manager not initialized"),
                    )
                    .await;
            }
        }

        // Start HTTP server if enabled
        if self.config.http.enabled {
            let http_config = self.config.http.clone();
            let bind_addr = self.config.network.bind_address.clone();
            let health = self.health.clone();
            let containers = self.containers.clone();
            let media = self.media.clone();
            let claude = self.claude.clone();
            let maint = self.maint.clone();
            let reactive = self.reactive.clone();
            let agent_http = self.agent.clone();
            let shutdown_rx = self.shutdown_tx.subscribe();

            // Resolve API token if configured
            let api_token = if let Some(ref token_ref) = http_config.token_ref {
                match self.secrets.get(token_ref).await {
                    Ok(token) => Some(token),
                    Err(e) => {
                        warn!("Failed to get HTTP API token: {}", e);
                        None
                    }
                }
            } else {
                None
            };

            tokio::spawn(async move {
                if let Err(e) = henry_http::run_server(
                    http_config,
                    &bind_addr,
                    health,
                    api_token,
                    containers,
                    media,
                    claude,
                    maint,
                    reactive,
                    agent_http,
                    shutdown_rx,
                )
                .await
                {
                    error!("HTTP server error: {}", e);
                }
            });

            self.health
                .write()
                .await
                .update_module(ModuleHealth::new("http").healthy())
                .await;
            info!("HTTP server started on port {}", self.config.http.port);
        }

        // Start Telegram bot if enabled
        if self.config.telegram.enabled {
            let telegram_config = self.config.telegram.clone();
            let health = self.health.clone();
            let containers = self.containers.clone();
            let media = self.media.clone();
            let claude = self.claude.clone();
            let maint = self.maint.clone();
            let reactive = self.reactive.clone();
            let agent = self.agent.clone();
            let shutdown_rx = self.shutdown_tx.subscribe();

            // Get bot token from 1Password
            let token = match self.secrets.get(&telegram_config.token_ref).await {
                Ok(token) => token,
                Err(e) => {
                    error!("Failed to get Telegram bot token: {}", e);
                    self.health
                        .write()
                        .await
                        .update_module(
                            ModuleHealth::new("telegram")
                                .unhealthy(format!("Failed to get token: {}", e)),
                        )
                        .await;
                    return Ok(());
                }
            };

            tokio::spawn(async move {
                if let Err(e) =
                    henry_telegram::run_bot(token, telegram_config, health, containers, media, claude, maint, reactive, agent, shutdown_rx)
                        .await
                {
                    error!("Telegram bot error: {}", e);
                }
            });

            self.health
                .write()
                .await
                .update_module(ModuleHealth::new("telegram").healthy())
                .await;
            info!("Telegram bot started");
        }

        // Start reactive engine if enabled
        if self.config.reactive.enabled {
            if let Some(ref reactive) = self.reactive {
                let shutdown_rx = self.shutdown_tx.subscribe();
                let mut engine = reactive.write().await;
                match engine.start(shutdown_rx).await {
                    Ok(()) => {
                        self.health
                            .write()
                            .await
                            .update_module(ModuleHealth::new("reactive").healthy())
                            .await;
                        info!("Reactive engine started");
                    }
                    Err(e) => {
                        warn!("Failed to start reactive engine: {}", e);
                        self.health
                            .write()
                            .await
                            .update_module(
                                ModuleHealth::new("reactive")
                                    .unhealthy(format!("Failed to start: {}", e)),
                            )
                            .await;
                    }
                }
            }
        }

        // Start agent engine if enabled
        if self.config.agent.enabled {
            if let Some(ref agent) = self.agent {
                let agent_clone = agent.clone();
                tokio::spawn(async move {
                    let mut engine = agent_clone.write().await;
                    if let Err(e) = engine.start().await {
                        error!("Agent engine error: {}", e);
                    }
                });

                self.health
                    .write()
                    .await
                    .update_module(ModuleHealth::new("agent").healthy())
                    .await;
                info!("Agent engine started");
            } else {
                self.health
                    .write()
                    .await
                    .update_module(
                        ModuleHealth::new("agent").unhealthy("Agent engine not initialized"),
                    )
                    .await;
            }
        }

        Ok(())
    }

    async fn register_modules(config: &Config, app: &mut App) {
        // Register core modules
        let modules = [
            ("server", config.containers.enabled),
            ("media", config.media.enabled),
            ("claude", config.claude.enabled),
            ("maint", config.maint.enabled),
            ("telegram", config.telegram.enabled),
            ("http", config.http.enabled),
            ("reactive", config.reactive.enabled),
            ("agent", config.agent.enabled),
        ];

        for (name, enabled) in modules {
            app.modules.push(if enabled {
                ModuleHealth::new(name).healthy()
            } else {
                ModuleHealth::new(name).disabled()
            });
        }

        app.log_info(format!(
            "Registered {} modules",
            modules.iter().filter(|(_, e)| *e).count()
        ));
    }

    async fn register_modules_headless(&mut self) {
        let modules = [
            ("server", self.config.containers.enabled),
            ("media", self.config.media.enabled),
            ("claude", self.config.claude.enabled),
            ("maint", self.config.maint.enabled),
            ("telegram", self.config.telegram.enabled),
            ("http", self.config.http.enabled),
            ("reactive", self.config.reactive.enabled),
            ("agent", self.config.agent.enabled),
        ];

        let health = self.health.read().await;
        for (name, enabled) in modules {
            health.register_module(name, enabled).await;
            if enabled {
                health
                    .update_module(ModuleHealth::new(name).healthy())
                    .await;
            }
        }

        info!(
            "Registered {} modules",
            modules.iter().filter(|(_, e)| *e).count()
        );
    }

    /// Get the current configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Get the state manager.
    pub fn state(&self) -> &Arc<RwLock<StateManager>> {
        &self.state
    }

    /// Get the health manager.
    pub fn health(&self) -> &Arc<RwLock<HealthManager>> {
        &self.health
    }
}

// Home directory helper
mod dirs {
    use std::path::PathBuf;

    pub fn home_dir() -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}
