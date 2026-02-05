//! Type definitions for the apps module.

use chrono::{DateTime, Utc};
use henry_state::{AppRecord, AppServiceRecord};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// App status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AppStatus {
    Pending,
    Cloning,
    Building,
    Deploying,
    Running,
    Stopped,
    Failed(String),
}

impl AppStatus {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Pending => "pending",
            Self::Cloning => "cloning",
            Self::Building => "building",
            Self::Deploying => "deploying",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Failed(_) => "failed",
        }
    }

    pub fn from_str(s: &str, error_message: Option<&str>) -> Self {
        match s {
            "pending" => Self::Pending,
            "cloning" => Self::Cloning,
            "building" => Self::Building,
            "deploying" => Self::Deploying,
            "running" => Self::Running,
            "stopped" => Self::Stopped,
            "failed" => Self::Failed(error_message.unwrap_or("Unknown error").to_string()),
            _ => Self::Pending,
        }
    }
}

impl std::fmt::Display for AppStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(msg) => write!(f, "failed: {}", msg),
            _ => write!(f, "{}", self.as_str()),
        }
    }
}

/// Service type classification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ServiceType {
    Frontend,
    Backend,
    Worker,
    Other(String),
}

impl ServiceType {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Frontend => "frontend",
            Self::Backend => "backend",
            Self::Worker => "worker",
            Self::Other(s) => s.as_str(),
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "frontend" => Self::Frontend,
            "backend" => Self::Backend,
            "worker" => Self::Worker,
            other => Self::Other(other.to_string()),
        }
    }
}

impl std::fmt::Display for ServiceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A service within an app (e.g., "api", "webapp").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Service {
    pub id: String,
    pub name: String,
    pub service_type: ServiceType,
    /// Subdirectory relative to repo root (None = repo root).
    pub build_context: Option<String>,
    pub container_name: String,
    pub image_name: String,
    pub port: u16,
    pub env_vars: HashMap<String, String>,
    pub status: AppStatus,
    /// Proxy path for this service.
    pub path_prefix: String,
}

impl Service {
    pub fn new(
        app_name: &str,
        name: impl Into<String>,
        service_type: ServiceType,
        port: u16,
    ) -> Self {
        let name = name.into();
        let container_name = format!("henry-app-{}-{}", app_name, name);
        let image_name = format!("henry-app-{}-{}:latest", app_name, name);

        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            service_type,
            build_context: None,
            container_name,
            image_name,
            port,
            env_vars: HashMap::new(),
            status: AppStatus::Pending,
            path_prefix: "/".to_string(),
        }
    }

    pub fn with_build_context(mut self, ctx: impl Into<String>) -> Self {
        self.build_context = Some(ctx.into());
        self
    }

    pub fn with_path_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.path_prefix = prefix.into();
        self
    }

    pub fn with_env_vars(mut self, env_vars: HashMap<String, String>) -> Self {
        self.env_vars = env_vars;
        self
    }

    pub fn to_record(&self, app_id: &str) -> AppServiceRecord {
        let env_json = serde_json::to_string(&self.env_vars).ok();
        let (status, error_msg) = match &self.status {
            AppStatus::Failed(msg) => ("failed".to_string(), Some(msg.clone())),
            s => (s.as_str().to_string(), None),
        };

        AppServiceRecord {
            id: self.id.clone(),
            app_id: app_id.to_string(),
            name: self.name.clone(),
            service_type: self.service_type.as_str().to_string(),
            build_context: self.build_context.clone(),
            container_name: self.container_name.clone(),
            image_name: self.image_name.clone(),
            port: self.port as i32,
            env_vars: env_json,
            status,
            error_message: error_msg,
            path_prefix: self.path_prefix.clone(),
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        }
    }

    pub fn from_record(record: AppServiceRecord) -> Self {
        let env_vars: HashMap<String, String> = record
            .env_vars
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        let status = AppStatus::from_str(&record.status, record.error_message.as_deref());

        Self {
            id: record.id,
            name: record.name,
            service_type: ServiceType::from_str(&record.service_type),
            build_context: record.build_context,
            container_name: record.container_name,
            image_name: record.image_name,
            port: record.port as u16,
            env_vars,
            status,
            path_prefix: record.path_prefix,
        }
    }
}

/// An application managed by Henry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct App {
    /// Unique identifier.
    pub id: String,
    /// App name (used for container name, directory, etc.).
    pub name: String,
    /// Git repository URL.
    pub repo_url: String,
    /// Git branch to deploy.
    pub branch: String,
    /// Domain for the app.
    pub domain: String,
    /// Path prefix (e.g., "/myapp").
    pub path_prefix: String,
    /// Container name (primary service for backwards compat).
    pub container_name: String,
    /// Docker image name (primary service for backwards compat).
    pub image_name: String,
    /// Container port (primary service for backwards compat).
    pub port: u16,
    /// Environment variables.
    pub env_vars: HashMap<String, String>,
    /// Current status.
    pub status: AppStatus,
    /// When the app was created.
    pub created_at: DateTime<Utc>,
    /// When the app was last deployed.
    pub last_deployed: Option<DateTime<Utc>>,
    /// Services within this app.
    pub services: Vec<Service>,
}

