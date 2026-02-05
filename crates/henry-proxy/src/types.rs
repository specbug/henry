//! Type definitions for the proxy module.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A reverse proxy route configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    /// Unique identifier.
    pub id: String,
    /// Domain for this route (e.g., "sixeleven.in").
    pub domain: String,
    /// Path prefix to match (e.g., "/myapp").
    pub path_prefix: String,
    /// Upstream target (e.g., "henry-app-myapp:8080").
    pub upstream: String,
    /// Whether to strip the path prefix before forwarding.
    pub strip_prefix: bool,
    /// Whether TLS is enabled (HTTPS).
    pub tls_enabled: bool,
    /// Optional app ID this route belongs to.
    pub app_id: Option<String>,
    /// When this route was created.
    pub created_at: DateTime<Utc>,
}

impl Route {
    /// Create a new route with default settings.
    pub fn new(
        domain: impl Into<String>,
        path_prefix: impl Into<String>,
        upstream: impl Into<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            domain: domain.into(),
            path_prefix: path_prefix.into(),
            upstream: upstream.into(),
            strip_prefix: true,
            tls_enabled: true,
            app_id: None,
            created_at: Utc::now(),
        }
    }

    /// Set whether to strip the path prefix.
    pub fn with_strip_prefix(mut self, strip: bool) -> Self {
        self.strip_prefix = strip;
        self
    }

    /// Set TLS enabled status.
    pub fn with_tls(mut self, enabled: bool) -> Self {
        self.tls_enabled = enabled;
        self
    }

    /// Associate with an app.
    pub fn with_app(mut self, app_id: impl Into<String>) -> Self {
        self.app_id = Some(app_id.into());
        self
    }

    /// Get the full hostname pattern for this route.
    pub fn hostname_pattern(&self) -> String {
        self.domain.clone()
    }

    /// Get the full URL matcher for Caddy.
    pub fn url_matcher(&self) -> String {
        if self.path_prefix == "/" {
            format!("{}", self.domain)
        } else {
            format!("{}{}*", self.domain, self.path_prefix)
        }
    }
}

/// Proxy status information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyStatus {
    /// Whether the proxy is enabled.
    pub enabled: bool,
    /// Whether the Caddy container is running.
    pub running: bool,
    /// Name of the Caddy container.
    pub container_name: String,
    /// Number of configured routes.
    pub route_count: usize,
    /// Domains being served.
    pub domains: Vec<String>,
    /// HTTP port.
    pub http_port: u16,
    /// HTTPS port.
    pub https_port: u16,
}

/// Request to create a new route.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateRouteRequest {
    /// Domain for this route.
    pub domain: String,
    /// Path prefix to match.
    #[serde(default = "default_path")]
    pub path_prefix: String,
    /// Upstream target.
    pub upstream: String,
    /// Whether to strip the path prefix.
    #[serde(default = "default_strip")]
    pub strip_prefix: bool,
    /// Whether TLS is enabled.
    #[serde(default = "default_tls")]
    pub tls_enabled: bool,
}

fn default_path() -> String {
    "/".to_string()
}

fn default_strip() -> bool {
    true
}

fn default_tls() -> bool {
    true
}

impl CreateRouteRequest {
    /// Convert to a Route.
    pub fn into_route(self) -> Route {
        Route::new(self.domain, self.path_prefix, self.upstream)
            .with_strip_prefix(self.strip_prefix)
            .with_tls(self.tls_enabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_route_creation() {
        let route = Route::new("example.com", "/app", "localhost:8080");
        assert_eq!(route.domain, "example.com");
        assert_eq!(route.path_prefix, "/app");
        assert_eq!(route.upstream, "localhost:8080");
        assert!(route.strip_prefix);
        assert!(route.tls_enabled);
        assert!(route.app_id.is_none());
    }

    #[test]
    fn test_url_matcher() {
        let route = Route::new("example.com", "/app", "localhost:8080");
        assert_eq!(route.url_matcher(), "example.com/app*");

        let root_route = Route::new("example.com", "/", "localhost:8080");
        assert_eq!(root_route.url_matcher(), "example.com");
    }
}
