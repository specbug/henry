//! Telegram bot integration for Henry daemon.
//!
//! Provides a Telegram bot interface with allowlist authentication.

use henry_claude::{format_session, ClaudeManager};
use henry_config::TelegramConfig;
use henry_health::{format_bytes, format_duration, HealthManager, HealthStatus};
use henry_maint::{format_backup_size, format_job, MaintenanceManager};
use henry_media::{format_ticks, MediaManager};
use henry_agent::AgentEngine;
use henry_reactive::ReactiveEngine;
use henry_server::ContainerManager;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

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
    #[command(description = "media status and info")]
    Media(String),
    #[command(description = "claude session management: new|list|stop|ask")]
    Claude(String),
    #[command(description = "maintenance: status|backup|jobs|update")]
    Maint(String),
    #[command(description = "reactive control: status|pause|resume|history|approve")]
    Reactive(String),
    #[command(description = "agent tasks: new|list|status|cancel|input")]
    Agent(String),
    #[command(description = "start the bot")]
    Start,
}

/// Shared state for the bot handlers.
pub struct BotState {
    pub health: Arc<RwLock<HealthManager>>,
    pub config: TelegramConfig,
    pub containers: Option<Arc<RwLock<ContainerManager>>>,
    pub media: Option<Arc<RwLock<MediaManager>>>,
    pub claude: Option<Arc<RwLock<ClaudeManager>>>,
    pub maint: Option<Arc<RwLock<MaintenanceManager>>>,
    pub reactive: Option<Arc<RwLock<ReactiveEngine>>>,
    pub agent: Option<Arc<RwLock<AgentEngine>>>,
}

