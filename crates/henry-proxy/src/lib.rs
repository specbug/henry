//! Reverse proxy management for Henry daemon.
//!
//! Manages a Caddy container for dynamic reverse proxy configuration,
//! supporting multiple domains and path-based routing.

use bollard::container::{
    Config, CreateContainerOptions, InspectContainerOptions, ListContainersOptions,
    RemoveContainerOptions, StartContainerOptions, StopContainerOptions,
};
use bollard::models::{HostConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum};
use bollard::Docker;
use henry_config::ProxyConfig;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

pub mod caddy;
pub mod types;

pub use types::{CreateRouteRequest, ProxyStatus, Route};

#[derive(Error, Debug)]
pub enum ProxyError {
    #[error("failed to connect to container runtime: {0}")]
    Connection(#[from] bollard::errors::Error),

    #[error("proxy container not running")]
    NotRunning,

    #[error("route not found: {0}")]
    RouteNotFound(String),

    #[error("route already exists: {0}/{1}")]
    RouteExists(String, String),

    #[error("failed to write Caddyfile: {0}")]
    WriteConfig(#[source] std::io::Error),

    #[error("failed to reload Caddy: {0}")]
    ReloadFailed(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Proxy manager for Caddy reverse proxy container.
pub struct ProxyManager {
    config: ProxyConfig,
    docker: Docker,
    routes: Arc<RwLock<Vec<Route>>>,
}

impl ProxyManager {
    /// Create a new proxy manager.
    pub async fn new(config: ProxyConfig, docker: Docker) -> Result<Self, ProxyError> {
        // Verify Docker connection
        docker.ping().await?;

        let manager = Self {
            config,
            docker,
            routes: Arc::new(RwLock::new(Vec::new())),
        };

        // Ensure config directory exists
        manager.ensure_config_dir()?;

        Ok(manager)
    }

    /// Create from an existing ContainerManager's Docker connection.
    pub async fn from_container_manager(
        config: ProxyConfig,
        containers: &henry_server::ContainerManager,
    ) -> Result<Self, ProxyError> {
        // Connect to same Docker socket
        let socket_url = if config.socket_path.is_some() {
            config.socket_path.clone().unwrap()
        } else {
            henry_server::detect_podman_socket()
                .or_else(henry_server::detect_docker_socket)
                .unwrap_or_else(|| "unix:///var/run/docker.sock".to_string())
        };

        let docker = Docker::connect_with_socket(&socket_url, 120, bollard::API_DEFAULT_VERSION)?;
        Self::new(config, docker).await
    }

    /// Ensure the Caddy config directory exists.
    fn ensure_config_dir(&self) -> Result<(), ProxyError> {
        let config_dir = self.config_dir();
        if !config_dir.exists() {
            std::fs::create_dir_all(&config_dir)?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(())
    }

    /// Get the Caddy config directory.
    fn config_dir(&self) -> PathBuf {
        self.config.config_dir.clone()
    }

    /// Get the Caddyfile path.
    fn caddyfile_path(&self) -> PathBuf {
        self.config_dir().join("Caddyfile")
    }

    /// Get proxy status.
    pub async fn status(&self) -> ProxyStatus {
        let running = self.is_container_running().await;
        let routes = self.routes.read().await;

        let domains: Vec<String> = routes
            .iter()
            .map(|r| r.domain.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        ProxyStatus {
            enabled: self.config.enabled,
            running,
            container_name: self.config.caddy_container.clone(),
            route_count: routes.len(),
            domains,
            http_port: self.config.http_port,
            https_port: self.config.https_port,
        }
    }

    /// Check if the Caddy container is running.
    pub async fn is_container_running(&self) -> bool {
        match self
            .docker
            .inspect_container(&self.config.caddy_container, None::<InspectContainerOptions>)
            .await
        {
            Ok(info) => info
                .state
                .and_then(|s| s.running)
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    /// Ensure the Caddy container is running.
    pub async fn ensure_running(&mut self) -> Result<(), ProxyError> {
        if self.is_container_running().await {
            debug!("Caddy container already running");
            return Ok(());
        }

        // Check if container exists but is stopped
        let exists = self.container_exists().await;

        if exists {
            info!("Starting existing Caddy container");
            self.docker
                .start_container(
                    &self.config.caddy_container,
                    None::<StartContainerOptions<String>>,
                )
                .await?;
        } else {
            info!("Creating and starting Caddy container");
            self.create_container().await?;
        }

        // Write initial Caddyfile
        self.write_caddyfile().await?;

        Ok(())
    }

    /// Check if the container exists (running or stopped).
    async fn container_exists(&self) -> bool {
        let mut filters = HashMap::new();
        filters.insert("name", vec![self.config.caddy_container.as_str()]);

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

    /// Create the Caddy container.
    async fn create_container(&self) -> Result<(), ProxyError> {
        let config_dir = self.config_dir();
        let caddyfile_path = self.caddyfile_path();

        // Write initial empty Caddyfile if it doesn't exist
        if !caddyfile_path.exists() {
            let initial_config = caddy::generate_health_caddyfile(self.config.http_port);
            std::fs::write(&caddyfile_path, initial_config)?;
        }

        // Port bindings
        let mut port_bindings = HashMap::new();
        port_bindings.insert(
            "80/tcp".to_string(),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some(self.config.http_port.to_string()),
            }]),
        );
        port_bindings.insert(
            "443/tcp".to_string(),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some(self.config.https_port.to_string()),
            }]),
        );

        // Volume bindings
        let binds = vec![
            format!("{}:/etc/caddy/Caddyfile:ro", caddyfile_path.display()),
            format!("{}:/data", config_dir.join("data").display()),
            format!("{}:/config", config_dir.join("config").display()),
        ];

        // Create data and config directories
        std::fs::create_dir_all(config_dir.join("data"))?;
        std::fs::create_dir_all(config_dir.join("config"))?;

        let host_config = HostConfig {
            binds: Some(binds),
            port_bindings: Some(port_bindings),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            }),
            network_mode: Some(self.config.network_name.clone()),
            ..Default::default()
        };

        let container_config = Config {
            image: Some(self.config.caddy_image.clone()),
            host_config: Some(host_config),
            ..Default::default()
        };

        let options = CreateContainerOptions {
            name: &self.config.caddy_container,
            platform: None,
        };

        self.docker
            .create_container(Some(options), container_config)
            .await?;

        self.docker
            .start_container(
                &self.config.caddy_container,
                None::<StartContainerOptions<String>>,
            )
            .await?;

        info!("Created and started Caddy container: {}", self.config.caddy_container);
        Ok(())
    }

    /// Stop the Caddy container.
    pub async fn stop(&self) -> Result<(), ProxyError> {
        if !self.is_container_running().await {
            return Ok(());
        }

        let options = StopContainerOptions { t: 10 };
        self.docker
            .stop_container(&self.config.caddy_container, Some(options))
            .await?;

        info!("Stopped Caddy container");
        Ok(())
    }

    /// Remove the Caddy container.
    pub async fn remove(&self) -> Result<(), ProxyError> {
        // Stop first if running
        self.stop().await.ok();

        let options = RemoveContainerOptions {
            force: true,
            ..Default::default()
        };

        self.docker
            .remove_container(&self.config.caddy_container, Some(options))
            .await?;

        info!("Removed Caddy container");
        Ok(())
    }

    /// Add a route.
    pub async fn add_route(&self, route: Route) -> Result<Route, ProxyError> {
        let mut routes = self.routes.write().await;

        // Check for existing route with same domain/path
        if routes
            .iter()
            .any(|r| r.domain == route.domain && r.path_prefix == route.path_prefix)
        {
            return Err(ProxyError::RouteExists(
                route.domain.clone(),
                route.path_prefix.clone(),
            ));
        }

        routes.push(route.clone());
        drop(routes);

        // Update Caddyfile
        self.write_caddyfile().await?;

        // Reload Caddy
        self.reload().await?;

        info!(
            "Added route: {} {} -> {}",
            route.domain, route.path_prefix, route.upstream
        );

        Ok(route)
    }

    /// Remove a route by ID.
    pub async fn remove_route(&self, id: &str) -> Result<(), ProxyError> {
        let mut routes = self.routes.write().await;

        let original_len = routes.len();
        routes.retain(|r| r.id != id);

        if routes.len() == original_len {
            return Err(ProxyError::RouteNotFound(id.to_string()));
        }

        drop(routes);

        // Update Caddyfile
        self.write_caddyfile().await?;

        // Reload Caddy
        self.reload().await?;

        info!("Removed route: {}", id);
        Ok(())
    }

    /// Get all routes.
    pub async fn list_routes(&self) -> Vec<Route> {
        self.routes.read().await.clone()
    }

    /// Get a route by ID.
    pub async fn get_route(&self, id: &str) -> Option<Route> {
        self.routes.read().await.iter().find(|r| r.id == id).cloned()
    }

    /// Get routes by domain.
    pub async fn get_routes_by_domain(&self, domain: &str) -> Vec<Route> {
        self.routes
            .read()
            .await
            .iter()
            .filter(|r| r.domain == domain)
            .cloned()
            .collect()
    }

    /// Get routes by app ID.
    pub async fn get_routes_by_app(&self, app_id: &str) -> Vec<Route> {
        self.routes
            .read()
            .await
            .iter()
            .filter(|r| r.app_id.as_deref() == Some(app_id))
            .cloned()
            .collect()
    }

    /// Remove all routes for an app.
    pub async fn remove_routes_for_app(&self, app_id: &str) -> Result<usize, ProxyError> {
        let mut routes = self.routes.write().await;
        let original_len = routes.len();
        routes.retain(|r| r.app_id.as_deref() != Some(app_id));
        let removed = original_len - routes.len();
        drop(routes);

        if removed > 0 {
            self.write_caddyfile().await?;
            self.reload().await?;
            info!("Removed {} routes for app: {}", removed, app_id);
        }

        Ok(removed)
    }

    /// Write the Caddyfile based on current routes.
    async fn write_caddyfile(&self) -> Result<(), ProxyError> {
        let routes = self.routes.read().await;
        let caddyfile = if routes.is_empty() {
            caddy::generate_health_caddyfile(self.config.http_port)
        } else {
            caddy::generate_caddyfile(&routes, self.config.acme_email.as_deref())
        };

        let path = self.caddyfile_path();
        std::fs::write(&path, &caddyfile).map_err(ProxyError::WriteConfig)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }

        debug!("Wrote Caddyfile to {}", path.display());
        Ok(())
    }

