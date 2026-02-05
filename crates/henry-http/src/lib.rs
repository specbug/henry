//! HTTP API server for Henry daemon.
//!
//! Provides REST endpoints for status, health, module, and container information.

use axum::{
    extract::{Path, Query, State},
    http::{header, Method, StatusCode},
    middleware,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use henry_claude::{ClaudeManager, ClaudeStatus, SessionInfo};
use henry_config::HttpConfig;
use henry_health::{HealthManager, HealthStatus, ModuleHealth, SystemMetrics};
use henry_maint::{BackupInfo, MaintStatus, MaintenanceManager, ScheduledJob, UpdateInfo};
use henry_media::{MediaLibrary, MediaManager, MediaStatus, PlaybackSession};
use henry_agent::AgentEngine;
use henry_reactive::{ReactiveEngine, ReactiveStatus};
use henry_server::{ContainerInfo, ContainerManager};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::info;

mod auth;

#[derive(Error, Debug)]
pub enum HttpError {
    #[error("failed to bind to address: {0}")]
    Bind(#[source] std::io::Error),

    #[error("server error: {0}")]
    Server(String),
}

/// Shared state for HTTP handlers.
#[derive(Clone)]
pub struct AppState {
    pub health: Arc<RwLock<HealthManager>>,
    pub config: HttpConfig,
    pub api_token: Option<String>,
    pub containers: Option<Arc<RwLock<ContainerManager>>>,
    pub media: Option<Arc<RwLock<MediaManager>>>,
    pub claude: Option<Arc<RwLock<ClaudeManager>>>,
    pub maint: Option<Arc<RwLock<MaintenanceManager>>>,
    pub reactive: Option<Arc<RwLock<ReactiveEngine>>>,
    pub agent: Option<Arc<RwLock<AgentEngine>>>,
}

/// Health check response.
#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

/// Full status response.
#[derive(Serialize)]
pub struct StatusResponse {
    pub status: String,
    pub version: String,
    pub uptime_secs: u64,
    pub system: SystemMetrics,
}

/// Module list response.
#[derive(Serialize)]
pub struct ModulesResponse {
    pub modules: Vec<ModuleInfo>,
}

#[derive(Serialize)]
pub struct ModuleInfo {
    pub name: String,
    pub status: String,
    pub enabled: bool,
    pub message: Option<String>,
}

impl From<ModuleHealth> for ModuleInfo {
    fn from(m: ModuleHealth) -> Self {
        Self {
            name: m.name,
            status: m.status.to_string(),
            enabled: m.enabled,
            message: m.message,
        }
    }
}

/// Create and run the HTTP server.
pub async fn run_server(
    config: HttpConfig,
    bind_addr: &str,
    health: Arc<RwLock<HealthManager>>,
    api_token: Option<String>,
    containers: Option<Arc<RwLock<ContainerManager>>>,
    media: Option<Arc<RwLock<MediaManager>>>,
    claude: Option<Arc<RwLock<ClaudeManager>>>,
    maint: Option<Arc<RwLock<MaintenanceManager>>>,
    reactive: Option<Arc<RwLock<ReactiveEngine>>>,
    agent: Option<Arc<RwLock<AgentEngine>>>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) -> Result<(), HttpError> {
    let state = AppState {
        health,
        config: config.clone(),
        api_token: api_token.clone(),
        containers,
        media,
        claude,
        maint,
        reactive,
        agent,
    };

    let app = create_router(state);

    let addr: SocketAddr = format!("{}:{}", bind_addr, config.port)
        .parse()
        .map_err(|e| HttpError::Server(format!("invalid address: {}", e)))?;

    info!("Starting HTTP server on {}", addr);

    let listener = TcpListener::bind(addr).await.map_err(HttpError::Bind)?;

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.recv().await;
            info!("HTTP server received shutdown signal");
        })
        .await
        .map_err(|e| HttpError::Server(e.to_string()))?;

    info!("HTTP server stopped");
    Ok(())
}

