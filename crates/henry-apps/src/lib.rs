//! App deployment and management for Henry daemon.
//!
//! Provides functionality to deploy apps from GitHub repositories
//! to containers with automatic reverse proxy configuration.

use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::info;

pub mod builder;
pub mod deployer;
pub mod git;
pub mod types;

pub use types::{App, AppStatus, CreateAppRequest};

#[derive(Error, Debug)]
pub enum AppsError {
    #[error("git error: {0}")]
    Git(String),

    #[error("build error: {0}")]
    Build(String),

    #[error("deploy error: {0}")]
    Deploy(String),

    #[error("container error: {0}")]
    Container(#[from] henry_server::ContainerError),

    #[error("proxy error: {0}")]
    Proxy(#[from] henry_proxy::ProxyError),

    #[error("app not found: {0}")]
    NotFound(String),

    #[error("app already exists: {0}")]
    AlreadyExists(String),

    #[error("invalid repo URL: {0}")]
    InvalidRepoUrl(String),

    #[error("dockerfile generation failed: {0}")]
    DockerfileGeneration(String),

    #[error("state error: {0}")]
    State(#[from] henry_state::StateError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Apps manager for deploying and managing applications.
pub struct AppsManager {
    config: henry_config::AppsConfig,
    containers: Arc<RwLock<henry_server::ContainerManager>>,
    proxy: Option<Arc<RwLock<henry_proxy::ProxyManager>>>,
    claude: Option<Arc<RwLock<henry_claude::ClaudeManager>>>,
    state: Option<Arc<RwLock<henry_state::StateManager>>>,
    apps: Arc<RwLock<Vec<App>>>,
}

impl AppsManager {
    /// Create a new apps manager.
    pub fn new(
        config: henry_config::AppsConfig,
        containers: Arc<RwLock<henry_server::ContainerManager>>,
    ) -> Self {
        Self {
            config,
            containers,
            proxy: None,
            claude: None,
            state: None,
            apps: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Set the proxy manager.
    pub fn with_proxy(mut self, proxy: Arc<RwLock<henry_proxy::ProxyManager>>) -> Self {
        self.proxy = Some(proxy);
        self
    }

    /// Set the Claude manager (for Dockerfile generation).
    pub fn with_claude(mut self, claude: Arc<RwLock<henry_claude::ClaudeManager>>) -> Self {
        self.claude = Some(claude);
        self
    }

    /// Set the state manager for persistence.
    pub fn with_state(mut self, state: Arc<RwLock<henry_state::StateManager>>) -> Self {
        self.state = Some(state);
        self
    }

    /// Initialize the manager (load apps from state).
    pub async fn init(&mut self) -> Result<(), AppsError> {
        // Ensure workspace directory exists
        self.ensure_workspace_dir()?;

        // Load apps from state if available
        if let Some(ref state) = self.state {
            let state_mgr = state.read().await;
            let records = state_mgr.get_apps().await?;

            let mut apps = self.apps.write().await;
            for record in records {
                apps.push(App::from_record(record));
            }
            info!("Loaded {} apps from state", apps.len());
        }

        Ok(())
    }

    /// Ensure workspace directory exists.
    fn ensure_workspace_dir(&self) -> Result<(), AppsError> {
        if !self.config.workspace_dir.exists() {
            std::fs::create_dir_all(&self.config.workspace_dir)?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    &self.config.workspace_dir,
                    std::fs::Permissions::from_mode(0o700),
                )?;
            }
        }
        Ok(())
    }

    /// Deploy a new app.
    pub async fn deploy(&self, request: CreateAppRequest) -> Result<App, AppsError> {
        // Validate repo URL
        git::validate_repo_url(&request.repo_url)?;

        // Check if app already exists
        {
            let apps = self.apps.read().await;
            if apps.iter().any(|a| a.name == request.name) {
                return Err(AppsError::AlreadyExists(request.name));
            }
        }

        // Create app
        let mut app = App::new(
            &request.name,
            &request.repo_url,
            &request.domain,
            &request.path_prefix,
            self.config.default_port,
        );

        if let Some(branch) = &request.branch {
            app.branch = branch.clone();
        }

        if let Some(env_vars) = &request.env_vars {
            app.env_vars = env_vars.clone();
        }

        // Add to apps list
        {
            let mut apps = self.apps.write().await;
            apps.push(app.clone());
        }

        // Save to state
        self.save_app(&app).await?;

        // Clone repo
        app.status = AppStatus::Cloning;
        self.save_app(&app).await?;

        let app_dir = self.config.workspace_dir.join(&app.name);
        if let Err(e) = git::clone_repo(&request.repo_url, &app.branch, &app_dir).await {
            app.status = AppStatus::Failed(format!("Clone failed: {}", e));
            self.save_app(&app).await?;
            return Err(e);
        }

        // Detect or generate Dockerfile
        app.status = AppStatus::Building;
        self.save_app(&app).await?;

        let dockerfile_path = app_dir.join("Dockerfile");
        if !dockerfile_path.exists() {
            // Try to generate Dockerfile using Claude
            if let Some(ref claude) = self.claude {
                match builder::generate_dockerfile(&app_dir, claude.clone()).await {
                    Ok(dockerfile) => {
                        std::fs::write(&dockerfile_path, dockerfile)?;
                        info!("Generated Dockerfile for app: {}", app.name);
                    }
                    Err(e) => {
                        app.status = AppStatus::Failed(format!("Dockerfile generation failed: {}", e));
                        self.save_app(&app).await?;
                        return Err(AppsError::DockerfileGeneration(e.to_string()));
                    }
                }
            } else {
                app.status = AppStatus::Failed("No Dockerfile found and Claude not available".to_string());
                self.save_app(&app).await?;
                return Err(AppsError::Build("No Dockerfile found".to_string()));
            }
        }

        // Build image
        let containers = self.containers.read().await;
        if let Err(e) = builder::build_image(&app_dir, &app.image_name, &containers).await {
            app.status = AppStatus::Failed(format!("Build failed: {}", e));
            drop(containers);
            self.save_app(&app).await?;
            return Err(e);
        }
        drop(containers);

        // Deploy container
        app.status = AppStatus::Deploying;
        self.save_app(&app).await?;

        let containers = self.containers.read().await;
        if let Err(e) = deployer::deploy_container(&app, &containers, &self.config).await {
            app.status = AppStatus::Failed(format!("Deploy failed: {}", e));
            drop(containers);
            self.save_app(&app).await?;
            return Err(e);
        }
        drop(containers);

        // Create proxy route
        if let Some(ref proxy) = self.proxy {
            let route = henry_proxy::Route::new(&app.domain, &app.path_prefix, &format!("{}:{}", app.container_name, app.port))
                .with_app(&app.id);

            let proxy_mgr = proxy.read().await;
            if let Err(e) = proxy_mgr.add_route(route).await {
                info!("Warning: Failed to add proxy route: {}", e);
            }
        }

        // Mark as running
        app.status = AppStatus::Running;
        app.last_deployed = Some(chrono::Utc::now());
        self.save_app(&app).await?;

        // Update in apps list
        {
            let mut apps = self.apps.write().await;
            if let Some(existing) = apps.iter_mut().find(|a| a.id == app.id) {
                *existing = app.clone();
            }
        }

        info!("Deployed app: {} -> https://{}{}", app.name, app.domain, app.path_prefix);
        Ok(app)
    }

    /// Stop an app.
    pub async fn stop(&self, name: &str) -> Result<(), AppsError> {
        let app = self.get_app_by_name(name).await?;

        let containers = self.containers.read().await;
        containers.stop(&app.container_name).await.ok();

        // Update status
        let mut apps = self.apps.write().await;
        if let Some(existing) = apps.iter_mut().find(|a| a.name == name) {
            existing.status = AppStatus::Stopped;
        }

        if let Some(ref state) = self.state {
            let state_mgr = state.read().await;
            if let Some(mut record) = state_mgr.get_app(&app.id).await? {
                record.status = "stopped".to_string();
                record.updated_at = chrono::Utc::now().to_rfc3339();
                state_mgr.save_app(&record).await?;
            }
        }

        info!("Stopped app: {}", name);
        Ok(())
    }

    /// Start a stopped app.
    pub async fn start(&self, name: &str) -> Result<(), AppsError> {
        let app = self.get_app_by_name(name).await?;

        let containers = self.containers.read().await;
        containers.start(&app.container_name).await?;

        // Update status
        let mut apps = self.apps.write().await;
        if let Some(existing) = apps.iter_mut().find(|a| a.name == name) {
            existing.status = AppStatus::Running;
        }

        if let Some(ref state) = self.state {
            let state_mgr = state.read().await;
            if let Some(mut record) = state_mgr.get_app(&app.id).await? {
                record.status = "running".to_string();
                record.updated_at = chrono::Utc::now().to_rfc3339();
                state_mgr.save_app(&record).await?;
            }
        }

        info!("Started app: {}", name);
        Ok(())
    }

    /// Remove an app.
    pub async fn remove(&self, name: &str) -> Result<(), AppsError> {
        let app = self.get_app_by_name(name).await?;

        // Stop and remove container
        let containers = self.containers.read().await;
        containers.stop(&app.container_name).await.ok();
        // Note: Container removal would need to be added to ContainerManager
        drop(containers);

        // Remove proxy route
        if let Some(ref proxy) = self.proxy {
            let proxy_mgr = proxy.read().await;
            proxy_mgr.remove_routes_for_app(&app.id).await.ok();
        }

        // Remove from apps list
        {
            let mut apps = self.apps.write().await;
            apps.retain(|a| a.name != name);
        }

        // Delete from state
        if let Some(ref state) = self.state {
            let state_mgr = state.read().await;
            state_mgr.delete_app(&app.id).await?;
            state_mgr.delete_proxy_routes_by_app(&app.id).await?;
        }

        // Remove workspace directory
        let app_dir = self.config.workspace_dir.join(&app.name);
        if app_dir.exists() {
            std::fs::remove_dir_all(&app_dir).ok();
        }

        info!("Removed app: {}", name);
        Ok(())
    }

    /// Redeploy an app (pull latest and rebuild).
    pub async fn redeploy(&self, name: &str) -> Result<App, AppsError> {
        let app = self.get_app_by_name(name).await?;

        // Pull latest changes
        let app_dir = self.config.workspace_dir.join(&app.name);
        git::pull_repo(&app_dir).await?;

        // Rebuild
        let containers = self.containers.read().await;
        builder::build_image(&app_dir, &app.image_name, &containers).await?;
        drop(containers);

        // Restart container
        let containers = self.containers.read().await;
        containers.restart(&app.container_name).await?;

        // Update last_deployed
        let mut apps = self.apps.write().await;
        if let Some(existing) = apps.iter_mut().find(|a| a.name == name) {
            existing.last_deployed = Some(chrono::Utc::now());
            existing.status = AppStatus::Running;
        }

        let updated_app = apps.iter().find(|a| a.name == name).cloned();

        if let Some(ref app) = updated_app {
            self.save_app(app).await?;
        }

        info!("Redeployed app: {}", name);
        updated_app.ok_or_else(|| AppsError::NotFound(name.to_string()))
    }

    /// List all apps.
    pub async fn list(&self) -> Vec<App> {
        self.apps.read().await.clone()
    }

    /// Get an app by name.
    pub async fn get_app_by_name(&self, name: &str) -> Result<App, AppsError> {
        let apps = self.apps.read().await;
        apps.iter()
            .find(|a| a.name == name)
            .cloned()
            .ok_or_else(|| AppsError::NotFound(name.to_string()))
    }

    /// Get an app by ID.
    pub async fn get_app(&self, id: &str) -> Result<App, AppsError> {
        let apps = self.apps.read().await;
        apps.iter()
            .find(|a| a.id == id || a.id.starts_with(id))
            .cloned()
            .ok_or_else(|| AppsError::NotFound(id.to_string()))
    }

    /// Get app logs.
    pub async fn logs(&self, name: &str, tail: usize) -> Result<String, AppsError> {
        let app = self.get_app_by_name(name).await?;
        let containers = self.containers.read().await;
        containers.logs(&app.container_name, tail).await.map_err(AppsError::from)
    }

    /// Save app to state.
    async fn save_app(&self, app: &App) -> Result<(), AppsError> {
        if let Some(ref state) = self.state {
            let state_mgr = state.read().await;
            let record = app.to_record();
            state_mgr.save_app(&record).await?;
        }
        Ok(())
    }

    /// Get apps status summary.
    pub async fn status(&self) -> AppsStatus {
        let apps = self.apps.read().await;
        let running = apps.iter().filter(|a| matches!(a.status, AppStatus::Running)).count();
        let stopped = apps.iter().filter(|a| matches!(a.status, AppStatus::Stopped)).count();
        let failed = apps.iter().filter(|a| matches!(a.status, AppStatus::Failed(_))).count();

        AppsStatus {
            enabled: self.config.enabled,
            total_apps: apps.len(),
            running,
            stopped,
            failed,
            workspace_dir: self.config.workspace_dir.display().to_string(),
        }
    }
}

/// Apps status summary.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AppsStatus {
    pub enabled: bool,
    pub total_apps: usize,
    pub running: usize,
    pub stopped: usize,
    pub failed: usize,
    pub workspace_dir: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_creation() {
        let app = App::new("myapp", "https://github.com/user/repo", "example.com", "/myapp", 8080);
        assert_eq!(app.name, "myapp");
        assert_eq!(app.domain, "example.com");
        assert_eq!(app.path_prefix, "/myapp");
        assert!(matches!(app.status, AppStatus::Pending));
    }
}
