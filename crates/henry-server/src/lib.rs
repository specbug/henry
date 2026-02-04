//! Container management for Henry daemon.
//!
//! Provides Podman/Docker container lifecycle management via the bollard library.

use bollard::container::{
    InspectContainerOptions, ListContainersOptions, LogOutput, LogsOptions,
    RestartContainerOptions, StartContainerOptions, StopContainerOptions,
};
use bollard::Docker;
use futures::StreamExt;
use henry_config::ContainerConfig;
use std::collections::HashMap;
use thiserror::Error;
use tracing::{debug, info};

mod podman;
mod types;

pub use podman::{detect_docker_socket, detect_podman_socket};
pub use types::{ContainerDetail, ContainerInfo, ContainerResources, ContainerStatus};

#[derive(Error, Debug)]
pub enum ContainerError {
    #[error("failed to connect to container runtime: {0}")]
    Connection(#[from] bollard::errors::Error),

    #[error("no container runtime socket found")]
    NoSocket,

    #[error("container not found: {0}")]
    NotFound(String),

    #[error("container not in allowlist: {0}")]
    NotAllowed(String),

    #[error("container operation failed: {0}")]
    Operation(String),
}

/// Container manager for Podman/Docker operations.
pub struct ContainerManager {
    docker: Docker,
    config: ContainerConfig,
}

impl ContainerManager {
    /// Create a new container manager.
    pub async fn new(config: ContainerConfig) -> Result<Self, ContainerError> {
        let socket_url = Self::resolve_socket(&config)?;
        info!("Connecting to container runtime at: {}", socket_url);

        let docker = Docker::connect_with_socket(&socket_url, 120, bollard::API_DEFAULT_VERSION)?;

        // Verify connection by pinging
        docker.ping().await?;
        info!("Connected to container runtime");

        Ok(Self { docker, config })
    }

    /// Resolve the socket URL based on configuration.
    fn resolve_socket(config: &ContainerConfig) -> Result<String, ContainerError> {
        // Use explicit socket path if configured
        if let Some(ref path) = config.socket_path {
            if !path.is_empty() {
                return Ok(if path.starts_with("unix://") {
                    path.clone()
                } else {
                    format!("unix://{}", path)
                });
            }
        }

        // Auto-detect socket
        if config.use_podman {
            if let Some(socket) = detect_podman_socket() {
                return Ok(socket);
            }
        }

        // Fall back to Docker socket
        if let Some(socket) = detect_docker_socket() {
            return Ok(socket);
        }

        Err(ContainerError::NoSocket)
    }

    /// Check if a container is in the allowlist.
    fn is_allowed(&self, name: &str) -> bool {
        if self.config.allowed_containers.is_empty() {
            return true;
        }
        self.config
            .allowed_containers
            .iter()
            .any(|allowed| allowed == name || allowed == &format!("/{}", name))
    }

    /// Verify a container is allowed and return an error if not.
    fn verify_allowed(&self, name: &str) -> Result<(), ContainerError> {
        if !self.is_allowed(name) {
            return Err(ContainerError::NotAllowed(name.to_string()));
        }
        Ok(())
    }

    /// List all containers (optionally only running).
    pub async fn list(&self, all: bool) -> Result<Vec<ContainerInfo>, ContainerError> {
        let mut filters = HashMap::new();
        if !all {
            filters.insert("status", vec!["running"]);
        }

        let options = ListContainersOptions {
            all,
            filters,
            ..Default::default()
        };

        let containers = self.docker.list_containers(Some(options)).await?;

        let mut result = Vec::new();
        for c in containers {
            let name = c
                .names
                .as_ref()
                .and_then(|n| n.first())
                .map(|s| s.trim_start_matches('/').to_string())
                .unwrap_or_default();

            // Filter by allowlist
            if !self.is_allowed(&name) {
                continue;
            }

            let id = c.id.unwrap_or_default();
            let short_id = if id.len() > 12 { &id[..12] } else { &id };

            result.push(ContainerInfo {
                id: short_id.to_string(),
                name,
                image: c.image.unwrap_or_default(),
                status: ContainerStatus::from_state(&c.state.unwrap_or_default()),
                status_message: c.status.unwrap_or_default(),
                ports: Self::format_ports(&c.ports.unwrap_or_default()),
                created: c.created.unwrap_or(0),
            });
        }

        Ok(result)
    }

    /// Start a container by name.
    pub async fn start(&self, name: &str) -> Result<(), ContainerError> {
        self.verify_allowed(name)?;
        debug!("Starting container: {}", name);

        self.docker
            .start_container(name, None::<StartContainerOptions<String>>)
            .await
            .map_err(|e| {
                if e.to_string().contains("No such container") {
                    ContainerError::NotFound(name.to_string())
                } else {
                    ContainerError::Connection(e)
                }
            })?;

        info!("Started container: {}", name);
        Ok(())
    }