/// Create the router with all endpoints.
pub fn create_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION]);

    let api_routes = Router::new()
        .route("/status", get(handle_status))
        .route("/modules", get(handle_modules))
        .route("/metrics", get(handle_metrics))
        .route("/snapshot", get(handle_snapshot))
        // Container endpoints
        .route("/containers", get(handle_containers))
        .route("/containers/{name}", get(handle_container_inspect))
        .route("/containers/{name}/start", post(handle_container_start))
        .route("/containers/{name}/stop", post(handle_container_stop))
        .route("/containers/{name}/restart", post(handle_container_restart))
        .route("/containers/{name}/logs", get(handle_container_logs))
        // Media endpoints
        .route("/media/status", get(handle_media_status))
        .route("/media/libraries", get(handle_media_libraries))
        .route("/media/sessions", get(handle_media_sessions))
        .route("/media/scan", post(handle_media_scan))
        .route("/media/start", post(handle_media_start))
        .route("/media/stop", post(handle_media_stop))
        .route("/media/restart", post(handle_media_restart))
        .route("/media/logs", get(handle_media_logs))
        // Claude endpoints
        .route("/claude/status", get(handle_claude_status))
        .route("/claude/sessions", get(handle_claude_sessions))
        .route("/claude/sessions/new", post(handle_claude_new_session))
        .route("/claude/sessions/{id}", get(handle_claude_get_session))
        .route("/claude/sessions/{id}/stop", post(handle_claude_stop_session))
        .route("/claude/ask", post(handle_claude_ask))
        // Maintenance endpoints
        .route("/maint/status", get(handle_maint_status))
        .route("/maint/backups", get(handle_maint_backups))
        .route("/maint/backups/create", post(handle_maint_create_backup))
        .route("/maint/backups/{name}", axum::routing::delete(handle_maint_delete_backup))
        .route("/maint/jobs", get(handle_maint_jobs))
        .route("/maint/rotate", post(handle_maint_rotate_logs))
        .route("/maint/update-check", post(handle_maint_update_check))
        // Reactive endpoints
        .route("/reactive/status", get(handle_reactive_status))
        .route("/reactive/pause", post(handle_reactive_pause))
        .route("/reactive/resume", post(handle_reactive_resume))
        .route("/reactive/anomalies", get(handle_reactive_anomalies))
        .route("/reactive/actions", get(handle_reactive_actions))
        .route("/reactive/actions/{id}/approve", post(handle_reactive_approve))
        // Agent endpoints
        .route("/agent/status", get(handle_agent_status))
        .route("/agent/tasks", get(handle_agent_tasks))
        .route("/agent/tasks", post(handle_agent_create_task))
        .route("/agent/tasks/{id}", get(handle_agent_get_task))
        .route("/agent/tasks/{id}/cancel", post(handle_agent_cancel_task))
        .route("/agent/tasks/{id}/input", post(handle_agent_input))
        .route("/agent/pause", post(handle_agent_pause))
        .route("/agent/resume", post(handle_agent_resume));

    // Apply auth middleware if token is configured
    let api_routes = if state.api_token.is_some() {
        api_routes.layer(middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
    } else {
        api_routes
    };

    Router::new()
        .route("/health", get(handle_health))
        .route("/", get(handle_root))
        .nest("/api", api_routes)
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn handle_root() -> &'static str {
    "Henry Daemon API"
}

async fn handle_health(State(state): State<AppState>) -> impl IntoResponse {
    let health = state.health.read().await;
    let version = health.version().to_string();
    drop(health);

    Json(HealthResponse {
        status: "ok".to_string(),
        version,
    })
}

async fn handle_status(State(state): State<AppState>) -> impl IntoResponse {
    let mut health = state.health.write().await;
    let snapshot = health.get_snapshot().await;

    let status = match snapshot.overall_status {
        HealthStatus::Healthy => "healthy",
        HealthStatus::Degraded => "degraded",
        HealthStatus::Unhealthy => "unhealthy",
        HealthStatus::Unknown => "unknown",
    };

    Json(StatusResponse {
        status: status.to_string(),
        version: snapshot.version,
        uptime_secs: snapshot.uptime_secs,
        system: snapshot.system,
    })
}

async fn handle_modules(State(state): State<AppState>) -> impl IntoResponse {
    let health = state.health.read().await;
    let modules = health.get_all_modules().await;

    Json(ModulesResponse {
        modules: modules.into_iter().map(ModuleInfo::from).collect(),
    })
}

async fn handle_metrics(State(state): State<AppState>) -> impl IntoResponse {
    let mut health = state.health.write().await;
    let metrics = health.get_system_metrics();

    Json(metrics)
}

async fn handle_snapshot(State(state): State<AppState>) -> impl IntoResponse {
    let mut health = state.health.write().await;
    let snapshot = health.get_snapshot().await;

    Json(snapshot)
}

// Container response types
#[derive(Serialize)]
pub struct ContainersResponse {
    pub containers: Vec<ContainerInfo>,
}

#[derive(Serialize)]
pub struct ContainerActionResponse {
    pub success: bool,
    pub message: String,
}

#[derive(Serialize)]
pub struct ContainerLogsResponse {
    pub name: String,
    pub logs: String,
}

#[derive(Deserialize)]
pub struct LogsQuery {
    pub tail: Option<usize>,
}