impl App {
    /// Create a new single-service app (backwards compatible).
    pub fn new(
        name: impl Into<String>,
        repo_url: impl Into<String>,
        domain: impl Into<String>,
        path_prefix: impl Into<String>,
        port: u16,
    ) -> Self {
        let name = name.into();
        let container_name = format!("henry-app-{}", name);
        let image_name = format!("henry-app-{}:latest", name);

        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            repo_url: repo_url.into(),
            branch: "main".to_string(),
            domain: domain.into(),
            path_prefix: path_prefix.into(),
            container_name,
            image_name,
            port,
            env_vars: HashMap::new(),
            status: AppStatus::Pending,
            created_at: Utc::now(),
            last_deployed: None,
            services: Vec::new(),
        }
    }

    /// Create a new multi-service app.
    pub fn new_multi(
        name: impl Into<String>,
        repo_url: impl Into<String>,
        domain: impl Into<String>,
        path_prefix: impl Into<String>,
        services: Vec<Service>,
    ) -> Self {
        let name = name.into();
        // Primary service determines the top-level container/image/port
        let (container_name, image_name, port) = if let Some(primary) = services.first() {
            (
                primary.container_name.clone(),
                primary.image_name.clone(),
                primary.port,
            )
        } else {
            (
                format!("henry-app-{}", name),
                format!("henry-app-{}:latest", name),
                8080,
            )
        };

        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            repo_url: repo_url.into(),
            branch: "main".to_string(),
            domain: domain.into(),
            path_prefix: path_prefix.into(),
            container_name,
            image_name,
            port,
            env_vars: HashMap::new(),
            status: AppStatus::Pending,
            created_at: Utc::now(),
            last_deployed: None,
            services,
        }
    }

    /// Returns true if this is a multi-service app.
    pub fn is_multi_service(&self) -> bool {
        self.services.len() > 1
    }

    /// Get the primary service (first service, typically the frontend).
    pub fn primary_service(&self) -> Option<&Service> {
        self.services.first()
    }

    /// Get a service by name.
    pub fn service_by_name(&self, name: &str) -> Option<&Service> {
        self.services.iter().find(|s| s.name == name)
    }

    /// Get all container names for this app (services + legacy single-container).
    pub fn all_container_names(&self) -> Vec<String> {
        if self.services.is_empty() {
            vec![self.container_name.clone()]
        } else {
            self.services.iter().map(|s| s.container_name.clone()).collect()
        }
    }

    /// Set the git branch.
    pub fn with_branch(mut self, branch: impl Into<String>) -> Self {
        self.branch = branch.into();
        self
    }

    /// Add environment variables.
    pub fn with_env_vars(mut self, env_vars: HashMap<String, String>) -> Self {
        self.env_vars = env_vars;
        self
    }

    /// Get the full URL for this app.
    pub fn url(&self) -> String {
        if self.path_prefix == "/" {
            format!("https://{}", self.domain)
        } else {
            format!("https://{}{}", self.domain, self.path_prefix)
        }
    }

    /// Convert to a state record.
    pub fn to_record(&self) -> AppRecord {
        let env_json = serde_json::to_string(&self.env_vars).ok();
        let (status, error_msg) = match &self.status {
            AppStatus::Failed(msg) => ("failed".to_string(), Some(msg.clone())),
            s => (s.as_str().to_string(), None),
        };

        AppRecord {
            id: self.id.clone(),
            name: self.name.clone(),
            repo_url: self.repo_url.clone(),
            branch: self.branch.clone(),
            domain: self.domain.clone(),
            path_prefix: self.path_prefix.clone(),
            container_name: self.container_name.clone(),
            image_name: self.image_name.clone(),
            port: self.port as i32,
            env_vars: env_json,
            status,
            error_message: error_msg,
            created_at: self.created_at.to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
            last_deployed: self.last_deployed.map(|d| d.to_rfc3339()),
        }
    }

    /// Create from a state record (without services — caller must load services separately).
    pub fn from_record(record: AppRecord) -> Self {
        let env_vars: HashMap<String, String> = record
            .env_vars
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        let status = AppStatus::from_str(&record.status, record.error_message.as_deref());

        let created_at = chrono::DateTime::parse_from_rfc3339(&record.created_at)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());

        let last_deployed = record.last_deployed.and_then(|s| {
            chrono::DateTime::parse_from_rfc3339(&s)
                .map(|d| d.with_timezone(&Utc))
                .ok()
        });

        Self {
            id: record.id,
            name: record.name,
            repo_url: record.repo_url,
            branch: record.branch,
            domain: record.domain,
            path_prefix: record.path_prefix,
            container_name: record.container_name,
            image_name: record.image_name,
            port: record.port as u16,
            env_vars,
            status,
            created_at,
            last_deployed,
            services: Vec::new(),
        }
    }

    /// Create from a state record with service records.
    pub fn from_record_with_services(record: AppRecord, service_records: Vec<AppServiceRecord>) -> Self {
        let mut app = Self::from_record(record);
        app.services = service_records.into_iter().map(Service::from_record).collect();
        app
    }
}

