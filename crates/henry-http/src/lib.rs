//! HTTP API server for Henry daemon.
//!
//! Provides REST endpoints for status, health, and module information.

use axum::{
    extract::State,
    http::{header, Method},
    middleware,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use henry_config::HttpConfig;
use henry_health::{HealthManager, HealthStatus, ModuleHealth, SystemMetrics};
use serde::Serialize;
use std::net::SocketAddr;
use std::sync::Arc;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::{error, info};

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
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) -> Result<(), HttpError> {
    let state = AppState {
        health,
        config: config.clone(),
        api_token: api_token.clone(),
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
        .route("/snapshot", get(handle_snapshot));

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
