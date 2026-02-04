//! Container types for Henry server module.

use serde::Serialize;

/// Container status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ContainerStatus {
    Running,
    Paused,
    Restarting,
    Exited,
    Dead,
    Created,
    Removing,
    Unknown,
}

impl ContainerStatus {
    /// Parse status from Docker/Podman state string.
    pub fn from_state(state: &str) -> Self {
        match state.to_lowercase().as_str() {
            "running" => Self::Running,
            "paused" => Self::Paused,
            "restarting" => Self::Restarting,
            "exited" => Self::Exited,
            "dead" => Self::Dead,
            "created" => Self::Created,
            "removing" => Self::Removing,
            _ => Self::Unknown,
        }
    }

    /// Check if the container is running.
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }

    /// Get a display string for the status.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Restarting => "restarting",
            Self::Exited => "exited",
            Self::Dead => "dead",
            Self::Created => "created",
            Self::Removing => "removing",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for ContainerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Basic container information.
#[derive(Debug, Clone, Serialize)]
pub struct ContainerInfo {
    /// Container ID (short form).
    pub id: String,
    /// Container name.
    pub name: String,
    /// Image name.
    pub image: String,
    /// Current status.
    pub status: ContainerStatus,
    /// Status message (e.g., "Up 2 hours").
    pub status_message: String,
    /// Port mappings as "host:container" strings.
    pub ports: Vec<String>,
    /// Creation timestamp (Unix seconds).
    pub created: i64,
}

/// Detailed container information.
#[derive(Debug, Clone, Serialize)]
pub struct ContainerDetail {
    /// Basic info.
    #[serde(flatten)]
    pub info: ContainerInfo,
    /// Container command.
    pub command: String,
    /// Environment variables (filtered to hide secrets).
    pub env: Vec<String>,
    /// Volume mounts as "host:container" strings.
    pub mounts: Vec<String>,
    /// Network names.
    pub networks: Vec<String>,
    /// Resource limits.
    pub resources: ContainerResources,
    /// Health check status.
    pub health: Option<String>,
}

/// Container resource limits.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ContainerResources {
    /// Memory limit in bytes (0 = unlimited).
    pub memory_limit: u64,
    /// CPU quota (0 = unlimited).
    pub cpu_quota: i64,
    /// Number of CPUs (as floating point).
    pub cpu_count: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_container_status_from_state() {
        assert_eq!(ContainerStatus::from_state("running"), ContainerStatus::Running);
        assert_eq!(ContainerStatus::from_state("RUNNING"), ContainerStatus::Running);
        assert_eq!(ContainerStatus::from_state("exited"), ContainerStatus::Exited);
        assert_eq!(ContainerStatus::from_state("foo"), ContainerStatus::Unknown);
    }

    #[test]
    fn test_container_status_is_running() {
        assert!(ContainerStatus::Running.is_running());
        assert!(!ContainerStatus::Exited.is_running());
        assert!(!ContainerStatus::Unknown.is_running());
    }

    #[test]
    fn test_container_status_display() {
        assert_eq!(ContainerStatus::Running.to_string(), "running");
        assert_eq!(ContainerStatus::Exited.to_string(), "exited");
    }
}
