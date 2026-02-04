//! Health monitoring and system metrics for Henry daemon.
//!
//! Provides CPU, memory, and disk usage metrics, plus module health probes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use sysinfo::{Disks, System};
use thiserror::Error;
use tokio::sync::RwLock;

#[derive(Error, Debug)]
pub enum HealthError {
    #[error("probe failed: {0}")]
    ProbeFailed(String),

    #[error("module not found: {0}")]
    ModuleNotFound(String),
}

/// System metrics snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMetrics {
    /// CPU usage percentage (0-100).
    pub cpu_percent: f32,
    /// Memory used in bytes.
    pub memory_used: u64,
    /// Total memory in bytes.
    pub memory_total: u64,
    /// Memory usage percentage.
    pub memory_percent: f32,
    /// Disk usage by mount point.
    pub disks: Vec<DiskMetrics>,
    /// Timestamp of measurement.
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskMetrics {
    pub mount_point: String,
    pub total: u64,
    pub used: u64,
    pub available: u64,
    pub percent: f32,
}

/// Module health status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum HealthStatus {
    /// Module is healthy and operational.
    Healthy,
    /// Module is running but degraded.
    Degraded,
    /// Module is unhealthy.
    Unhealthy,
    /// Module health is unknown.
    Unknown,
}

impl std::fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HealthStatus::Healthy => write!(f, "healthy"),
            HealthStatus::Degraded => write!(f, "degraded"),
            HealthStatus::Unhealthy => write!(f, "unhealthy"),
            HealthStatus::Unknown => write!(f, "unknown"),
        }
    }
}

/// Module health report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleHealth {
    pub name: String,
    pub status: HealthStatus,
    pub enabled: bool,
    pub message: Option<String>,
    pub last_check: DateTime<Utc>,
    /// Optional metrics specific to the module.
    pub metrics: HashMap<String, String>,
}

impl ModuleHealth {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: HealthStatus::Unknown,
            enabled: true,
            message: None,
            last_check: Utc::now(),
            metrics: HashMap::new(),
        }
    }

    pub fn healthy(mut self) -> Self {
        self.status = HealthStatus::Healthy;
        self.last_check = Utc::now();
        self
    }

    pub fn unhealthy(mut self, message: impl Into<String>) -> Self {
        self.status = HealthStatus::Unhealthy;
        self.message = Some(message.into());
        self.last_check = Utc::now();
        self
    }

    pub fn degraded(mut self, message: impl Into<String>) -> Self {
        self.status = HealthStatus::Degraded;
        self.message = Some(message.into());
        self.last_check = Utc::now();
        self
    }

    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self.status = HealthStatus::Unknown;
        self
    }

    pub fn with_metric(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metrics.insert(key.into(), value.into());
        self
    }
}

/// Overall health snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthSnapshot {
    pub version: String,
    pub uptime_secs: u64,
    pub started_at: DateTime<Utc>,
    pub system: SystemMetrics,
    pub modules: Vec<ModuleHealth>,
    pub overall_status: HealthStatus,
}

/// Health manager for collecting and aggregating health data.
pub struct HealthManager {
    started_at: DateTime<Utc>,
    version: String,
    system: System,
    modules: Arc<RwLock<HashMap<String, ModuleHealth>>>,
}

impl HealthManager {
    /// Create a new health manager.
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            started_at: Utc::now(),
            version: version.into(),
            system: System::new_all(),
            modules: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get system metrics.
    pub fn get_system_metrics(&mut self) -> SystemMetrics {
        self.system.refresh_cpu_all();
        self.system.refresh_memory();

        let cpu_percent = self.system.global_cpu_usage();
        let memory_used = self.system.used_memory();
        let memory_total = self.system.total_memory();
        let memory_percent = if memory_total > 0 {
            (memory_used as f64 / memory_total as f64 * 100.0) as f32
        } else {
            0.0
        };

        let disks = Disks::new_with_refreshed_list();
        let disk_metrics: Vec<DiskMetrics> = disks
            .iter()
            .map(|d| {
                let total = d.total_space();
                let available = d.available_space();
                let used = total.saturating_sub(available);
                let percent = if total > 0 {
                    (used as f64 / total as f64 * 100.0) as f32
                } else {
                    0.0
                };
                DiskMetrics {
                    mount_point: d.mount_point().to_string_lossy().to_string(),
                    total,
                    used,
                    available,
                    percent,
                }
            })
            .collect();

        SystemMetrics {
            cpu_percent,
            memory_used,
            memory_total,
            memory_percent,
            disks: disk_metrics,
            timestamp: Utc::now(),
        }
    }