/// Create and run the Telegram bot.
pub async fn run_bot(
    token: String,
    config: TelegramConfig,
    health: Arc<RwLock<HealthManager>>,
    containers: Option<Arc<RwLock<ContainerManager>>>,
    media: Option<Arc<RwLock<MediaManager>>>,
    claude: Option<Arc<RwLock<ClaudeManager>>>,
    maint: Option<Arc<RwLock<MaintenanceManager>>>,
    reactive: Option<Arc<RwLock<ReactiveEngine>>>,
    agent: Option<Arc<RwLock<AgentEngine>>>,
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
        media,
        claude,
        maint,
        reactive,
        agent,
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
        Command::Media(args) => handle_media_command(&state, &args).await,
        Command::Claude(args) => handle_claude_command(&state, &args).await,
        Command::Maint(args) => handle_maint_command(&state, &args).await,
        Command::Reactive(args) => handle_reactive_command(&state, &args).await,
        Command::Agent(args) => handle_agent_command(&state, &args).await,
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

async fn handle_media_command(state: &BotState, args: &str) -> String {
    let Some(ref media) = state.media else {
        return "Media module not enabled.".to_string();
    };

    let parts: Vec<&str> = args.trim().split_whitespace().collect();
    let action = parts.first().map(|s| s.to_lowercase());

    match action.as_deref() {
        None | Some("status") => format_media_status(media).await,
        Some("libraries") | Some("libs") => format_media_libraries(media).await,
        Some("sessions") | Some("playing") => format_media_sessions(media).await,
        Some("scan") => {
            let manager = media.read().await;
            match manager.scan_libraries().await {
                Ok(()) => "Library scan started.".to_string(),
                Err(e) => format!("Error triggering scan: {}", e),
            }
        }
        Some("start") => {
            let manager = media.read().await;
            match manager.start_container().await {
                Ok(()) => format!("Started Jellyfin container: {}", manager.container_name()),
                Err(e) => format!("Error starting Jellyfin: {}", e),
            }
        }
        Some("stop") => {
            let manager = media.read().await;
            match manager.stop_container().await {
                Ok(()) => format!("Stopped Jellyfin container: {}", manager.container_name()),
                Err(e) => format!("Error stopping Jellyfin: {}", e),
            }
        }
        Some("restart") => {
            let manager = media.read().await;
            match manager.restart_container().await {
                Ok(()) => format!("Restarted Jellyfin container: {}", manager.container_name()),
                Err(e) => format!("Error restarting Jellyfin: {}", e),
            }
        }
        Some("logs") => {
            let tail = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(50);
            let manager = media.read().await;
            match manager.container_logs(tail).await {
                Ok(logs) => {
                    if logs.is_empty() {
                        "No Jellyfin logs.".to_string()
                    } else {
                        let max_len = 4000;
                        if logs.len() > max_len {
                            format!("Jellyfin logs (truncated):\n```\n{}...\n```", &logs[..max_len])
                        } else {
                            format!("Jellyfin logs:\n```\n{}\n```", logs)
                        }
                    }
                }
                Err(e) => format!("Error getting logs: {}", e),
            }
        }
        _ => "Usage: /media [status|libraries|sessions|scan|start|stop|restart|logs]".to_string(),
    }
}

async fn format_media_status(media: &Arc<RwLock<MediaManager>>) -> String {
    let manager = media.read().await;
    let status = manager.status().await;

    let mut lines = vec!["Media Status:".to_string()];

    lines.push(format!(
        "  Container: {}",
        if status.container_running { "running" } else { "stopped" }
    ));
    lines.push(format!(
        "  API: {}",
        if status.api_reachable { "reachable" } else { "unreachable" }
    ));

    if let Some(ref server) = status.server {
        lines.push(format!("  Server: {} v{}", server.name, server.version));
        lines.push(format!(
            "  HW Transcode: {}",
            if server.hw_transcode_available { "available" } else { "not available" }
        ));
    }

    lines.push(format!("  Libraries: {}", status.library_count));
    lines.push(format!("  Active Sessions: {}", status.active_sessions));

    if !status.library_paths.is_empty() {
        lines.push("  Paths:".to_string());
        for path in &status.library_paths {
            lines.push(format!("    {}", path));
        }
    }

    lines.join("\n")
}

async fn format_media_libraries(media: &Arc<RwLock<MediaManager>>) -> String {
    let manager = media.read().await;
    match manager.libraries().await {
        Ok(libs) => {
            if libs.is_empty() {
                return "No libraries configured.".to_string();
            }
            let mut lines = vec!["Libraries:".to_string()];
            for lib in libs {
                lines.push(format!("  {} ({})", lib.name, lib.content_type));
                for path in &lib.paths {
                    lines.push(format!("    {}", path));
                }
            }
            lines.join("\n")
        }
        Err(e) => format!("Error fetching libraries: {}", e),
    }
}

async fn format_media_sessions(media: &Arc<RwLock<MediaManager>>) -> String {
    let manager = media.read().await;
    match manager.active_sessions().await {
        Ok(sessions) => {
            if sessions.is_empty() {
                return "No active playback sessions.".to_string();
            }
            let mut lines = vec!["Active Sessions:".to_string()];
            for s in sessions {
                let progress = match (s.position_ticks, s.duration_ticks) {
                    (Some(pos), Some(dur)) if dur > 0 => {
                        format!(" [{}/{}]", format_ticks(pos), format_ticks(dur))
                    }
                    _ => String::new(),
                };
                let pause_indicator = if s.is_paused { " (paused)" } else { "" };
                let transcode = s
                    .transcode_info
                    .as_deref()
                    .map(|t| format!(" [{}]", t))
                    .unwrap_or_default();

                lines.push(format!(
                    "  {} on {}: {}{}{}{}",
                    s.user, s.client, s.item_name, progress, pause_indicator, transcode
                ));
            }
            lines.join("\n")
        }
        Err(e) => format!("Error fetching sessions: {}", e),
    }
}

async fn handle_claude_command(state: &BotState, args: &str) -> String {
    let Some(ref claude) = state.claude else {
        return "Claude module not enabled.".to_string();
    };

    let parts: Vec<&str> = args.trim().split_whitespace().collect();
    let action = parts.first().map(|s| s.to_lowercase());

    match action.as_deref() {
        None | Some("status") => format_claude_status(claude).await,
        Some("list") | Some("sessions") => format_claude_sessions(claude).await,
        Some("new") => {
            let workspace = parts.get(1).map(|s| *s);
            let Some(workspace) = workspace else {
                return "Usage: /claude new <workspace>".to_string();
            };
            let manager = claude.read().await;
            match manager.new_session(workspace).await {
                Ok(session) => format!(
                    "Started Claude Code session:\n  ID: {}\n  Workspace: {}\n  PID: {}",
                    session.id,
                    session.workspace,
                    session.pid.map(|p| p.to_string()).unwrap_or_else(|| "N/A".to_string())
                ),
                Err(e) => format!("Error starting session: {}", e),
            }
        }
        Some("stop") => {
            let session_id = parts.get(1).map(|s| *s);
            let Some(session_id) = session_id else {
                return "Usage: /claude stop <session_id>".to_string();
            };
            let manager = claude.read().await;
            match manager.stop_session(session_id).await {
                Ok(()) => format!("Stopped session: {}", session_id),
                Err(e) => format!("Error stopping session: {}", e),
            }
        }
        Some("ask") => {
            // Collect all remaining text as the question
            if parts.len() < 2 {
                return "Usage: /claude ask <question>".to_string();
            }
            let question = parts[1..].join(" ");
            let manager = claude.read().await;
            match manager.ask(&question).await {
                Ok(response) => {
                    // Truncate if too long for Telegram
                    let max_len = 4000;
                    if response.len() > max_len {
                        format!("{}...\n\n(truncated)", &response[..max_len])
                    } else {
                        response
                    }
                }
                Err(e) => format!("Error: {}", e),
            }
        }
        _ => "Usage: /claude [status|list|new <workspace>|stop <id>|ask <question>]".to_string(),
    }
}

async fn format_claude_status(claude: &Arc<RwLock<ClaudeManager>>) -> String {
    let manager = claude.read().await;
    let status = manager.status().await;

    let mut lines = vec!["Claude Status:".to_string()];
    lines.push(format!(
        "  API Key: {}",
        if status.api_key_configured { "configured" } else { "not configured" }
    ));
    lines.push(format!(
        "  Sessions: {}/{}",
        status.active_sessions, status.max_sessions
    ));
    lines.push(format!("  Workspace: {}", status.workspace_dir));

    lines.join("\n")
}

async fn format_claude_sessions(claude: &Arc<RwLock<ClaudeManager>>) -> String {
    let manager = claude.read().await;
    let sessions = manager.list_sessions().await;

    if sessions.is_empty() {
        return "No Claude Code sessions.".to_string();
    }

    let mut lines = vec!["Claude Sessions:".to_string()];
    for session in sessions {
        lines.push(format!("  {}", format_session(&session)));
    }

    lines.join("\n")
}

async fn handle_maint_command(state: &BotState, args: &str) -> String {
    let Some(ref maint) = state.maint else {
        return "Maintenance module not enabled.".to_string();
    };

    let parts: Vec<&str> = args.trim().split_whitespace().collect();
    let action = parts.first().map(|s| s.to_lowercase());

    match action.as_deref() {
        None | Some("status") => format_maint_status(maint).await,
        Some("backup") => {
            let mut manager = maint.write().await;
            match manager.backup_now().await {
                Ok(backup) => format!(
                    "Backup created:\n  Name: {}\n  Size: {}",
                    backup.name,
                    format_backup_size(backup.size)
                ),
                Err(e) => format!("Error creating backup: {}", e),
            }
        }
        Some("backups") | Some("list") => format_maint_backups(maint).await,
        Some("jobs") | Some("schedule") => format_maint_jobs(maint).await,
        Some("rotate") => {
            let mut manager = maint.write().await;
            match manager.rotate_logs_now().await {
                Ok(result) => format!(
                    "Log rotation complete:\n  Rotated: {} files\n  Deleted: {} old files\n  Processed: {} bytes",
                    result.files_rotated,
                    result.files_deleted,
                    format_backup_size(result.bytes_processed)
                ),
                Err(e) => format!("Error rotating logs: {}", e),
            }
        }
        Some("update") | Some("check") => {
            let mut manager = maint.write().await;
            match manager.check_updates_now().await {
                Ok(info) => {
                    if info.update_available {
                        format!(
                            "Update available!\n  Current: {}\n  Latest: {}\n  URL: {}",
                            info.current_version,
                            info.latest_version,
                            info.release_url.unwrap_or_else(|| "N/A".to_string())
                        )
                    } else {
                        format!("Already up to date (v{})", info.current_version)
                    }
                }
                Err(e) => format!("Error checking updates: {}", e),
            }
        }
        _ => "Usage: /maint [status|backup|backups|jobs|rotate|update]".to_string(),
    }
}

async fn format_maint_status(maint: &Arc<RwLock<MaintenanceManager>>) -> String {
    let manager = maint.read().await;
    let status = manager.status().await;

    let mut lines = vec!["Maintenance Status:".to_string()];
    lines.push(format!("  Version: {}", status.current_version));
    lines.push(format!("  Scheduled Jobs: {}", status.scheduled_jobs));

    if let Some(ref update) = status.available_update {
        lines.push(format!("  Update Available: {}", update));
    }

    lines.push(format!(
        "  Last Backup: {}",
        status
            .last_backup
            .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "never".to_string())
    ));

    lines.push(format!(
        "  Last Update Check: {}",
        status
            .last_update_check
            .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "never".to_string())
    ));

    lines.push(format!(
        "  Backups: {} ({} total)",
        status.backups_retained,
        format_backup_size(status.total_backup_size)
    ));

    lines.join("\n")
}

