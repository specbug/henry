//! Anomaly observer that polls system state and detects issues.

use crate::types::{Anomaly, Severity, SystemEvent};
use henry_health::{HealthManager, HealthStatus};
use henry_server::ContainerManager;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// Threshold configuration for anomaly detection.
#[derive(Debug, Clone)]
pub struct ObserverThresholds {
    /// Disk usage warning threshold (percent).
    pub disk_warning_percent: f32,
    /// Disk usage critical threshold (percent).
    pub disk_critical_percent: f32,
    /// Memory usage warning threshold (percent).
    pub memory_warning_percent: f32,
    /// Memory usage critical threshold (percent).
    pub memory_critical_percent: f32,
    /// CPU usage warning threshold (percent).
    pub cpu_warning_percent: f32,
    /// Duration of high CPU to trigger alert (seconds).
    pub cpu_alert_duration_secs: u64,
}

impl Default for ObserverThresholds {
    fn default() -> Self {
        Self {
            disk_warning_percent: 80.0,
            disk_critical_percent: 95.0,
            memory_warning_percent: 85.0,
            memory_critical_percent: 95.0,
            cpu_warning_percent: 90.0,
            cpu_alert_duration_secs: 300,
        }
    }
}

/// State tracked across observation cycles.
#[derive(Debug, Default)]
struct ObserverState {
    /// Container states from last check (name -> running).
    last_container_states: HashMap<String, bool>,
    /// CPU high since timestamp (for duration tracking).
    cpu_high_since: Option<std::time::Instant>,
    /// Services that were unreachable (to avoid duplicate alerts).
    unreachable_services: HashMap<String, std::time::Instant>,
    /// Recently detected anomaly types (to avoid duplicates within cooldown).
    recent_anomalies: HashMap<String, std::time::Instant>,
}

impl ObserverState {
    /// Check if we've recently detected this type of anomaly.
    fn recently_detected(&self, key: &str, cooldown_secs: u64) -> bool {
        if let Some(last) = self.recent_anomalies.get(key) {
            last.elapsed().as_secs() < cooldown_secs
        } else {
            false
        }
    }

    /// Record that we detected an anomaly.
    fn record_detection(&mut self, key: impl Into<String>) {
        self.recent_anomalies.insert(key.into(), std::time::Instant::now());
    }

    /// Clean up old entries.
    fn cleanup(&mut self, max_age_secs: u64) {
        let now = std::time::Instant::now();
        self.recent_anomalies.retain(|_, v| now.duration_since(*v).as_secs() < max_age_secs);
        self.unreachable_services.retain(|_, v| now.duration_since(*v).as_secs() < max_age_secs);
    }
}

/// The anomaly observer that polls system state.
pub struct AnomalyObserver {
    /// Health manager for system metrics and module health.
    health: Arc<RwLock<HealthManager>>,
    /// Optional container manager for container state.
    containers: Option<Arc<RwLock<ContainerManager>>>,
    /// Detection thresholds.
    thresholds: ObserverThresholds,
    /// Internal state.
    state: ObserverState,
    /// Cooldown for duplicate anomaly detection (seconds).
    duplicate_cooldown_secs: u64,
}

impl AnomalyObserver {
    /// Create a new observer.
    pub fn new(
        health: Arc<RwLock<HealthManager>>,
        containers: Option<Arc<RwLock<ContainerManager>>>,
        thresholds: ObserverThresholds,
    ) -> Self {
        Self {
            health,
            containers,
            thresholds,
            state: ObserverState::default(),
            duplicate_cooldown_secs: 300, // 5 minutes
        }
    }

    /// Set duplicate detection cooldown.
    pub fn set_duplicate_cooldown(&mut self, secs: u64) {
        self.duplicate_cooldown_secs = secs;
    }

    /// Run a single observation cycle and return detected anomalies.
    pub async fn observe(&mut self) -> Vec<Anomaly> {
        let mut anomalies = Vec::new();

        // Cleanup old state
        self.state.cleanup(self.duplicate_cooldown_secs * 2);

        // Check system metrics
        anomalies.extend(self.check_system_metrics().await);

        // Check container states
        anomalies.extend(self.check_containers().await);

        // Check module health
        anomalies.extend(self.check_modules().await);

        anomalies
    }