    /// Register a module for health tracking.
    pub async fn register_module(&self, name: impl Into<String>, enabled: bool) {
        let name = name.into();
        let mut health = ModuleHealth::new(&name);
        if !enabled {
            health = health.disabled();
        }
        self.modules.write().await.insert(name, health);
    }

    /// Update module health.
    pub async fn update_module(&self, health: ModuleHealth) {
        self.modules.write().await.insert(health.name.clone(), health);
    }

    /// Get module health.
    pub async fn get_module(&self, name: &str) -> Option<ModuleHealth> {
        self.modules.read().await.get(name).cloned()
    }

    /// Get all module health statuses.
    pub async fn get_all_modules(&self) -> Vec<ModuleHealth> {
        self.modules.read().await.values().cloned().collect()
    }

    /// Get a full health snapshot.
    pub async fn get_snapshot(&mut self) -> HealthSnapshot {
        let system = self.get_system_metrics();
        let modules = self.get_all_modules().await;

        // Calculate overall status
        let overall_status = Self::calculate_overall_status(&modules);

        let uptime = Utc::now()
            .signed_duration_since(self.started_at)
            .num_seconds() as u64;

        HealthSnapshot {
            version: self.version.clone(),
            uptime_secs: uptime,
            started_at: self.started_at,
            system,
            modules,
            overall_status,
        }
    }

    fn calculate_overall_status(modules: &[ModuleHealth]) -> HealthStatus {
        let enabled_modules: Vec<_> = modules.iter().filter(|m| m.enabled).collect();

        if enabled_modules.is_empty() {
            return HealthStatus::Healthy;
        }

        let any_unhealthy = enabled_modules
            .iter()
            .any(|m| m.status == HealthStatus::Unhealthy);
        let any_degraded = enabled_modules
            .iter()
            .any(|m| m.status == HealthStatus::Degraded);
        let any_unknown = enabled_modules
            .iter()
            .any(|m| m.status == HealthStatus::Unknown);

        if any_unhealthy {
            HealthStatus::Unhealthy
        } else if any_degraded {
            HealthStatus::Degraded
        } else if any_unknown {
            HealthStatus::Unknown
        } else {
            HealthStatus::Healthy
        }
    }

    /// Get daemon uptime.
    pub fn uptime(&self) -> Duration {
        let secs = Utc::now()
            .signed_duration_since(self.started_at)
            .num_seconds() as u64;
        Duration::from_secs(secs)
    }

    /// Get version.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Get start time.
    pub fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }
}

/// Format bytes as human-readable string.
pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;

    if bytes >= TB {
        format!("{:.1}TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.1}GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1}MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1}KB", bytes as f64 / KB as f64)
    } else {
        format!("{}B", bytes)
    }
}

/// Format duration as human-readable string.
pub fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;

    if days > 0 {
        format!("{}d {}h", days, hours)
    } else if hours > 0 {
        format!("{}h {}m", hours, mins)
    } else if mins > 0 {
        format!("{}m", mins)
    } else {
        format!("{}s", secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500B");
        assert_eq!(format_bytes(1500), "1.5KB");
        assert_eq!(format_bytes(1_500_000), "1.4MB");
        assert_eq!(format_bytes(1_500_000_000), "1.4GB");
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(Duration::from_secs(30)), "30s");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m");
        assert_eq!(format_duration(Duration::from_secs(3700)), "1h 1m");
        assert_eq!(format_duration(Duration::from_secs(90000)), "1d 1h");
    }

    #[tokio::test]
    async fn test_module_health() {
        let manager = HealthManager::new("0.1.0");

        manager.register_module("test", true).await;
        manager
            .update_module(ModuleHealth::new("test").healthy())
            .await;

        let health = manager.get_module("test").await.unwrap();
        assert_eq!(health.status, HealthStatus::Healthy);
    }

    #[tokio::test]
    async fn test_overall_status() {
        let manager = HealthManager::new("0.1.0");

        manager.register_module("mod1", true).await;
        manager.register_module("mod2", true).await;

        manager
            .update_module(ModuleHealth::new("mod1").healthy())
            .await;
        manager
            .update_module(ModuleHealth::new("mod2").degraded("slow"))
            .await;

        let modules = manager.get_all_modules().await;
        let status = HealthManager::calculate_overall_status(&modules);
        assert_eq!(status, HealthStatus::Degraded);
    }
}