async fn format_maint_backups(maint: &Arc<RwLock<MaintenanceManager>>) -> String {
    let manager = maint.read().await;
    match manager.list_backups() {
        Ok(backups) => {
            if backups.is_empty() {
                return "No backups found.".to_string();
            }

            let mut lines = vec!["Backups:".to_string()];
            for backup in backups.iter().take(10) {
                lines.push(format!(
                    "  {} ({}) - {}",
                    backup.name,
                    format_backup_size(backup.size),
                    backup.created_at.format("%Y-%m-%d %H:%M")
                ));
            }

            if backups.len() > 10 {
                lines.push(format!("  ... and {} more", backups.len() - 10));
            }

            lines.join("\n")
        }
        Err(e) => format!("Error listing backups: {}", e),
    }
}

async fn format_maint_jobs(maint: &Arc<RwLock<MaintenanceManager>>) -> String {
    let manager = maint.read().await;
    let jobs = manager.list_jobs().await;

    if jobs.is_empty() {
        return "No scheduled jobs.".to_string();
    }

    let mut lines = vec!["Scheduled Jobs:".to_string()];
    for job in jobs {
        lines.push(format!("  {}", format_job(&job)));
    }

    lines.join("\n")
}

async fn handle_reactive_command(state: &BotState, args: &str) -> String {
    let Some(ref reactive) = state.reactive else {
        return "Reactive module not enabled.".to_string();
    };

    let parts: Vec<&str> = args.trim().split_whitespace().collect();
    let action = parts.first().map(|s| s.to_lowercase());

    match action.as_deref() {
        None | Some("status") => format_reactive_status(reactive).await,
        Some("pause") => {
            let engine = reactive.read().await;
            match engine.pause().await {
                Ok(()) => "Reactive engine paused.".to_string(),
                Err(e) => format!("Error pausing: {}", e),
            }
        }
        Some("resume") => {
            let engine = reactive.read().await;
            match engine.resume().await {
                Ok(()) => "Reactive engine resumed.".to_string(),
                Err(e) => format!("Error resuming: {}", e),
            }
        }
        Some("history") => {
            let limit = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
            format_reactive_history(reactive, limit).await
        }
        Some("approve") => {
            let Some(action_id) = parts.get(1) else {
                return "Usage: /reactive approve <action_id>".to_string();
            };
            let engine = reactive.read().await;
            match engine.approve_action(action_id, "telegram").await {
                Ok(()) => format!("Action {} approved.", action_id),
                Err(e) => format!("Error approving action: {}", e),
            }
        }
        Some("anomalies") => format_reactive_anomalies(reactive).await,
        Some("pending") => format_reactive_pending(reactive).await,
        _ => "Usage: /reactive [status|pause|resume|history|approve|anomalies|pending]".to_string(),
    }
}