    /// Stop a container by name.
    pub async fn stop(&self, name: &str) -> Result<(), ContainerError> {
        self.verify_allowed(name)?;
        debug!("Stopping container: {}", name);

        let options = StopContainerOptions { t: 10 };

        self.docker.stop_container(name, Some(options)).await.map_err(|e| {
            if e.to_string().contains("No such container") {
                ContainerError::NotFound(name.to_string())
            } else {
                ContainerError::Connection(e)
            }
        })?;

        info!("Stopped container: {}", name);
        Ok(())
    }

    /// Restart a container by name.
    pub async fn restart(&self, name: &str) -> Result<(), ContainerError> {
        self.verify_allowed(name)?;
        debug!("Restarting container: {}", name);

        let options = RestartContainerOptions { t: 10 };

        self.docker.restart_container(name, Some(options)).await.map_err(|e| {
            if e.to_string().contains("No such container") {
                ContainerError::NotFound(name.to_string())
            } else {
                ContainerError::Connection(e)
            }
        })?;

        info!("Restarted container: {}", name);
        Ok(())
    }

    /// Get container logs (last N lines).
    pub async fn logs(&self, name: &str, tail: usize) -> Result<String, ContainerError> {
        self.verify_allowed(name)?;
        debug!("Fetching logs for container: {} (tail {})", name, tail);

        let options = LogsOptions::<String> {
            stdout: true,
            stderr: true,
            tail: tail.to_string(),
            ..Default::default()
        };

        let mut stream = self.docker.logs(name, Some(options));
        let mut logs = String::new();

        while let Some(result) = stream.next().await {
            match result {
                Ok(output) => {
                    let line = match output {
                        LogOutput::StdOut { message } => String::from_utf8_lossy(&message).to_string(),
                        LogOutput::StdErr { message } => String::from_utf8_lossy(&message).to_string(),
                        LogOutput::Console { message } => String::from_utf8_lossy(&message).to_string(),
                        LogOutput::StdIn { message } => String::from_utf8_lossy(&message).to_string(),
                    };
                    logs.push_str(&line);
                }
                Err(e) => {
                    if e.to_string().contains("No such container") {
                        return Err(ContainerError::NotFound(name.to_string()));
                    }
                    return Err(ContainerError::Connection(e));
                }
            }
        }

        Ok(logs)
    }

    /// Inspect a container and get detailed info.
    pub async fn inspect(&self, name: &str) -> Result<ContainerDetail, ContainerError> {
        self.verify_allowed(name)?;
        debug!("Inspecting container: {}", name);

        let inspect = self
            .docker
            .inspect_container(name, None::<InspectContainerOptions>)
            .await
            .map_err(|e| {
                if e.to_string().contains("No such container") {
                    ContainerError::NotFound(name.to_string())
                } else {
                    ContainerError::Connection(e)
                }
            })?;

        let id = inspect.id.unwrap_or_default();
        let short_id = if id.len() > 12 { &id[..12] } else { &id };

        let config = inspect.config.unwrap_or_default();
        let state = inspect.state.unwrap_or_default();
        let host_config = inspect.host_config.unwrap_or_default();
        let network_settings = inspect.network_settings.unwrap_or_default();

        let name = inspect
            .name
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_string();

        let status = state
            .status
            .as_ref()
            .map(|s| ContainerStatus::from_state(&s.to_string()))
            .unwrap_or(ContainerStatus::Unknown);
        let status_message = state.status.map(|s| s.to_string()).unwrap_or_default();

        // Get ports from network settings
        let ports = network_settings
            .ports
            .unwrap_or_default()
            .iter()
            .filter_map(|(container_port, bindings)| {
                bindings.as_ref().and_then(|b| {
                    b.first().map(|binding| {
                        format!(
                            "{}:{} -> {}",
                            binding.host_ip.as_deref().unwrap_or("0.0.0.0"),
                            binding.host_port.as_deref().unwrap_or("?"),
                            container_port
                        )
                    })
                })
            })
            .collect();

        // Get mounts
        let mounts = inspect
            .mounts
            .unwrap_or_default()
            .iter()
            .map(|m| {
                format!(
                    "{}:{}",
                    m.source.as_deref().unwrap_or("?"),
                    m.destination.as_deref().unwrap_or("?")
                )
            })
            .collect();

        // Get networks
        let networks = network_settings
            .networks
            .unwrap_or_default()
            .keys()
            .cloned()
            .collect();

        // Filter environment variables to hide secrets
        let env = config
            .env
            .unwrap_or_default()
            .into_iter()
            .map(|e| {
                if Self::is_secret_env(&e) {
                    let parts: Vec<&str> = e.splitn(2, '=').collect();
                    if parts.len() == 2 {
                        format!("{}=***", parts[0])
                    } else {
                        e
                    }
                } else {
                    e
                }
            })
            .collect();

        // Get resource limits
        let resources = ContainerResources {
            memory_limit: host_config.memory.unwrap_or(0) as u64,
            cpu_quota: host_config.cpu_quota.unwrap_or(0),
            cpu_count: host_config.nano_cpus.map(|n| n as f64 / 1_000_000_000.0).unwrap_or(0.0),
        };

        // Get health check status
        let health = state.health.and_then(|h| h.status.map(|s| s.to_string()));

        Ok(ContainerDetail {
            info: ContainerInfo {
                id: short_id.to_string(),
                name,
                image: config.image.unwrap_or_default(),
                status,
                status_message,
                ports,
                created: inspect.created.map(|s| {
                    chrono::DateTime::parse_from_rfc3339(&s)
                        .map(|dt| dt.timestamp())
                        .unwrap_or(0)
                }).unwrap_or(0),
            },
            command: config.cmd.unwrap_or_default().join(" "),
            env,
            mounts,
            networks,
            resources,
            health,
        })
    }