    /// Check system metrics (CPU, memory, disk).
    async fn check_system_metrics(&mut self) -> Vec<Anomaly> {
        let mut anomalies = Vec::new();

        let mut health = self.health.write().await;
        let metrics = health.get_system_metrics();
        drop(health);

        // Check memory
        if metrics.memory_percent >= self.thresholds.memory_critical_percent {
            let key = "memory_critical";
            if !self.state.recently_detected(key, self.duplicate_cooldown_secs) {
                self.state.record_detection(key);
                anomalies.push(Anomaly::with_severity(
                    SystemEvent::MemoryPressure {
                        percent: metrics.memory_percent,
                        used_bytes: metrics.memory_used,
                        total_bytes: metrics.memory_total,
                    },
                    Severity::Critical,
                ));
            }
        } else if metrics.memory_percent >= self.thresholds.memory_warning_percent {
            let key = "memory_warning";
            if !self.state.recently_detected(key, self.duplicate_cooldown_secs) {
                self.state.record_detection(key);
                anomalies.push(Anomaly::new(SystemEvent::MemoryPressure {
                    percent: metrics.memory_percent,
                    used_bytes: metrics.memory_used,
                    total_bytes: metrics.memory_total,
                }));
            }
        }

        // Check disks
        for disk in &metrics.disks {
            let key = format!("disk_{}", disk.mount_point);
            if disk.percent >= self.thresholds.disk_critical_percent {
                if !self.state.recently_detected(&key, self.duplicate_cooldown_secs) {
                    self.state.record_detection(&key);
                    anomalies.push(Anomaly::with_severity(
                        SystemEvent::DiskSpaceLow {
                            mount_point: disk.mount_point.clone(),
                            percent: disk.percent,
                            available_bytes: disk.available,
                        },
                        Severity::Critical,
                    ));
                }
            } else if disk.percent >= self.thresholds.disk_warning_percent {
                if !self.state.recently_detected(&key, self.duplicate_cooldown_secs) {
                    self.state.record_detection(&key);
                    anomalies.push(Anomaly::new(SystemEvent::DiskSpaceLow {
                        mount_point: disk.mount_point.clone(),
                        percent: disk.percent,
                        available_bytes: disk.available,
                    }));
                }
            }
        }

        // Check CPU (with duration tracking)
        if metrics.cpu_percent >= self.thresholds.cpu_warning_percent {
            match self.state.cpu_high_since {
                Some(since) => {
                    let duration = since.elapsed().as_secs();
                    if duration >= self.thresholds.cpu_alert_duration_secs {
                        let key = "cpu_high";
                        if !self.state.recently_detected(key, self.duplicate_cooldown_secs) {
                            self.state.record_detection(key);
                            anomalies.push(Anomaly::new(SystemEvent::HighCpuUsage {
                                percent: metrics.cpu_percent,
                                duration_secs: duration,
                            }));
                        }
                    }
                }
                None => {
                    self.state.cpu_high_since = Some(std::time::Instant::now());
                }
            }
        } else {
            self.state.cpu_high_since = None;
        }

        anomalies
    }

    /// Check container states for crashes.
    async fn check_containers(&mut self) -> Vec<Anomaly> {
        let mut anomalies = Vec::new();

        let Some(ref containers) = self.containers else {
            return anomalies;
        };

        let manager = containers.read().await;
        let Ok(container_list) = manager.list(true).await else {
            debug!("Failed to list containers");
            return anomalies;
        };
        drop(manager);

        // Build current state
        let mut current_states: HashMap<String, bool> = HashMap::new();
        for container in &container_list {
            let is_running = container.status == henry_server::ContainerStatus::Running;
            current_states.insert(container.name.clone(), is_running);
        }

        // Compare with last state to detect crashes
        for (name, &is_running) in &current_states {
            let was_running = self.state.last_container_states.get(name).copied().unwrap_or(false);

            // Container was running and now it's not
            if was_running && !is_running {
                let key = format!("container_crashed_{}", name);
                if !self.state.recently_detected(&key, self.duplicate_cooldown_secs) {
                    self.state.record_detection(&key);
                    warn!("Container '{}' crashed", name);
                    anomalies.push(Anomaly::new(SystemEvent::ContainerCrashed {
                        name: name.clone(),
                        exit_code: None, // Exit code not available from basic listing
                    }));
                }
            }
        }

        // Update last states
        self.state.last_container_states = current_states;

        anomalies
    }