async fn format_reactive_status(reactive: &Arc<RwLock<ReactiveEngine>>) -> String {
    let engine = reactive.read().await;
    let status = engine.status().await;

    let mut lines = vec!["Reactive Status:".to_string()];
    lines.push(format!(
        "  State: {}",
        if status.paused {
            "paused"
        } else if status.running {
            "running"
        } else {
            "stopped"
        }
    ));

    if let Some(started) = status.started_at {
        lines.push(format!("  Started: {}", started.format("%Y-%m-%d %H:%M")));
    }

    lines.push(format!("  Total Checks: {}", status.total_checks));
    lines.push(format!("  Anomalies Detected: {}", status.anomalies_detected));
    lines.push(format!("  Actions Executed: {}", status.actions_executed));
    lines.push(format!(
        "  Actions This Hour: {}/20",
        status.actions_this_hour
    ));
    lines.push(format!("  Active Anomalies: {}", status.active_anomalies));
    lines.push(format!("  Pending Approvals: {}", status.pending_approvals));
    lines.push(format!(
        "  Quiet Hours: {}",
        if status.in_quiet_hours { "yes" } else { "no" }
    ));

    if let Some(last) = status.last_check {
        lines.push(format!("  Last Check: {}", last.format("%H:%M:%S")));
    }

    lines.join("\n")
}

