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
use henry_config::HttpConfig;
use henry_health::{HealthManager, HealthStatus, ModuleHealth, SystemMetrics};
use henry_media::{MediaLibrary, MediaManager, MediaStatus, PlaybackSession};
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
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) -> Result<(), HttpError> {
    let state = AppState {
        health,
        config: config.clone(),
        api_token: api_token.clone(),
        containers,
        media,
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
        .route("/media/logs", get(handle_media_logs));

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