    /// Reload Caddy configuration.
    pub async fn reload(&self) -> Result<(), ProxyError> {
        if !self.is_container_running().await {
            warn!("Caddy container not running, skipping reload");
            return Ok(());
        }

        // Execute caddy reload inside the container
        let exec = bollard::exec::CreateExecOptions {
            cmd: Some(vec!["caddy", "reload", "--config", "/etc/caddy/Caddyfile"]),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        };

        let exec_id = self
            .docker
            .create_exec(&self.config.caddy_container, exec)
            .await?
            .id;

        let result = self.docker.start_exec(&exec_id, None).await?;

        match result {
            bollard::exec::StartExecResults::Attached { mut output, .. } => {
                use futures::StreamExt;
                while let Some(msg) = output.next().await {
                    match msg {
                        Ok(bollard::container::LogOutput::StdErr { message }) => {
                            let stderr = String::from_utf8_lossy(&message);
                            if stderr.contains("error") || stderr.contains("Error") {
                                error!("Caddy reload error: {}", stderr);
                                return Err(ProxyError::ReloadFailed(stderr.to_string()));
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            return Err(ProxyError::ReloadFailed(e.to_string()));
                        }
                    }
                }
            }
            bollard::exec::StartExecResults::Detached => {}
        }

        info!("Reloaded Caddy configuration");
        Ok(())
    }

    /// Load routes from state (called during initialization).
    pub async fn load_routes(&self, routes: Vec<Route>) {
        let mut current = self.routes.write().await;
        *current = routes;
        info!("Loaded {} routes from state", current.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_route_creation() {
        let route = Route::new("example.com", "/app", "localhost:8080");
        assert!(!route.id.is_empty());
        assert_eq!(route.domain, "example.com");
        assert_eq!(route.path_prefix, "/app");
        assert_eq!(route.upstream, "localhost:8080");
    }

    #[test]
    fn test_route_with_app() {
        let route = Route::new("example.com", "/", "localhost:8080").with_app("app-123");
        assert_eq!(route.app_id, Some("app-123".to_string()));
    }
}