async fn format_reactive_history(reactive: &Arc<RwLock<ReactiveEngine>>, limit: usize) -> String {
    let engine = reactive.read().await;
    let actions = engine.get_action_history(limit).await;

    if actions.is_empty() {
        return "No actions in history.".to_string();
    }

    let mut lines = vec!["Action History:".to_string()];
    for action in actions {
        let status_emoji = match action.status {
            henry_reactive::types::ActionStatus::Completed => "OK",
            henry_reactive::types::ActionStatus::Failed => "ERR",
            henry_reactive::types::ActionStatus::AwaitingApproval => "WAIT",
            henry_reactive::types::ActionStatus::Executing => "RUN",
            _ => "?",
        };
        lines.push(format!(
            "  {} {} {}",
            status_emoji,
            action.action_type.description(),
            action.created_at.format("%m-%d %H:%M")
        ));
    }

    lines.join("\n")
}

async fn format_reactive_anomalies(reactive: &Arc<RwLock<ReactiveEngine>>) -> String {
    let engine = reactive.read().await;
    let anomalies = engine.get_active_anomalies().await;

    if anomalies.is_empty() {
        return "No active anomalies.".to_string();
    }

    let mut lines = vec!["Active Anomalies:".to_string()];
    for anomaly in anomalies.iter().take(10) {
        let severity = match anomaly.severity {
            henry_reactive::Severity::Info => "INFO",
            henry_reactive::Severity::Warning => "WARN",
            henry_reactive::Severity::Critical => "CRIT",
            henry_reactive::Severity::Emergency => "EMRG",
        };
        lines.push(format!(
            "  {} {} - {}",
            severity,
            anomaly.event.event_type(),
            anomaly.event.description()
        ));
    }

    if anomalies.len() > 10 {
        lines.push(format!("  ... and {} more", anomalies.len() - 10));
    }

    lines.join("\n")
}