async fn handle_containers(
    State(state): State<AppState>,
) -> Result<Json<ContainersResponse>, (StatusCode, String)> {
    let Some(ref containers) = state.containers else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Containers module not enabled".to_string(),
        ));
    };

    let manager = containers.read().await;
    match manager.list(true).await {
        Ok(containers) => Ok(Json(ContainersResponse { containers })),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_container_inspect(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let Some(ref containers) = state.containers else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Containers module not enabled".to_string(),
        ));
    };

    let manager = containers.read().await;
    match manager.inspect(&name).await {
        Ok(detail) => Ok(Json(detail)),
        Err(henry_server::ContainerError::NotFound(_)) => {
            Err((StatusCode::NOT_FOUND, format!("Container not found: {}", name)))
        }
        Err(henry_server::ContainerError::NotAllowed(_)) => {
            Err((StatusCode::FORBIDDEN, format!("Container not in allowlist: {}", name)))
        }
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_container_start(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<ContainerActionResponse>, (StatusCode, String)> {
    let Some(ref containers) = state.containers else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Containers module not enabled".to_string(),
        ));
    };

    let manager = containers.read().await;
    match manager.start(&name).await {
        Ok(()) => Ok(Json(ContainerActionResponse {
            success: true,
            message: format!("Started container: {}", name),
        })),
        Err(henry_server::ContainerError::NotFound(_)) => {
            Err((StatusCode::NOT_FOUND, format!("Container not found: {}", name)))
        }
        Err(henry_server::ContainerError::NotAllowed(_)) => {
            Err((StatusCode::FORBIDDEN, format!("Container not in allowlist: {}", name)))
        }
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_container_stop(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<ContainerActionResponse>, (StatusCode, String)> {
    let Some(ref containers) = state.containers else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Containers module not enabled".to_string(),
        ));
    };

    let manager = containers.read().await;
    match manager.stop(&name).await {
        Ok(()) => Ok(Json(ContainerActionResponse {
            success: true,
            message: format!("Stopped container: {}", name),
        })),
        Err(henry_server::ContainerError::NotFound(_)) => {
            Err((StatusCode::NOT_FOUND, format!("Container not found: {}", name)))
        }
        Err(henry_server::ContainerError::NotAllowed(_)) => {
            Err((StatusCode::FORBIDDEN, format!("Container not in allowlist: {}", name)))
        }
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_container_restart(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<ContainerActionResponse>, (StatusCode, String)> {
    let Some(ref containers) = state.containers else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Containers module not enabled".to_string(),
        ));
    };

    let manager = containers.read().await;
    match manager.restart(&name).await {
        Ok(()) => Ok(Json(ContainerActionResponse {
            success: true,
            message: format!("Restarted container: {}", name),
        })),
        Err(henry_server::ContainerError::NotFound(_)) => {
            Err((StatusCode::NOT_FOUND, format!("Container not found: {}", name)))
        }
        Err(henry_server::ContainerError::NotAllowed(_)) => {
            Err((StatusCode::FORBIDDEN, format!("Container not in allowlist: {}", name)))
        }
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_container_logs(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(query): Query<LogsQuery>,
) -> Result<Json<ContainerLogsResponse>, (StatusCode, String)> {
    let Some(ref containers) = state.containers else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Containers module not enabled".to_string(),
        ));
    };

    let tail = query.tail.unwrap_or(50);
    let manager = containers.read().await;
    match manager.logs(&name, tail).await {
        Ok(logs) => Ok(Json(ContainerLogsResponse { name, logs })),
        Err(henry_server::ContainerError::NotFound(_)) => {
            Err((StatusCode::NOT_FOUND, format!("Container not found: {}", name)))
        }
        Err(henry_server::ContainerError::NotAllowed(_)) => {
            Err((StatusCode::FORBIDDEN, format!("Container not in allowlist: {}", name)))
        }
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

// Media response types
#[derive(Serialize)]
pub struct MediaLibrariesResponse {
    pub libraries: Vec<MediaLibrary>,
}

#[derive(Serialize)]
pub struct MediaSessionsResponse {
    pub sessions: Vec<PlaybackSession>,
}

#[derive(Serialize)]
pub struct MediaActionResponse {
    pub success: bool,
    pub message: String,
}

async fn handle_media_status(
    State(state): State<AppState>,
) -> Result<Json<MediaStatus>, (StatusCode, String)> {
    let Some(ref media) = state.media else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Media module not enabled".to_string(),
        ));
    };

    let manager = media.read().await;
    Ok(Json(manager.status().await))
}

