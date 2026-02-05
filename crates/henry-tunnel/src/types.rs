//! Type definitions for the tunnel module.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A tunnel route configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelRoute {
    /// Unique identifier.
    pub id: String,
    /// Hostname to expose (e.g., "myapp.example.com").
    pub hostname: String,
    /// Path prefix (optional).
    pub path: Option<String>,
    /// Backend service URL (e.g., "http://caddy:80").
    pub service: String,
    /// When this route was created.
    pub created_at: DateTime<Utc>,
}

impl TunnelRoute {
    /// Create a new tunnel route.
    pub fn new(hostname: impl Into<String>, service: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            hostname: hostname.into(),
            path: None,
            service: service.into(),
            created_at: Utc::now(),
        }
    }

    /// Set the path.
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Get the full URL pattern.
    pub fn url_pattern(&self) -> String {
        match &self.path {
            Some(p) => format!("{}{}", self.hostname, p),
            None => self.hostname.clone(),
        }
    }
}

/// Tunnel status information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelStatus {
    /// Whether the tunnel is enabled.
    pub enabled: bool,
    /// Whether the cloudflared container is running.
    pub running: bool,
    /// Name of the cloudflared container.
    pub container_name: String,
    /// Name/ID of the tunnel.
    pub tunnel_name: String,
    /// Number of configured routes.
    pub route_count: usize,
    /// Whether the Cloudflare API is configured.
    pub api_configured: bool,
}

/// Request to create a new tunnel route.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateTunnelRouteRequest {
    /// Hostname to expose.
    pub hostname: String,
    /// Path prefix (optional).
    pub path: Option<String>,
    /// Backend service URL.
    pub service: String,
}

impl CreateTunnelRouteRequest {
    /// Convert to a TunnelRoute.
    pub fn into_route(self) -> TunnelRoute {
        let mut route = TunnelRoute::new(self.hostname, self.service);
        if let Some(path) = self.path {
            route = route.with_path(path);
        }
        route
    }
}

/// Convert TunnelRoute to state record.
impl TunnelRoute {
    pub fn to_record(&self) -> henry_state::TunnelRouteRecord {
        henry_state::TunnelRouteRecord {
            id: self.id.clone(),
            hostname: self.hostname.clone(),
            path: self.path.clone(),
            service: self.service.clone(),
            created_at: self.created_at.to_rfc3339(),
        }
    }

    pub fn from_record(record: henry_state::TunnelRouteRecord) -> Self {
        Self {
            id: record.id,
            hostname: record.hostname,
            path: record.path,
            service: record.service,
            created_at: chrono::DateTime::parse_from_rfc3339(&record.created_at)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tunnel_route_url_pattern() {
        let route = TunnelRoute::new("example.com", "http://localhost:8080");
        assert_eq!(route.url_pattern(), "example.com");

        let route_with_path = TunnelRoute::new("example.com", "http://localhost:8080").with_path("/api");
        assert_eq!(route_with_path.url_pattern(), "example.com/api");
    }
}