async fn format_reactive_pending(reactive: &Arc<RwLock<ReactiveEngine>>) -> String {
    let engine = reactive.read().await;
    let actions = engine.get_pending_actions().await;

    if actions.is_empty() {
        return "No pending actions.".to_string();
    }

    let mut lines = vec!["Pending Approvals:".to_string()];
    for action in actions {
        lines.push(format!(
            "  [{}] {} - {}",
            &action.id[..8],
            action.action_type.description(),
            action.created_at.format("%m-%d %H:%M")
        ));
    }
    lines.push("\nUse /reactive approve <id> to approve.".to_string());

    lines.join("\n")
}

async fn handle_agent_command(state: &BotState, args: &str) -> String {
    let Some(ref agent) = state.agent else {
        return "Agent module not enabled.".to_string();
    };

    let parts: Vec<&str> = args.trim().split_whitespace().collect();
    let action = parts.first().map(|s| s.to_lowercase());

    match action.as_deref() {
        None | Some("status") => format_agent_status(agent).await,
        Some("new") => {
            // Expect: /agent new <title> - <description>
            let rest = parts.get(1..).map(|p| p.join(" ")).unwrap_or_default();
            if rest.is_empty() {
                return "Usage: /agent new <title> - <description>".to_string();
            }

            let (title, description) = if let Some(idx) = rest.find(" - ") {
                (rest[..idx].to_string(), rest[idx + 3..].to_string())
            } else {
                (rest.clone(), rest)
            };

            let mut engine = agent.write().await;
            match engine.submit_task(title, description, "telegram".to_string()).await {
                Ok(task_id) => format!("Task created: {}", &task_id[..8]),
                Err(e) => format!("Error creating task: {}", e),
            }
        }
        Some("list") => format_agent_tasks(agent).await,
        Some("task") | Some("get") => {
            let Some(task_id) = parts.get(1) else {
                return "Usage: /agent task <task_id>".to_string();
            };
            format_agent_task(agent, task_id).await
        }
        Some("cancel") => {
            let Some(task_id) = parts.get(1) else {
                return "Usage: /agent cancel <task_id>".to_string();
            };
            let mut engine = agent.write().await;
            match engine.cancel_task(&task_id.to_string()).await {
                Ok(()) => format!("Task {} cancelled.", task_id),
                Err(e) => format!("Error cancelling task: {}", e),
            }
        }
        Some("input") => {
            // Expect: /agent input <task_id> <key>=<value>
            let Some(task_id) = parts.get(1) else {
                return "Usage: /agent input <task_id> <key>=<value>".to_string();
            };
            let rest = parts.get(2..).map(|p| p.join(" ")).unwrap_or_default();
            let Some((key, value)) = rest.split_once('=') else {
                return "Usage: /agent input <task_id> <key>=<value>".to_string();
            };
            let mut engine = agent.write().await;
            match engine
                .provide_input(&task_id.to_string(), key.to_string(), value.to_string())
                .await
            {
                Ok(()) => format!("Input provided for task {}.", task_id),
                Err(e) => format!("Error providing input: {}", e),
            }
        }
        Some("pause") => {
            let mut engine = agent.write().await;
            engine.pause();
            "Agent engine paused.".to_string()
        }
        Some("resume") => {
            let mut engine = agent.write().await;
            engine.resume();
            "Agent engine resumed.".to_string()
        }
        _ => "Usage: /agent [status|new|list|task|cancel|input|pause|resume]".to_string(),
    }
}