async fn handle_media_libraries(
    State(state): State<AppState>,
) -> Result<Json<MediaLibrariesResponse>, (StatusCode, String)> {
    let Some(ref media) = state.media else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Media module not enabled".to_string(),
        ));
    };

    let manager = media.read().await;
    match manager.libraries().await {
        Ok(libraries) => Ok(Json(MediaLibrariesResponse { libraries })),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_media_sessions(
    State(state): State<AppState>,
) -> Result<Json<MediaSessionsResponse>, (StatusCode, String)> {
    let Some(ref media) = state.media else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Media module not enabled".to_string(),
        ));
    };

    let manager = media.read().await;
    match manager.active_sessions().await {
        Ok(sessions) => Ok(Json(MediaSessionsResponse { sessions })),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_media_scan(
    State(state): State<AppState>,
) -> Result<Json<MediaActionResponse>, (StatusCode, String)> {
    let Some(ref media) = state.media else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Media module not enabled".to_string(),
        ));
    };

    let manager = media.read().await;
    match manager.scan_libraries().await {
        Ok(()) => Ok(Json(MediaActionResponse {
            success: true,
            message: "Library scan started".to_string(),
        })),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_media_start(
    State(state): State<AppState>,
) -> Result<Json<MediaActionResponse>, (StatusCode, String)> {
    let Some(ref media) = state.media else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Media module not enabled".to_string(),
        ));
    };

    let manager = media.read().await;
    match manager.start_container().await {
        Ok(()) => Ok(Json(MediaActionResponse {
            success: true,
            message: format!("Started Jellyfin container: {}", manager.container_name()),
        })),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_media_stop(
    State(state): State<AppState>,
) -> Result<Json<MediaActionResponse>, (StatusCode, String)> {
    let Some(ref media) = state.media else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Media module not enabled".to_string(),
        ));
    };

    let manager = media.read().await;
    match manager.stop_container().await {
        Ok(()) => Ok(Json(MediaActionResponse {
            success: true,
            message: format!("Stopped Jellyfin container: {}", manager.container_name()),
        })),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_media_restart(
    State(state): State<AppState>,
) -> Result<Json<MediaActionResponse>, (StatusCode, String)> {
    let Some(ref media) = state.media else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Media module not enabled".to_string(),
        ));
    };

    let manager = media.read().await;
    match manager.restart_container().await {
        Ok(()) => Ok(Json(MediaActionResponse {
            success: true,
            message: format!("Restarted Jellyfin container: {}", manager.container_name()),
        })),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_media_logs(
    State(state): State<AppState>,
    Query(query): Query<LogsQuery>,
) -> Result<Json<ContainerLogsResponse>, (StatusCode, String)> {
    let Some(ref media) = state.media else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Media module not enabled".to_string(),
        ));
    };

    let tail = query.tail.unwrap_or(50);
    let manager = media.read().await;
    let name = manager.container_name().to_string();
    match manager.container_logs(tail).await {
        Ok(logs) => Ok(Json(ContainerLogsResponse { name, logs })),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

// Claude response types
#[derive(Serialize)]
pub struct ClaudeSessionsResponse {
    pub sessions: Vec<SessionInfo>,
}

#[derive(Deserialize)]
pub struct NewSessionRequest {
    pub workspace: String,
}

#[derive(Serialize)]
pub struct ClaudeActionResponse {
    pub success: bool,
    pub message: String,
}

#[derive(Deserialize)]
pub struct AskRequest {
    pub question: String,
    pub model: Option<String>,
    pub max_tokens: Option<u32>,
}

#[derive(Serialize)]
pub struct AskResponse {
    pub response: String,
    pub model: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

async fn handle_claude_status(
    State(state): State<AppState>,
) -> Result<Json<ClaudeStatus>, (StatusCode, String)> {
    let Some(ref claude) = state.claude else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Claude module not enabled".to_string(),
        ));
    };

    let manager = claude.read().await;
    Ok(Json(manager.status().await))
}

async fn handle_claude_sessions(
    State(state): State<AppState>,
) -> Result<Json<ClaudeSessionsResponse>, (StatusCode, String)> {
    let Some(ref claude) = state.claude else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Claude module not enabled".to_string(),
        ));
    };

    let manager = claude.read().await;
    let sessions = manager.list_sessions().await;
    Ok(Json(ClaudeSessionsResponse { sessions }))
}

