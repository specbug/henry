//! Application state for the TUI.

use chrono::{DateTime, Local};
use henry_health::{HealthManager, HealthStatus, ModuleHealth, SystemMetrics};
use std::collections::VecDeque;

/// Maximum activity log entries to keep.
const MAX_LOG_ENTRIES: usize = 100;

/// Application state.
pub struct App {
    /// Whether the app should quit.
    pub should_quit: bool,
    /// Current UI state/tab.
    pub state: AppState,
    /// Health manager reference.
    health: Option<HealthManager>,
    /// Cached system metrics.
    pub system_metrics: Option<SystemMetrics>,
    /// Module health statuses.
    pub modules: Vec<ModuleHealth>,
    /// Activity log entries.
    pub activity_log: VecDeque<LogEntry>,
    /// Command input buffer.
    pub command_input: String,
    /// Whether command input is focused.
    pub command_focused: bool,
    /// Selected module index.
    pub selected_module: usize,
    /// Version string.
    pub version: String,
}

/// UI states/tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppState {
    Dashboard,
    Modules,
    Containers,
    Logs,
    Help,
}

/// Activity log entry.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub timestamp: DateTime<Local>,
    pub message: String,
    pub level: LogLevel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
    Debug,
}

impl App {
    /// Create a new application instance.
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            should_quit: false,
            state: AppState::Dashboard,
            health: None,
            system_metrics: None,
            modules: vec![],
            activity_log: VecDeque::with_capacity(MAX_LOG_ENTRIES),
            command_input: String::new(),
            command_focused: false,
            selected_module: 0,
            version: version.into(),
        }
    }

    /// Set the health manager.
    pub fn with_health_manager(mut self, health: HealthManager) -> Self {
        self.health = Some(health);
        self
    }

    /// Refresh system metrics.
    pub fn refresh_metrics(&mut self) {
        if let Some(ref mut health) = self.health {
            self.system_metrics = Some(health.get_system_metrics());
        }
    }

    /// Refresh module statuses.
    pub async fn refresh_modules(&mut self) {
        if let Some(ref health) = self.health {
            self.modules = health.get_all_modules().await;
        }
    }

    /// Add a log entry.
    pub fn log(&mut self, level: LogLevel, message: impl Into<String>) {
        if self.activity_log.len() >= MAX_LOG_ENTRIES {
            self.activity_log.pop_front();
        }
        self.activity_log.push_back(LogEntry {
            timestamp: Local::now(),
            message: message.into(),
            level,
        });
    }

    /// Add an info log entry.
    pub fn log_info(&mut self, message: impl Into<String>) {
        self.log(LogLevel::Info, message);
    }

    /// Add a warning log entry.
    pub fn log_warn(&mut self, message: impl Into<String>) {
        self.log(LogLevel::Warn, message);
    }

    /// Add an error log entry.
    pub fn log_error(&mut self, message: impl Into<String>) {
        self.log(LogLevel::Error, message);
    }

    /// Handle key input.
    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;

        if self.command_focused {
            match key.code {
                KeyCode::Enter => {
                    let cmd = self.command_input.trim().to_string();
                    if !cmd.is_empty() {
                        self.execute_command(&cmd);
                    }
                    self.command_input.clear();
                    self.command_focused = false;
                }
                KeyCode::Esc => {
                    self.command_input.clear();
                    self.command_focused = false;
                }
                KeyCode::Backspace => {
                    self.command_input.pop();
                }
                KeyCode::Char(c) => {
                    self.command_input.push(c);
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Tab => {
                self.state = match self.state {
                    AppState::Dashboard => AppState::Modules,
                    AppState::Modules => AppState::Containers,
                    AppState::Containers => AppState::Logs,
                    AppState::Logs => AppState::Dashboard,
                    AppState::Help => AppState::Dashboard,
                };
            }
            KeyCode::F(1) => {
                self.state = AppState::Help;
            }
            KeyCode::F(5) => {
                self.refresh_metrics();
                self.log_info("Refreshed metrics");
            }
            KeyCode::Char(':') | KeyCode::Char('/') => {
                self.command_focused = true;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected_module > 0 {
                    self.selected_module -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected_module < self.modules.len().saturating_sub(1) {
                    self.selected_module += 1;
                }
            }
            KeyCode::Char('1') => self.state = AppState::Dashboard,
            KeyCode::Char('2') => self.state = AppState::Modules,
            KeyCode::Char('3') => self.state = AppState::Containers,
            KeyCode::Char('4') => self.state = AppState::Logs,
            _ => {}
        }
    }

    fn execute_command(&mut self, cmd: &str) {
        self.log_info(format!("> {}", cmd));

        match cmd.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["help"] => {
                self.state = AppState::Help;
            }
            ["refresh"] | ["r"] => {
                self.refresh_metrics();
                self.log_info("Metrics refreshed");
            }
            ["modules"] | ["m"] => {
                self.state = AppState::Modules;
            }
            ["containers"] | ["c"] => {
                self.state = AppState::Containers;
            }
            ["logs"] | ["l"] => {
                self.state = AppState::Logs;
            }
            ["quit"] | ["q"] => {
                self.should_quit = true;
            }
            _ => {
                self.log_warn(format!("Unknown command: {}", cmd));
            }
        }
    }

    /// Get module status icon.
    pub fn module_status_icon(status: &HealthStatus, enabled: bool) -> &'static str {
        if !enabled {
            return "○";
        }
        match status {
            HealthStatus::Healthy => "●",
            HealthStatus::Degraded => "◐",
            HealthStatus::Unhealthy => "◉",
            HealthStatus::Unknown => "◌",
        }
    }

    /// Get module status color.
    pub fn module_status_color(status: &HealthStatus, enabled: bool) -> ratatui::style::Color {
        use ratatui::style::Color;
        if !enabled {
            return Color::DarkGray;
        }
        match status {
            HealthStatus::Healthy => Color::Green,
            HealthStatus::Degraded => Color::Yellow,
            HealthStatus::Unhealthy => Color::Red,
            HealthStatus::Unknown => Color::Gray,
        }
    }
}