async fn format_agent_status(agent: &Arc<RwLock<AgentEngine>>) -> String {
    let engine = agent.read().await;

    let state_str = match engine.state() {
        henry_agent::types::AgentState::Idle => "idle",
        henry_agent::types::AgentState::Processing => "processing",
        henry_agent::types::AgentState::Paused => "paused",
    };

    let pending = engine.pending_count();

    let mut lines = vec!["Agent Status:".to_string()];
    lines.push(format!("  State: {}", state_str));
    lines.push(format!("  Pending Tasks: {}", pending));
    lines.push(format!("  Enabled: {}", engine.is_enabled()));

    lines.join("\n")
}

async fn format_agent_tasks(agent: &Arc<RwLock<AgentEngine>>) -> String {
    let engine = agent.read().await;
    let tasks = engine.list_tasks(None);

    if tasks.is_empty() {
        return "No tasks.".to_string();
    }

    let mut lines = vec!["Tasks:".to_string()];
    for task in tasks.iter().take(10) {
        let status = match task.status {
            henry_agent::types::TaskStatus::Queued => "WAIT",
            henry_agent::types::TaskStatus::Planning => "PLAN",
            henry_agent::types::TaskStatus::Executing => "RUN",
            henry_agent::types::TaskStatus::AwaitingInput => "INPUT",
            henry_agent::types::TaskStatus::Completed => "OK",
            henry_agent::types::TaskStatus::Failed => "ERR",
            henry_agent::types::TaskStatus::Cancelled => "X",
        };
        lines.push(format!(
            "  {} [{}] {}",
            status,
            &task.id[..8],
            truncate_str(&task.title, 40)
        ));
    }

    if tasks.len() > 10 {
        lines.push(format!("  ... and {} more", tasks.len() - 10));
    }

    lines.join("\n")
}

async fn format_agent_task(agent: &Arc<RwLock<AgentEngine>>, task_id: &str) -> String {
    let engine = agent.read().await;

    // Try to find task by prefix match
    let tasks = engine.list_tasks(None);
    let task = tasks.iter().find(|t| t.id.starts_with(task_id));

    let Some(task) = task else {
        return format!("Task {} not found.", task_id);
    };

    let status = match task.status {
        henry_agent::types::TaskStatus::Queued => "queued",
        henry_agent::types::TaskStatus::Planning => "planning",
        henry_agent::types::TaskStatus::Executing => "executing",
        henry_agent::types::TaskStatus::AwaitingInput => "awaiting input",
        henry_agent::types::TaskStatus::Completed => "completed",
        henry_agent::types::TaskStatus::Failed => "failed",
        henry_agent::types::TaskStatus::Cancelled => "cancelled",
    };

    let mut lines = vec![format!("Task: {}", task.title)];
    lines.push(format!("  ID: {}", &task.id[..8]));
    lines.push(format!("  Status: {}", status));
    lines.push(format!(
        "  Progress: {}/{}",
        task.current_step_index,
        task.steps.len()
    ));
    lines.push(format!(
        "  Tokens: {}/{}",
        task.tokens_used, task.token_budget
    ));
    lines.push(format!(
        "  Created: {}",
        task.created_at.format("%Y-%m-%d %H:%M")
    ));

    if !task.steps.is_empty() {
        lines.push("  Steps:".to_string());
        for (i, step) in task.steps.iter().take(5).enumerate() {
            let step_status = match step.status {
                henry_agent::types::StepStatus::Pending => "-",
                henry_agent::types::StepStatus::Running => ">",
                henry_agent::types::StepStatus::Completed => "OK",
                henry_agent::types::StepStatus::Failed => "X",
                henry_agent::types::StepStatus::Skipped => "~",
            };
            lines.push(format!(
                "    {} {} {}",
                step_status,
                i + 1,
                truncate_str(&step.description, 30)
            ));
        }
    }

    if let Some(ref result) = task.result {
        lines.push(format!("  Result: {}", truncate_str(&result.summary, 100)));
    }

    lines.join("\n")
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
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
        assert!(desc.contains("media"));
        assert!(desc.contains("reactive"));
    }
}
