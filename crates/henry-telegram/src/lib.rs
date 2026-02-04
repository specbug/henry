//! Telegram bot integration for Henry daemon.
//!
//! Provides a Telegram bot interface with allowlist authentication.

use henry_config::TelegramConfig;
use henry_health::{format_bytes, format_duration, HealthManager, HealthStatus};
use henry_server::ContainerManager;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

mod auth;
pub use auth::is_authorized;

#[derive(Error, Debug)]
pub enum TelegramError {
    #[error("bot token not found or invalid")]
    InvalidToken,

    #[error("telegram API error: {0}")]
    Api(#[from] teloxide::RequestError),

    #[error("secrets error: {0}")]
    Secrets(#[from] henry_secrets::SecretsError),

    #[error("configuration error: {0}")]
    Config(String),
}

/// Telegram bot commands.
#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase", description = "Available commands:")]
pub enum Command {
    #[command(description = "display this help text")]
    Help,
    #[command(description = "show system status")]
    Status,
    #[command(description = "show module health")]
    Modules,
    #[command(description = "show system metrics")]
    Metrics,
    #[command(description = "list all containers")]
    Containers,
    #[command(description = "container action: start|stop|restart|logs <name>")]
    Container(String),
    #[command(description = "start the bot")]
    Start,
}

/// Shared state for the bot handlers.
pub struct BotState {
    pub health: Arc<RwLock<HealthManager>>,
    pub config: TelegramConfig,
    pub containers: Option<Arc<RwLock<ContainerManager>>>,
}

/// Create and run the Telegram bot.
pub async fn run_bot(
    token: String,
    config: TelegramConfig,
    health: Arc<RwLock<HealthManager>>,
    containers: Option<Arc<RwLock<ContainerManager>>>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) -> Result<(), TelegramError> {
    if token.is_empty() {
        return Err(TelegramError::InvalidToken);
    }

    let bot = Bot::new(&token);
    let state = Arc::new(BotState {
        health,
        config: config.clone(),
        containers,
    });

    info!("Starting Telegram bot");

    // Set bot commands
    if let Err(e) = bot.set_my_commands(Command::bot_commands()).await {
        warn!("Failed to set bot commands: {}", e);
    }

    // Create handler
    let handler = Update::filter_message()
        .filter_command::<Command>()
        .endpoint(handle_command);

    let mut dispatcher = Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .build();

    // Get shutdown token before dispatching
    let shutdown_token = dispatcher.shutdown_token();

    // Spawn dispatcher
    let dispatch_handle = tokio::spawn(async move {
        dispatcher.dispatch().await;
    });

    // Wait for shutdown signal
    let _ = shutdown.recv().await;
    info!("Telegram bot received shutdown signal");
    shutdown_token
        .shutdown()
        .expect("Failed to shutdown dispatcher")
        .await;

    // Wait for dispatcher to finish
    let _ = dispatch_handle.await;
    info!("Telegram bot stopped");

    Ok(())
}

async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    state: Arc<BotState>,
) -> ResponseResult<()> {
    let user = msg.from.as_ref();

    // Check authorization
    let authorized = if let Some(user) = user {
        is_authorized(
            user.id.0 as i64,
            user.username.as_deref(),
            &state.config.allowed_users,
            &state.config.allowed_usernames,
        )
    } else {
        false
    };

    if !authorized {
        let user_info = user
            .map(|u| format!("{}/@{}", u.id, u.username.as_deref().unwrap_or("unknown")))
            .unwrap_or_else(|| "unknown".to_string());
        warn!("Unauthorized access attempt from {}", user_info);
        bot.send_message(msg.chat.id, "Unauthorized. Access denied.")
            .await?;
        return Ok(());
    }

    debug!("Handling command {:?} from authorized user", cmd);

    let response = match cmd {
        Command::Start => {
            "Henry daemon bot. Use /help to see available commands.".to_string()
        }
        Command::Help => Command::descriptions().to_string(),
        Command::Status => format_status(&state).await,
        Command::Modules => format_modules(&state).await,
        Command::Metrics => format_metrics(&state).await,
        Command::Containers => format_containers(&state).await,
        Command::Container(args) => handle_container_command(&state, &args).await,
    };

    bot.send_message(msg.chat.id, response).await?;
    Ok(())
}

async fn format_status(state: &BotState) -> String {
    let mut health = state.health.write().await;
    let snapshot = health.get_snapshot().await;

    let status_emoji = match snapshot.overall_status {
        HealthStatus::Healthy => "OK",
        HealthStatus::Degraded => "WARN",
        HealthStatus::Unhealthy => "ERR",
        HealthStatus::Unknown => "?",
    };

    format!(
        "Henry Status: {}\n\
         Version: {}\n\
         Uptime: {}\n\
         CPU: {:.1}%\n\
         Memory: {}/{} ({:.1}%)",
        status_emoji,
        snapshot.version,
        format_duration(std::time::Duration::from_secs(snapshot.uptime_secs)),
        snapshot.system.cpu_percent,
        format_bytes(snapshot.system.memory_used),
        format_bytes(snapshot.system.memory_total),
        snapshot.system.memory_percent,
    )
}

async fn format_modules(state: &BotState) -> String {
    let health = state.health.read().await;
    let modules = health.get_all_modules().await;

    if modules.is_empty() {
        return "No modules registered.".to_string();
    }

    let mut lines = vec!["Modules:".to_string()];
    for module in modules {
        let status = if !module.enabled {
            "OFF"
        } else {
            match module.status {
                HealthStatus::Healthy => "OK",
                HealthStatus::Degraded => "WARN",
                HealthStatus::Unhealthy => "ERR",
                HealthStatus::Unknown => "?",
            }
        };
        lines.push(format!("  {} {}", status, module.name));
    }

    lines.join("\n")
}

async fn format_metrics(state: &BotState) -> String {
    let mut health = state.health.write().await;
    let metrics = health.get_system_metrics();

    let mut lines = vec![
        format!("CPU: {:.1}%", metrics.cpu_percent),
        format!(
            "Memory: {}/{} ({:.1}%)",
            format_bytes(metrics.memory_used),
            format_bytes(metrics.memory_total),
            metrics.memory_percent
        ),
        "Disks:".to_string(),
    ];

    for disk in metrics.disks {
        lines.push(format!(
            "  {} {}/{} ({:.1}%)",
            disk.mount_point,
            format_bytes(disk.used),
            format_bytes(disk.total),
            disk.percent
        ));
    }

    lines.join("\n")
}

async fn format_containers(state: &BotState) -> String {
    let Some(ref containers) = state.containers else {
        return "Containers module not enabled.".to_string();
    };

    let manager = containers.read().await;
    match manager.list(true).await {
        Ok(containers) => {
            if containers.is_empty() {
                return "No containers found.".to_string();
            }

            let mut lines = vec!["Containers:".to_string()];
            for c in containers {
                let status = match c.status {
                    henry_server::ContainerStatus::Running => "RUN",
                    henry_server::ContainerStatus::Exited => "EXIT",
                    henry_server::ContainerStatus::Paused => "PAUSE",
                    _ => "?",
                };
                lines.push(format!("  {} {} ({})", status, c.name, c.image));
            }
            lines.join("\n")
        }
        Err(e) => format!("Error listing containers: {}", e),
    }
}

async fn handle_container_command(state: &BotState, args: &str) -> String {
    let Some(ref containers) = state.containers else {
        return "Containers module not enabled.".to_string();
    };

    let parts: Vec<&str> = args.trim().split_whitespace().collect();
    if parts.is_empty() {
        return "Usage: /container <start|stop|restart|logs> <name>".to_string();
    }

    let action = parts[0].to_lowercase();
    let name = parts.get(1).map(|s| *s);

    match action.as_str() {
        "start" => {
            let Some(name) = name else {
                return "Usage: /container start <name>".to_string();
            };
            let manager = containers.read().await;
            match manager.start(name).await {
                Ok(()) => format!("Started container: {}", name),
                Err(e) => format!("Error starting {}: {}", name, e),
            }
        }
        "stop" => {
            let Some(name) = name else {
                return "Usage: /container stop <name>".to_string();
            };
            let manager = containers.read().await;
            match manager.stop(name).await {
                Ok(()) => format!("Stopped container: {}", name),
                Err(e) => format!("Error stopping {}: {}", name, e),
            }
        }
        "restart" => {
            let Some(name) = name else {
                return "Usage: /container restart <name>".to_string();
            };
            let manager = containers.read().await;
            match manager.restart(name).await {
                Ok(()) => format!("Restarted container: {}", name),
                Err(e) => format!("Error restarting {}: {}", name, e),
            }
        }
        "logs" => {
            let Some(name) = name else {
                return "Usage: /container logs <name>".to_string();
            };
            let tail = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(50);
            let manager = containers.read().await;
            match manager.logs(name, tail).await {
                Ok(logs) => {
                    if logs.is_empty() {
                        format!("No logs for container: {}", name)
                    } else {
                        // Truncate if too long for Telegram (max 4096 chars)
                        let max_len = 4000;
                        if logs.len() > max_len {
                            format!("Logs for {} (truncated):\n```\n{}...\n```", name, &logs[..max_len])
                        } else {
                            format!("Logs for {}:\n```\n{}\n```", name, logs)
                        }
                    }
                }
                Err(e) => format!("Error getting logs for {}: {}", name, e),
            }
        }
        _ => "Usage: /container <start|stop|restart|logs> <name>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_descriptions() {
        let desc = Command::descriptions().to_string();
        assert!(desc.contains("help"));
        assert!(desc.contains("status"));
    }
}
