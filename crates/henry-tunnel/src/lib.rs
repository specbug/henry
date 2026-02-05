//! Cloudflare Tunnel management for Henry daemon.
//!
//! Manages cloudflared container for exposing services to the internet.

use bollard::container::{
    Config, CreateContainerOptions, InspectContainerOptions, ListContainersOptions,
    RemoveContainerOptions, StartContainerOptions, StopContainerOptions,
};
use bollard::models::{HostConfig, RestartPolicy, RestartPolicyNameEnum};
use bollard::Docker;
use henry_config::TunnelConfig;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

pub mod api;
pub mod cloudflared;
pub mod types;

pub use types::{TunnelRoute, TunnelStatus};

#[derive(Error, Debug)]
pub enum TunnelError {
    #[error("failed to connect to container runtime: {0}")]
    Connection(#[from] bollard::errors::Error),

    #[error("tunnel container not running")]
    NotRunning,

    #[error("route not found: {0}")]
    RouteNotFound(String),

    #[error("route already exists: {0}")]
    RouteExists(String),

    #[error("cloudflare API error: {0}")]
    CloudflareApi(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Tunnel manager for Cloudflare Tunnel (cloudflared).
pub struct TunnelManager {
    config: TunnelConfig,
    docker: Docker,
    tunnel_token: Option<String>,
    api_token: Option<String>,
    routes: Arc<RwLock<Vec<TunnelRoute>>>,
}

impl TunnelManager {
    /// Create a new tunnel manager.
    pub async fn new(
        config: TunnelConfig,
        docker: Docker,
        tunnel_token: Option<String>,
        api_token: Option<String>,
    ) -> Result<Self, TunnelError> {
        // Verify Docker connection
        docker.ping().await?;

        Ok(Self {
            config,
            docker,
            tunnel_token,
            api_token,
            routes: Arc::new(RwLock::new(Vec::new())),
        })
    }

    /// Get tunnel status.
    pub async fn status(&self) -> TunnelStatus {
        let running = self.is_container_running().await;
        let routes = self.routes.read().await;

        TunnelStatus {
            enabled: self.config.enabled,
            running,
            container_name: self.config.container_name.clone(),
            tunnel_name: self.config.tunnel_name.clone(),
            route_count: routes.len(),
            api_configured: self.api_token.is_some(),
        }
    }

    /// Check if the cloudflared container is running.
    pub async fn is_container_running(&self) -> bool {
        match self
            .docker
            .inspect_container(&self.config.container_name, None::<InspectContainerOptions>)
            .await
        {
            Ok(info) => info.state.and_then(|s| s.running).unwrap_or(false),
            Err(_) => false,
        }
    }

    /// Ensure the cloudflared container is running.
    pub async fn ensure_running(&mut self) -> Result<(), TunnelError> {
        let Some(ref token) = self.tunnel_token else {
            return Err(TunnelError::Config("Tunnel token not configured".to_string()));
        };

        if self.is_container_running().await {
            debug!("cloudflared container already running");
            return Ok(());
        }

        // Check if container exists but is stopped
        let exists = self.container_exists().await;

        if exists {
            info!("Starting existing cloudflared container");
            self.docker
                .start_container(
                    &self.config.container_name,
                    None::<StartContainerOptions<String>>,
                )
                .await?;
        } else {
            info!("Creating and starting cloudflared container");
            self.create_container(token).await?;
        }

        Ok(())
    }

    /// Check if the container exists (running or stopped).
    async fn container_exists(&self) -> bool {
        let mut filters = HashMap::new();
        filters.insert("name", vec![self.config.container_name.as_str()]);

        let options = ListContainersOptions {
            all: true,
            filters,
            ..Default::default()
        };

        match self.docker.list_containers(Some(options)).await {
            Ok(containers) => !containers.is_empty(),
            Err(_) => false,
        }
    }

    /// Create the cloudflared container.
    async fn create_container(&self, token: &str) -> Result<(), TunnelError> {
        let host_config = HostConfig {
            network_mode: Some(self.config.network_name.clone()),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            }),
            ..Default::default()
        };

        let container_config = Config {
            image: Some(self.config.container_image.clone()),
            cmd: Some(vec![
                "tunnel".to_string(),
                "--no-autoupdate".to_string(),
                "run".to_string(),
                "--token".to_string(),
                token.to_string(),
            ]),
            host_config: Some(host_config),
            ..Default::default()
        };

        let options = CreateContainerOptions {
            name: &self.config.container_name,
            platform: None,
        };

        self.docker
            .create_container(Some(options), container_config)
            .await?;

        self.docker
            .start_container(
                &self.config.container_name,
                None::<StartContainerOptions<String>>,
            )
            .await?;

        info!(
            "Created and started cloudflared container: {}",
            self.config.container_name
        );
        Ok(())
    }

    /// Stop the cloudflared container.
    pub async fn stop(&self) -> Result<(), TunnelError> {
        if !self.is_container_running().await {
            return Ok(());
        }

        let options = StopContainerOptions { t: 10 };
        self.docker
            .stop_container(&self.config.container_name, Some(options))
            .await?;

        info!("Stopped cloudflared container");
        Ok(())
    }

    /// Restart the cloudflared container.
    pub async fn restart(&mut self) -> Result<(), TunnelError> {
        self.stop().await.ok();
        self.ensure_running().await
    }

    /// Add a tunnel route.
    pub async fn add_route(&self, route: TunnelRoute) -> Result<TunnelRoute, TunnelError> {
        let mut routes = self.routes.write().await;

        // Check for existing route
        if routes
            .iter()
            .any(|r| r.hostname == route.hostname && r.path == route.path)
        {
            return Err(TunnelError::RouteExists(format!(
                "{}{}",
                route.hostname,
                route.path.as_deref().unwrap_or("")
            )));
        }

        routes.push(route.clone());

        // Update Cloudflare tunnel config via API if configured
        if self.api_token.is_some() {
            // TODO: Implement Cloudflare API call to update tunnel ingress
            info!("Would update Cloudflare tunnel config via API");
        }

        info!(
            "Added tunnel route: {} -> {}",
            route.hostname, route.service
        );
        Ok(route)
    }

    /// Remove a tunnel route.
    pub async fn remove_route(&self, id: &str) -> Result<(), TunnelError> {
        let mut routes = self.routes.write().await;

        let original_len = routes.len();
        routes.retain(|r| r.id != id);

        if routes.len() == original_len {
            return Err(TunnelError::RouteNotFound(id.to_string()));
        }

        // Update Cloudflare tunnel config via API if configured
        if self.api_token.is_some() {
            // TODO: Implement Cloudflare API call to update tunnel ingress
            info!("Would update Cloudflare tunnel config via API");
        }

        info!("Removed tunnel route: {}", id);
        Ok(())
    }

    /// List all routes.
    pub async fn list_routes(&self) -> Vec<TunnelRoute> {
        self.routes.read().await.clone()
    }

    /// Load routes from state.
    pub async fn load_routes(&self, routes: Vec<TunnelRoute>) {
        let mut current = self.routes.write().await;
        *current = routes;
        info!("Loaded {} tunnel routes from state", current.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tunnel_route_creation() {
        let route = TunnelRoute::new("example.com", "http://localhost:8080");
        assert!(!route.id.is_empty());
        assert_eq!(route.hostname, "example.com");
        assert_eq!(route.service, "http://localhost:8080");
    }
}