/// Request to create a new app.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateAppRequest {
    /// App name.
    pub name: String,
    /// Git repository URL (HTTPS only).
    pub repo_url: String,
    /// Domain to deploy to.
    pub domain: String,
    /// Path prefix.
    #[serde(default = "default_path")]
    pub path_prefix: String,
    /// Git branch.
    pub branch: Option<String>,
    /// Environment variables.
    pub env_vars: Option<HashMap<String, String>>,
    /// Container port override.
    pub port: Option<u16>,
}

fn default_path() -> String {
    "/".to_string()
}

/// Service info for API responses.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceInfo {
    pub id: String,
    pub name: String,
    pub service_type: String,
    pub container_name: String,
    pub port: u16,
    pub status: String,
    pub path_prefix: String,
}

impl From<&Service> for ServiceInfo {
    fn from(svc: &Service) -> Self {
        Self {
            id: svc.id.clone(),
            name: svc.name.clone(),
            service_type: svc.service_type.as_str().to_string(),
            container_name: svc.container_name.clone(),
            port: svc.port,
            status: svc.status.to_string(),
            path_prefix: svc.path_prefix.clone(),
        }
    }
}

/// App info for API responses.
#[derive(Debug, Clone, Serialize)]
pub struct AppInfo {
    pub id: String,
    pub name: String,
    pub repo_url: String,
    pub branch: String,
    pub domain: String,
    pub path_prefix: String,
    pub url: String,
    pub status: String,
    pub port: u16,
    pub created_at: String,
    pub last_deployed: Option<String>,
    pub services: Vec<ServiceInfo>,
}

impl From<&App> for AppInfo {
    fn from(app: &App) -> Self {
        Self {
            id: app.id.clone(),
            name: app.name.clone(),
            repo_url: app.repo_url.clone(),
            branch: app.branch.clone(),
            domain: app.domain.clone(),
            path_prefix: app.path_prefix.clone(),
            url: app.url(),
            status: app.status.to_string(),
            port: app.port,
            created_at: app.created_at.to_rfc3339(),
            last_deployed: app.last_deployed.map(|d| d.to_rfc3339()),
            services: app.services.iter().map(ServiceInfo::from).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_status_conversion() {
        assert_eq!(AppStatus::Running.as_str(), "running");
        assert_eq!(AppStatus::from_str("running", None), AppStatus::Running);
        assert!(matches!(
            AppStatus::from_str("failed", Some("error")),
            AppStatus::Failed(_)
        ));
    }

    #[test]
    fn test_app_url() {
        let app = App::new("test", "https://github.com/user/repo", "example.com", "/app", 8080);
        assert_eq!(app.url(), "https://example.com/app");

        let root_app = App::new("test", "https://github.com/user/repo", "example.com", "/", 8080);
        assert_eq!(root_app.url(), "https://example.com");
    }

    #[test]
    fn test_service_type_roundtrip() {
        assert_eq!(ServiceType::from_str("frontend"), ServiceType::Frontend);
        assert_eq!(ServiceType::from_str("backend"), ServiceType::Backend);
        assert_eq!(ServiceType::from_str("worker"), ServiceType::Worker);
        assert_eq!(ServiceType::from_str("custom"), ServiceType::Other("custom".to_string()));
        assert_eq!(ServiceType::Frontend.as_str(), "frontend");
    }

    #[test]
    fn test_app_with_services() {
        let svc_api = Service::new("myapp", "api", ServiceType::Backend, 8000)
            .with_build_context("apps/api")
            .with_path_prefix("/api");
        let svc_web = Service::new("myapp", "webapp", ServiceType::Frontend, 3000)
            .with_build_context("apps/webapp")
            .with_path_prefix("/");

        let app = App::new_multi(
            "myapp",
            "https://github.com/user/repo",
            "example.com",
            "/app",
            vec![svc_api, svc_web],
        );

        assert!(app.is_multi_service());
        assert_eq!(app.services.len(), 2);
        assert!(app.service_by_name("api").is_some());
        assert!(app.service_by_name("webapp").is_some());
        assert!(app.service_by_name("nonexistent").is_none());
        assert_eq!(app.all_container_names().len(), 2);
    }

    #[test]
    fn test_backwards_compat_single_service() {
        let app = App::new("test", "https://github.com/user/repo", "example.com", "/", 8080);
        assert!(!app.is_multi_service());
        assert!(app.services.is_empty());
        assert_eq!(app.all_container_names(), vec!["henry-app-test".to_string()]);
        assert_eq!(app.container_name, "henry-app-test");
    }
}