    /// Check module health.
    async fn check_modules(&mut self) -> Vec<Anomaly> {
        let mut anomalies = Vec::new();

        let health = self.health.read().await;
        let modules = health.get_all_modules().await;
        drop(health);

        for module in modules {
            if !module.enabled {
                continue;
            }

            if module.status == HealthStatus::Unhealthy {
                let key = format!("module_unhealthy_{}", module.name);
                if !self.state.recently_detected(&key, self.duplicate_cooldown_secs) {
                    self.state.record_detection(&key);
                    anomalies.push(Anomaly::new(SystemEvent::ModuleUnhealthy {
                        module: module.name.clone(),
                        message: module.message.clone(),
                    }));
                }
            }
        }

        anomalies
    }

    /// Get the current thresholds.
    pub fn thresholds(&self) -> &ObserverThresholds {
        &self.thresholds
    }

    /// Update thresholds.
    pub fn set_thresholds(&mut self, thresholds: ObserverThresholds) {
        self.thresholds = thresholds;
    }

    /// Record a service as unreachable (for external callers).
    pub fn record_service_unreachable(&mut self, service: &str, url: &str, error: &str) -> Option<Anomaly> {
        let key = format!("service_unreachable_{}", service);
        if !self.state.recently_detected(&key, self.duplicate_cooldown_secs) {
            self.state.record_detection(&key);
            self.state.unreachable_services.insert(service.to_string(), std::time::Instant::now());
            Some(Anomaly::new(SystemEvent::ServiceUnreachable {
                service: service.to_string(),
                url: url.to_string(),
                error: error.to_string(),
            }))
        } else {
            None
        }
    }

    /// Record a backup failure.
    pub fn record_backup_failed(&mut self, error: &str) -> Option<Anomaly> {
        let key = "backup_failed";
        if !self.state.recently_detected(key, self.duplicate_cooldown_secs) {
            self.state.record_detection(key);
            Some(Anomaly::new(SystemEvent::BackupFailed {
                error: error.to_string(),
            }))
        } else {
            None
        }
    }

    /// Record an update available.
    pub fn record_update_available(&mut self, current: &str, latest: &str) -> Option<Anomaly> {
        let key = format!("update_available_{}", latest);
        if !self.state.recently_detected(&key, 86400) {
            // 24 hour cooldown for updates
            self.state.record_detection(&key);
            Some(Anomaly::new(SystemEvent::UpdateAvailable {
                current: current.to_string(),
                latest: latest.to_string(),
            }))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_observer_creation() {
        let health = Arc::new(RwLock::new(HealthManager::new("0.1.0")));
        let thresholds = ObserverThresholds::default();
        let observer = AnomalyObserver::new(health, None, thresholds);

        assert_eq!(observer.thresholds.disk_warning_percent, 80.0);
    }

    #[test]
    fn test_observer_state_cooldown() {
        let mut state = ObserverState::default();

        assert!(!state.recently_detected("test", 60));

        state.record_detection("test");
        assert!(state.recently_detected("test", 60));
    }

    #[tokio::test]
    async fn test_record_service_unreachable() {
        let health = Arc::new(RwLock::new(HealthManager::new("0.1.0")));
        let thresholds = ObserverThresholds::default();
        let mut observer = AnomalyObserver::new(health, None, thresholds);

        // First call should return an anomaly
        let anomaly = observer.record_service_unreachable("test", "http://localhost", "connection refused");
        assert!(anomaly.is_some());

        // Second call within cooldown should return None
        let anomaly = observer.record_service_unreachable("test", "http://localhost", "connection refused");
        assert!(anomaly.is_none());
    }
}
