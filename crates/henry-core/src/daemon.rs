//! Main daemon implementation.

use henry_config::Config;
use henry_health::{HealthManager, HealthStatus, ModuleHealth};
use henry_lock::InstanceLock;
use henry_state::StateManager;
use henry_tui::{ui, App, Event, EventHandler, Terminal};
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;
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

    #[error("daemon is shutting down")]
    Shutdown,
}

/// Main daemon structure.
pub struct Daemon {
    config: Config,
    config_path: PathBuf,
    _lock: InstanceLock,
    state: StateManager,
    health: HealthManager,
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
        let state = StateManager::new(&db_path).await?;

        // Initialize health manager
        let health = HealthManager::new(env!("CARGO_PKG_VERSION"));

        Ok(Self {
            config,
            config_path,
            _lock: lock,
            state,
            health,
        })
    }

    /// Run the daemon with TUI.
    pub async fn run_with_tui(self) -> Result<(), DaemonError> {
        // Initialize terminal
        let mut terminal = Terminal::new()?;

        // Create app state with health manager
        let mut app = App::new(env!("CARGO_PKG_VERSION")).with_health_manager(self.health);

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
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        let (reload_tx, mut reload_rx) = mpsc::channel::<()>(1);

        #[cfg(unix)]
        {
            let signal_handler = SignalHandler::new(shutdown_tx, reload_tx);
            tokio::spawn(async move {
                signal_handler.run().await;
            });
        }

        // Register modules
        self.register_modules_headless().await;

        // Main loop
        let mut interval = tokio::time::interval(Duration::from_secs(10));

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // Periodic health check
                    let snapshot = self.health.get_snapshot().await;
                    if snapshot.overall_status != HealthStatus::Healthy {
                        warn!("System health: {:?}", snapshot.overall_status);
                    }
                }
                Some(_) = shutdown_rx.recv() => {
                    info!("Shutdown signal received");
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

    async fn register_modules(config: &Config, app: &mut App) {
        // Register core modules
        let modules = [
            ("server", config.containers.enabled),
            ("media", config.media.enabled),
            ("claude", config.claude.enabled),
            ("telegram", config.telegram.enabled),
            ("http", config.http.enabled),
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
            ("telegram", self.config.telegram.enabled),
            ("http", self.config.http.enabled),
        ];

        for (name, enabled) in modules {
            self.health.register_module(name, enabled).await;
            if enabled {
                self.health
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
    pub fn state(&self) -> &StateManager {
        &self.state
    }

    /// Get the health manager.
    pub fn health(&self) -> &HealthManager {
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