async fn handle_claude_new_session(
    State(state): State<AppState>,
    Json(request): Json<NewSessionRequest>,
) -> Result<Json<SessionInfo>, (StatusCode, String)> {
    let Some(ref claude) = state.claude else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Claude module not enabled".to_string(),
        ));
    };

    let manager = claude.read().await;
    match manager.new_session(&request.workspace).await {
        Ok(session) => Ok(Json(session)),
        Err(henry_claude::ClaudeError::Session(henry_claude::SessionError::MaxSessionsReached(max))) => {
            Err((StatusCode::TOO_MANY_REQUESTS, format!("Max sessions reached: {}", max)))
        }
        Err(henry_claude::ClaudeError::Session(henry_claude::SessionError::SessionExists(ws))) => {
            Err((StatusCode::CONFLICT, format!("Session already exists for workspace: {}", ws)))
        }
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_claude_get_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<SessionInfo>, (StatusCode, String)> {
    let Some(ref claude) = state.claude else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Claude module not enabled".to_string(),
        ));
    };

    let manager = claude.read().await;
    match manager.get_session(&id).await {
        Some(session) => Ok(Json(session)),
        None => Err((StatusCode::NOT_FOUND, format!("Session not found: {}", id))),
    }
}

async fn handle_claude_stop_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ClaudeActionResponse>, (StatusCode, String)> {
    let Some(ref claude) = state.claude else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Claude module not enabled".to_string(),
        ));
    };

    let manager = claude.read().await;
    match manager.stop_session(&id).await {
        Ok(()) => Ok(Json(ClaudeActionResponse {
            success: true,
            message: format!("Stopped session: {}", id),
        })),
        Err(henry_claude::ClaudeError::Session(henry_claude::SessionError::SessionNotFound(_))) => {
            Err((StatusCode::NOT_FOUND, format!("Session not found: {}", id)))
        }
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_claude_ask(
    State(state): State<AppState>,
    Json(request): Json<AskRequest>,
) -> Result<Json<AskResponse>, (StatusCode, String)> {
    let Some(ref claude) = state.claude else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Claude module not enabled".to_string(),
        ));
    };

    let manager = claude.read().await;
    let messages = vec![henry_claude::Message::user(&request.question)];

    match manager
        .send_message(
            &messages,
            request.model.as_deref(),
            request.max_tokens,
            None,
        )
        .await
    {
        Ok(response) => Ok(Json(AskResponse {
            response: response.content,
            model: response.model,
            input_tokens: response.input_tokens,
            output_tokens: response.output_tokens,
        })),
        Err(henry_claude::ClaudeError::NotConfigured) => {
            Err((StatusCode::SERVICE_UNAVAILABLE, "API key not configured".to_string()))
        }
        Err(henry_claude::ClaudeError::Api(e)) => {
            Err((StatusCode::BAD_GATEWAY, format!("Anthropic API error: {}", e)))
        }
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

// Maintenance response types
#[derive(Serialize)]
pub struct MaintBackupsResponse {
    pub backups: Vec<BackupInfo>,
}

#[derive(Serialize)]
pub struct MaintJobsResponse {
    pub jobs: Vec<ScheduledJob>,
}

#[derive(Serialize)]
pub struct MaintActionResponse {
    pub success: bool,
    pub message: String,
}

#[derive(Serialize)]
pub struct LogRotationResponse {
    pub success: bool,
    pub files_rotated: usize,
    pub files_deleted: usize,
    pub bytes_processed: u64,
}

async fn handle_maint_status(
    State(state): State<AppState>,
) -> Result<Json<MaintStatus>, (StatusCode, String)> {
    let Some(ref maint) = state.maint else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Maintenance module not enabled".to_string(),
        ));
    };

    let manager = maint.read().await;
    Ok(Json(manager.status().await))
}

async fn handle_maint_backups(
    State(state): State<AppState>,
) -> Result<Json<MaintBackupsResponse>, (StatusCode, String)> {
    let Some(ref maint) = state.maint else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Maintenance module not enabled".to_string(),
        ));
    };

    let manager = maint.read().await;
    match manager.list_backups() {
        Ok(backups) => Ok(Json(MaintBackupsResponse { backups })),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_maint_create_backup(
    State(state): State<AppState>,
) -> Result<Json<BackupInfo>, (StatusCode, String)> {
    let Some(ref maint) = state.maint else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Maintenance module not enabled".to_string(),
        ));
    };

    let mut manager = maint.write().await;
    match manager.backup_now().await {
        Ok(backup) => Ok(Json(backup)),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_maint_delete_backup(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<MaintActionResponse>, (StatusCode, String)> {
    let Some(ref maint) = state.maint else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Maintenance module not enabled".to_string(),
        ));
    };

    let manager = maint.read().await;
    match manager.delete_backup(&name) {
        Ok(()) => Ok(Json(MaintActionResponse {
            success: true,
            message: format!("Deleted backup: {}", name),
        })),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_maint_jobs(
    State(state): State<AppState>,
) -> Result<Json<MaintJobsResponse>, (StatusCode, String)> {
    let Some(ref maint) = state.maint else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Maintenance module not enabled".to_string(),
        ));
    };

    let manager = maint.read().await;
    let jobs = manager.list_jobs().await;
    Ok(Json(MaintJobsResponse { jobs }))
}