    /// Check if an environment variable likely contains a secret.
    fn is_secret_env(env: &str) -> bool {
        let lower = env.to_lowercase();
        lower.contains("password")
            || lower.contains("secret")
            || lower.contains("token")
            || lower.contains("key")
            || lower.contains("api_key")
            || lower.contains("apikey")
            || lower.contains("auth")
            || lower.contains("credential")
    }

    /// Format port bindings as readable strings.
    fn format_ports(ports: &[bollard::models::Port]) -> Vec<String> {
        ports
            .iter()
            .filter_map(|p| {
                let container_port = p.private_port;
                let host_port = p.public_port;
                let ip = p.ip.as_deref().unwrap_or("0.0.0.0");
                let proto = p.typ.as_ref().map(|t| t.to_string()).unwrap_or_else(|| "tcp".to_string());

                host_port.map(|hp| format!("{}:{}->{}/{}", ip, hp, container_port, proto))
            })
            .collect()
    }
}

/// Shared container manager with health integration.
pub struct ServerModule {
    manager: Option<ContainerManager>,
    config: ContainerConfig,
    last_error: Option<String>,
}

impl ServerModule {
    /// Create a new server module.
    pub fn new(config: ContainerConfig) -> Self {
        Self {
            manager: None,
            config,
            last_error: None,
        }
    }

    /// Initialize the container manager.
    pub async fn init(&mut self) -> Result<(), ContainerError> {
        match ContainerManager::new(self.config.clone()).await {
            Ok(manager) => {
                self.manager = Some(manager);
                self.last_error = None;
                Ok(())
            }
            Err(e) => {
                self.last_error = Some(e.to_string());
                Err(e)
            }
        }
    }

    /// Get a reference to the container manager.
    pub fn manager(&self) -> Option<&ContainerManager> {
        self.manager.as_ref()
    }

    /// Get the last error message.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Check if the module is connected.
    pub fn is_connected(&self) -> bool {
        self.manager.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_secret_env() {
        assert!(ContainerManager::is_secret_env("DB_PASSWORD=foo"));
        assert!(ContainerManager::is_secret_env("API_KEY=bar"));
        assert!(ContainerManager::is_secret_env("SECRET_TOKEN=baz"));
        assert!(ContainerManager::is_secret_env("AUTH_CREDENTIAL=qux"));
        assert!(!ContainerManager::is_secret_env("DATABASE_URL=localhost"));
        assert!(!ContainerManager::is_secret_env("PORT=8080"));
    }

    // Helper to test allowlist logic without requiring Docker
    fn is_allowed_by_list(name: &str, allowed: &[String]) -> bool {
        if allowed.is_empty() {
            return true;
        }
        allowed
            .iter()
            .any(|a| a == name || a == &format!("/{}", name))
    }

    #[test]
    fn test_allowlist_empty_allows_all() {
        let allowed: Vec<String> = vec![];
        assert!(is_allowed_by_list("any-container", &allowed));
        assert!(is_allowed_by_list("foo", &allowed));
    }

    #[test]
    fn test_allowlist_filters() {
        let allowed = vec!["allowed-1".to_string(), "allowed-2".to_string()];
        assert!(is_allowed_by_list("allowed-1", &allowed));
        assert!(is_allowed_by_list("allowed-2", &allowed));
        assert!(!is_allowed_by_list("not-allowed", &allowed));
    }

    #[test]
    fn test_allowlist_with_slash_prefix() {
        let allowed = vec!["/jellyfin".to_string()];
        assert!(is_allowed_by_list("jellyfin", &allowed));
    }

    #[test]
    fn test_server_module_initial_state() {
        let config = ContainerConfig::default();
        let module = ServerModule::new(config);
        assert!(!module.is_connected());
        assert!(module.last_error().is_none());
    }
}
