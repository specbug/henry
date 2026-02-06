//! App deployment and management for Henry daemon.
//!
//! Provides functionality to deploy apps from GitHub repositories
//! to containers with automatic reverse proxy configuration.
//! Supports both single-service and multi-service (monorepo) apps.

use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::info;

pub mod analyzer;
pub mod builder;
pub mod deployer;
pub mod git;
pub mod types;

pub use types::{App, AppInfo, AppStatus, CreateAppRequest, Service, ServiceInfo, ServiceType};

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

    /// Set the Claude manager (for Dockerfile generation and repo analysis).
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
                let app_id = record.id.clone();
                let service_records = state_mgr.get_app_services(&app_id).await?;
                apps.push(App::from_record_with_services(record, service_records));
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

    /// Deploy a new app (supports both single-service and multi-service).
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

        // Create initial app for status tracking
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

        // Analyze repo to discover services
        let manifest = analyzer::analyze_repo(&app_dir, &request.name, self.claude.clone()).await?;

        // Build services from manifest
        let mut services: Vec<Service> = manifest
            .services
            .iter()
            .map(|sm| {
                let svc_type = ServiceType::from_str(&sm.service_type);
                let mut svc = Service::new(&request.name, &sm.name, svc_type, sm.port);

                if sm.path != "." {
                    svc = svc.with_build_context(sm.path.clone());
                }

                // Assign path prefixes
                let svc_path = match sm.service_type.as_str() {
                    "frontend" => request.path_prefix.clone(),
                    "backend" => {
                        let base = request.path_prefix.trim_end_matches('/');
                        format!("{}/api", base)
                    }
                    _ => {
                        let base = request.path_prefix.trim_end_matches('/');
                        format!("{}/{}", base, sm.name)
                    }
                };
                svc = svc.with_path_prefix(svc_path);

                if !sm.env_vars.is_empty() {
                    svc = svc.with_env_vars(sm.env_vars.clone());
                }

                svc
            })
            .collect();

        // Ensure container network exists
        deployer::ensure_network(&self.config.network_name, self.config.runtime_cmd()).await?;

        // Build phase
        app.status = AppStatus::Building;
        self.save_app(&app).await?;

        // Sort services: backends first, then frontends (dependency order)
        services.sort_by_key(|s| match s.service_type {
            ServiceType::Backend => 0,
            ServiceType::Worker => 1,
            ServiceType::Frontend => 2,
            ServiceType::Other(_) => 3,
        });

        for svc in &mut services {
            let service_dir = svc.build_context.as_deref();
            let target_dir = match service_dir {
                Some(sd) => app_dir.join(sd),
                None => app_dir.clone(),
            };

            // Generate Dockerfile if needed
            let dockerfile_path = target_dir.join("Dockerfile");
            if !dockerfile_path.exists() {
                match builder::generate_dockerfile(&app_dir, service_dir, self.claude.clone()).await {
                    Ok(dockerfile) => {
                        std::fs::write(&dockerfile_path, dockerfile)?;
                        info!("Generated Dockerfile for service: {}", svc.name);
                    }
                    Err(e) => {
                        svc.status = AppStatus::Failed(format!("Dockerfile generation failed: {}", e));
                        app.status = AppStatus::Failed(format!("Dockerfile generation failed for service {}: {}", svc.name, e));
                        app.services = services;
                        self.save_app(&app).await?;
                        self.save_services(&app).await?;
                        return Err(AppsError::DockerfileGeneration(e.to_string()));
                    }
                }
            }

            // Compute build args (e.g., PUBLIC_URL for frontends)
            let mut build_args = HashMap::new();
            if svc.service_type == crate::types::ServiceType::Frontend {
                build_args.insert("PUBLIC_URL".to_string(), svc.path_prefix.clone());
            }

            // Build image
            let containers = self.containers.read().await;
            if let Err(e) = builder::build_image(&app_dir, service_dir, &svc.image_name, &containers, self.config.runtime_cmd(), &build_args).await {
                svc.status = AppStatus::Failed(format!("Build failed: {}", e));
                app.status = AppStatus::Failed(format!("Build failed for service {}: {}", svc.name, e));
                app.services = services;
                drop(containers);
                self.save_app(&app).await?;
                self.save_services(&app).await?;
                return Err(e);
            }
        }

        // Deploy phase
        app.status = AppStatus::Deploying;
        self.save_app(&app).await?;

        for i in 0..services.len() {
            // Compute inter-service env vars (needs immutable borrow of all services)
            let extra_env = compute_inter_service_env(&services, &services[i]);

            let svc = &mut services[i];

            if let Err(e) = deployer::deploy_service_container(
                &app.name,
                &app.id,
                svc,
                &extra_env,
                &self.config,
            )
            .await
            {
                svc.status = AppStatus::Failed(format!("Deploy failed: {}", e));
                app.status = AppStatus::Failed(format!("Deploy failed for service {}: {}", svc.name, e));
                app.services = services;
                self.save_app(&app).await?;
                self.save_services(&app).await?;
                return Err(e);
            }

            svc.status = AppStatus::Running;

            // Create proxy route for this service
            if let Some(ref proxy) = self.proxy {
                let upstream = format!("{}:{}", svc.container_name, svc.port);
                let route = henry_proxy::Route::new(&app.domain, &svc.path_prefix, &upstream)
                    .with_app(&app.id);

                let proxy_mgr = proxy.read().await;
                if let Err(e) = proxy_mgr.add_route(route).await {
                    info!("Warning: Failed to add proxy route for service {}: {}", svc.name, e);
                }
            }
        }

        // Mark as running
        app.status = AppStatus::Running;
        app.last_deployed = Some(chrono::Utc::now());
        app.services = services;

        // Update primary fields from first service for backwards compat
        if let Some(primary) = app.services.first() {
            let cn = primary.container_name.clone();
            let im = primary.image_name.clone();
            let pt = primary.port;
            app.container_name = cn;
            app.image_name = im;
            app.port = pt;
        }

        self.save_app(&app).await?;
        self.save_services(&app).await?;

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

    /// Stop an app (stops all services).
    pub async fn stop(&self, name: &str) -> Result<(), AppsError> {
        let app = self.get_app_by_name(name).await?;

        let containers = self.containers.read().await;
        for container_name in app.all_container_names() {
            containers.stop(&container_name).await.ok();
        }

        // Update status
        let mut apps = self.apps.write().await;
        if let Some(existing) = apps.iter_mut().find(|a| a.name == name) {
            existing.status = AppStatus::Stopped;
            for svc in &mut existing.services {
                svc.status = AppStatus::Stopped;
            }
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

    /// Start a stopped app (starts all services).
    pub async fn start(&self, name: &str) -> Result<(), AppsError> {
        let app = self.get_app_by_name(name).await?;

        let containers = self.containers.read().await;
        for container_name in app.all_container_names() {
            containers.start(&container_name).await?;
        }

        // Update status
        let mut apps = self.apps.write().await;
        if let Some(existing) = apps.iter_mut().find(|a| a.name == name) {
            existing.status = AppStatus::Running;
            for svc in &mut existing.services {
                svc.status = AppStatus::Running;
            }
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

    /// Remove an app (removes all service containers).
    pub async fn remove(&self, name: &str) -> Result<(), AppsError> {
        let app = self.get_app_by_name(name).await?;

        // Stop and remove all service containers
        let cmd = self.config.runtime_cmd();
        for container_name in app.all_container_names() {
            deployer::remove_container(&container_name, cmd).await.ok();
        }

        // Remove proxy routes
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
            state_mgr.delete_app_services(&app.id).await?;
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

    /// Redeploy an app (pull latest and rebuild all services).
    pub async fn redeploy(&self, name: &str) -> Result<App, AppsError> {
        let app = self.get_app_by_name(name).await?;
        let app_dir = self.config.workspace_dir.join(&app.name);

        // Pull latest changes
        git::pull_repo(&app_dir).await?;

        // Rebuild and restart each service
        let containers = self.containers.read().await;
        let cmd = self.config.runtime_cmd();
        let empty_args = HashMap::new();
        for svc in &app.services {
            let mut build_args = HashMap::new();
            if svc.service_type == crate::types::ServiceType::Frontend {
                build_args.insert("PUBLIC_URL".to_string(), svc.path_prefix.clone());
            }
            builder::build_image(&app_dir, svc.build_context.as_deref(), &svc.image_name, &containers, cmd, &build_args).await?;
            containers.restart(&svc.container_name).await?;
        }

        // If no services (legacy single-container), use old path
        if app.services.is_empty() {
            builder::build_image(&app_dir, None, &app.image_name, &containers, cmd, &empty_args).await?;
            containers.restart(&app.container_name).await?;
        }
        drop(containers);

        // Update last_deployed
        let mut apps = self.apps.write().await;
        if let Some(existing) = apps.iter_mut().find(|a| a.name == name) {
            existing.last_deployed = Some(chrono::Utc::now());
            existing.status = AppStatus::Running;
            for svc in &mut existing.services {
                svc.status = AppStatus::Running;
            }
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

    /// Get app logs (optionally for a specific service).
    pub async fn logs(&self, name: &str, tail: usize) -> Result<String, AppsError> {
        let app = self.get_app_by_name(name).await?;
        let containers = self.containers.read().await;
        containers.logs(&app.container_name, tail).await.map_err(AppsError::from)
    }

    /// Get logs for a specific service within an app.
    pub async fn service_logs(&self, name: &str, service_name: &str, tail: usize) -> Result<String, AppsError> {
        let app = self.get_app_by_name(name).await?;
        let svc = app
            .service_by_name(service_name)
            .ok_or_else(|| AppsError::NotFound(format!("service '{}' in app '{}'", service_name, name)))?;
        let containers = self.containers.read().await;
        containers.logs(&svc.container_name, tail).await.map_err(AppsError::from)
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

    /// Save app services to state.
    async fn save_services(&self, app: &App) -> Result<(), AppsError> {
        if let Some(ref state) = self.state {
            let state_mgr = state.read().await;
            for svc in &app.services {
                let record = svc.to_record(&app.id);
                state_mgr.save_app_service(&record).await?;
            }
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

/// Compute inter-service environment variables for a service.
/// Generates SERVICE_{NAME}_URL for each sibling, plus API_URL for frontends.
fn compute_inter_service_env(all_services: &[Service], current: &Service) -> HashMap<String, String> {
    let mut env = HashMap::new();

    for svc in all_services {
        if svc.name == current.name {
            continue;
        }
        let key = format!("SERVICE_{}_URL", svc.name.to_uppercase().replace('-', "_"));
        let value = format!("http://{}:{}", svc.container_name, svc.port);
        env.insert(key, value);
    }

    // For frontend services, add a convenient API_URL pointing to the first backend
    if current.service_type == ServiceType::Frontend {
        if let Some(backend) = all_services
            .iter()
            .find(|s| s.service_type == ServiceType::Backend)
        {
            env.insert(
                "API_URL".to_string(),
                format!("http://{}:{}", backend.container_name, backend.port),
            );
        }
    }

    env
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

    #[test]
    fn test_compute_inter_service_env() {
        let services = vec![
            Service::new("app", "api", ServiceType::Backend, 8000),
            Service::new("app", "webapp", ServiceType::Frontend, 3000),
        ];

        let env = compute_inter_service_env(&services, &services[1]);
        assert!(env.contains_key("SERVICE_API_URL"));
        assert!(env.contains_key("API_URL"));
        assert_eq!(env["SERVICE_API_URL"], "http://henry-app-app-api:8000");
        assert_eq!(env["API_URL"], "http://henry-app-app-api:8000");

        let env_backend = compute_inter_service_env(&services, &services[0]);
        assert!(env_backend.contains_key("SERVICE_WEBAPP_URL"));
        assert!(!env_backend.contains_key("API_URL"));
    }
}