async fn handle_maint_rotate_logs(
    State(state): State<AppState>,
) -> Result<Json<LogRotationResponse>, (StatusCode, String)> {
    let Some(ref maint) = state.maint else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Maintenance module not enabled".to_string(),
        ));
    };

    let mut manager = maint.write().await;
    match manager.rotate_logs_now().await {
        Ok(result) => Ok(Json(LogRotationResponse {
            success: true,
            files_rotated: result.files_rotated,
            files_deleted: result.files_deleted,
            bytes_processed: result.bytes_processed,
        })),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_maint_update_check(
    State(state): State<AppState>,
) -> Result<Json<UpdateInfo>, (StatusCode, String)> {
    let Some(ref maint) = state.maint else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Maintenance module not enabled".to_string(),
        ));
    };

    let mut manager = maint.write().await;
    match manager.check_updates_now().await {
        Ok(info) => Ok(Json(info)),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

// Reactive response types
#[derive(Serialize)]
pub struct ReactiveActionResponse {
    pub success: bool,
    pub message: String,
}

#[derive(Serialize)]
pub struct ReactiveAnomaliesResponse {
    pub anomalies: Vec<AnomalyInfo>,
}

#[derive(Serialize)]
pub struct AnomalyInfo {
    pub id: String,
    pub event_type: String,
    pub description: String,
    pub severity: String,
    pub detected_at: String,
    pub resolved: bool,
}

#[derive(Serialize)]
pub struct ReactiveActionsResponse {
    pub actions: Vec<ActionInfo>,
}

#[derive(Serialize)]
pub struct ActionInfo {
    pub id: String,
    pub anomaly_id: String,
    pub action_type: String,
    pub description: String,
    pub status: String,
    pub created_at: String,
    pub requires_approval: bool,
}

#[derive(Deserialize)]
pub struct ApproveRequest {
    pub approved_by: Option<String>,
}

async fn handle_reactive_status(
    State(state): State<AppState>,
) -> Result<Json<ReactiveStatus>, (StatusCode, String)> {
    let Some(ref reactive) = state.reactive else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Reactive module not enabled".to_string(),
        ));
    };

    let engine = reactive.read().await;
    Ok(Json(engine.status().await))
}

async fn handle_reactive_pause(
    State(state): State<AppState>,
) -> Result<Json<ReactiveActionResponse>, (StatusCode, String)> {
    let Some(ref reactive) = state.reactive else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Reactive module not enabled".to_string(),
        ));
    };

    let engine = reactive.read().await;
    match engine.pause().await {
        Ok(()) => Ok(Json(ReactiveActionResponse {
            success: true,
            message: "Reactive engine paused".to_string(),
        })),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_reactive_resume(
    State(state): State<AppState>,
) -> Result<Json<ReactiveActionResponse>, (StatusCode, String)> {
    let Some(ref reactive) = state.reactive else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Reactive module not enabled".to_string(),
        ));
    };

    let engine = reactive.read().await;
    match engine.resume().await {
        Ok(()) => Ok(Json(ReactiveActionResponse {
            success: true,
            message: "Reactive engine resumed".to_string(),
        })),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_reactive_anomalies(
    State(state): State<AppState>,
) -> Result<Json<ReactiveAnomaliesResponse>, (StatusCode, String)> {
    let Some(ref reactive) = state.reactive else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Reactive module not enabled".to_string(),
        ));
    };

    let engine = reactive.read().await;
    let anomalies = engine.get_anomalies().await;

    let anomaly_infos: Vec<AnomalyInfo> = anomalies
        .into_iter()
        .map(|a| {
            let resolved = a.is_resolved();
            AnomalyInfo {
                id: a.id,
                event_type: a.event.event_type().to_string(),
                description: a.event.description(),
                severity: a.severity.to_string(),
                detected_at: a.detected_at.to_rfc3339(),
                resolved,
            }
        })
        .collect();

    Ok(Json(ReactiveAnomaliesResponse {
        anomalies: anomaly_infos,
    }))
}

async fn handle_reactive_actions(
    State(state): State<AppState>,
) -> Result<Json<ReactiveActionsResponse>, (StatusCode, String)> {
    let Some(ref reactive) = state.reactive else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Reactive module not enabled".to_string(),
        ));
    };

    let engine = reactive.read().await;
    let actions = engine.get_actions().await;

    let action_infos: Vec<ActionInfo> = actions
        .into_iter()
        .map(|a| ActionInfo {
            id: a.id,
            anomaly_id: a.anomaly_id,
            action_type: a.action_type.action_type().to_string(),
            description: a.action_type.description(),
            status: a.status.to_string(),
            created_at: a.created_at.to_rfc3339(),
            requires_approval: a.requires_approval,
        })
        .collect();

    Ok(Json(ReactiveActionsResponse {
        actions: action_infos,
    }))
}

async fn handle_reactive_approve(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<ApproveRequest>,
) -> Result<Json<ReactiveActionResponse>, (StatusCode, String)> {
    let Some(ref reactive) = state.reactive else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Reactive module not enabled".to_string(),
        ));
    };

    let approved_by = request.approved_by.unwrap_or_else(|| "api".to_string());
    let engine = reactive.read().await;
    match engine.approve_action(&id, &approved_by).await {
        Ok(()) => Ok(Json(ReactiveActionResponse {
            success: true,
            message: format!("Action {} approved", id),
        })),
        Err(henry_reactive::ReactiveError::ActionNotFound(_)) => {
            Err((StatusCode::NOT_FOUND, format!("Action not found: {}", id)))
        }
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

// ===== Agent endpoints =====

#[derive(Serialize)]
pub struct AgentStatusResponse {
    pub state: String,
    pub pending_tasks: usize,
    pub enabled: bool,
}

#[derive(Serialize)]
pub struct AgentTasksResponse {
    pub tasks: Vec<AgentTaskInfo>,
}

#[derive(Serialize)]
pub struct AgentTaskInfo {
    pub id: String,
    pub title: String,
    pub status: String,
    pub priority: String,
    pub progress: String,
    pub tokens_used: u32,
    pub token_budget: u32,
    pub created_at: String,
}

#[derive(Serialize)]
pub struct AgentTaskDetailResponse {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: String,
    pub priority: String,
    pub progress: String,
    pub steps: Vec<AgentStepInfo>,
    pub tokens_used: u32,
    pub token_budget: u32,
    pub created_at: String,
    pub created_by: String,
    pub result: Option<AgentTaskResultInfo>,
}

#[derive(Serialize)]
pub struct AgentStepInfo {
    pub id: String,
    pub description: String,
    pub status: String,
    pub iterations: usize,
    pub tokens_used: u32,
}

#[derive(Serialize)]
pub struct AgentTaskResultInfo {
    pub success: bool,
    pub summary: String,
    pub tokens_used: u32,
    pub duration_secs: u64,
}

#[derive(Deserialize)]
pub struct CreateTaskRequest {
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub priority: Option<String>,
}

#[derive(Serialize)]
pub struct CreateTaskResponse {
    pub success: bool,
    pub task_id: String,
}

#[derive(Deserialize)]
pub struct TaskInputRequest {
    pub key: String,
    pub value: String,
}

#[derive(Serialize)]
pub struct AgentActionResponse {
    pub success: bool,
    pub message: String,
}

async fn handle_agent_status(
    State(state): State<AppState>,
) -> Result<Json<AgentStatusResponse>, (StatusCode, String)> {
    let Some(ref agent) = state.agent else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Agent module not enabled".to_string(),
        ));
    };

    let engine = agent.read().await;
    let state_str = match engine.state() {
        henry_agent::types::AgentState::Idle => "idle",
        henry_agent::types::AgentState::Processing => "processing",
        henry_agent::types::AgentState::Paused => "paused",
    };

    Ok(Json(AgentStatusResponse {
        state: state_str.to_string(),
        pending_tasks: engine.pending_count(),
        enabled: engine.is_enabled(),
    }))
}

async fn handle_agent_tasks(
    State(state): State<AppState>,
) -> Result<Json<AgentTasksResponse>, (StatusCode, String)> {
    let Some(ref agent) = state.agent else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Agent module not enabled".to_string(),
        ));
    };

    let engine = agent.read().await;
    let tasks = engine.list_tasks(None);

    let task_infos: Vec<AgentTaskInfo> = tasks
        .into_iter()
        .map(|t| AgentTaskInfo {
            id: t.id.clone(),
            title: t.title.clone(),
            status: t.status.to_string(),
            priority: t.priority.to_string(),
            progress: format!("{}/{}", t.current_step_index, t.steps.len()),
            tokens_used: t.tokens_used,
            token_budget: t.token_budget,
            created_at: t.created_at.to_rfc3339(),
        })
        .collect();

    Ok(Json(AgentTasksResponse { tasks: task_infos }))
}

async fn handle_agent_create_task(
    State(state): State<AppState>,
    Json(request): Json<CreateTaskRequest>,
) -> Result<Json<CreateTaskResponse>, (StatusCode, String)> {
    let Some(ref agent) = state.agent else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Agent module not enabled".to_string(),
        ));
    };

    let mut engine = agent.write().await;

    let result = if let Some(priority_str) = request.priority {
        let priority = match priority_str.to_lowercase().as_str() {
            "low" => henry_agent::types::TaskPriority::Low,
            "high" => henry_agent::types::TaskPriority::High,
            "critical" => henry_agent::types::TaskPriority::Critical,
            _ => henry_agent::types::TaskPriority::Normal,
        };
        engine
            .submit_task_with_priority(
                request.title,
                request.description,
                "api".to_string(),
                priority,
            )
            .await
    } else {
        engine
            .submit_task(request.title, request.description, "api".to_string())
            .await
    };

    match result {
        Ok(task_id) => Ok(Json(CreateTaskResponse {
            success: true,
            task_id,
        })),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

async fn handle_agent_get_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AgentTaskDetailResponse>, (StatusCode, String)> {
    let Some(ref agent) = state.agent else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Agent module not enabled".to_string(),
        ));
    };

    let engine = agent.read().await;

    // Find task by ID or prefix
    let tasks = engine.list_tasks(None);
    let task = tasks.iter().find(|t| t.id == id || t.id.starts_with(&id));

    let Some(task) = task else {
        return Err((StatusCode::NOT_FOUND, format!("Task not found: {}", id)));
    };

    let steps: Vec<AgentStepInfo> = task
        .steps
        .iter()
        .map(|s| AgentStepInfo {
            id: s.id.clone(),
            description: s.description.clone(),
            status: s.status.to_string(),
            iterations: s.iterations.len(),
            tokens_used: s.tokens_used,
        })
        .collect();

    let result = task.result.as_ref().map(|r| AgentTaskResultInfo {
        success: r.success,
        summary: r.summary.clone(),
        tokens_used: r.tokens_used,
        duration_secs: r.duration_secs,
    });

    Ok(Json(AgentTaskDetailResponse {
        id: task.id.clone(),
        title: task.title.clone(),
        description: task.description.clone(),
        status: task.status.to_string(),
        priority: task.priority.to_string(),
        progress: format!("{}/{}", task.current_step_index, task.steps.len()),
        steps,
        tokens_used: task.tokens_used,
        token_budget: task.token_budget,
        created_at: task.created_at.to_rfc3339(),
        created_by: task.created_by.clone(),
        result,
    }))
}

async fn handle_agent_cancel_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AgentActionResponse>, (StatusCode, String)> {
    let Some(ref agent) = state.agent else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Agent module not enabled".to_string(),
        ));
    };

    let mut engine = agent.write().await;
    match engine.cancel_task(&id).await {
        Ok(()) => Ok(Json(AgentActionResponse {
            success: true,
            message: format!("Task {} cancelled", id),
        })),
        Err(e) => Err((StatusCode::BAD_REQUEST, e.to_string())),
    }
}

async fn handle_agent_input(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<TaskInputRequest>,
) -> Result<Json<AgentActionResponse>, (StatusCode, String)> {
    let Some(ref agent) = state.agent else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Agent module not enabled".to_string(),
        ));
    };

    let mut engine = agent.write().await;
    match engine.provide_input(&id, request.key, request.value).await {
        Ok(()) => Ok(Json(AgentActionResponse {
            success: true,
            message: format!("Input provided for task {}", id),
        })),
        Err(e) => Err((StatusCode::BAD_REQUEST, e.to_string())),
    }
}

async fn handle_agent_pause(
    State(state): State<AppState>,
) -> Result<Json<AgentActionResponse>, (StatusCode, String)> {
    let Some(ref agent) = state.agent else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Agent module not enabled".to_string(),
        ));
    };

    let mut engine = agent.write().await;
    engine.pause();
    Ok(Json(AgentActionResponse {
        success: true,
        message: "Agent engine paused".to_string(),
    }))
}

async fn handle_agent_resume(
    State(state): State<AppState>,
) -> Result<Json<AgentActionResponse>, (StatusCode, String)> {
    let Some(ref agent) = state.agent else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Agent module not enabled".to_string(),
        ));
    };

    let mut engine = agent.write().await;
    engine.resume();
    Ok(Json(AgentActionResponse {
        success: true,
        message: "Agent engine resumed".to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn create_test_state() -> AppState {
        AppState {
            health: Arc::new(RwLock::new(HealthManager::new("0.1.0-test"))),
            config: HttpConfig::default(),
            api_token: None,
            containers: None,
            media: None,
            claude: None,
            maint: None,
            reactive: None,
            agent: None,
        }
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let state = create_test_state();
        let app = create_router(state);

        let response = app
            .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_root_endpoint() {
        let state = create_test_state();
        let app = create_router(state);

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}
