//! Core types for the reactive control loop.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Severity levels for anomalies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Severity {
    /// Informational event, may not require action.
    Info,
    /// Warning level, should be monitored.
    Warning,
    /// Critical issue requiring immediate attention.
    Critical,
    /// Emergency situation, system may be at risk.
    Emergency,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Info => write!(f, "info"),
            Severity::Warning => write!(f, "warning"),
            Severity::Critical => write!(f, "critical"),
            Severity::Emergency => write!(f, "emergency"),
        }
    }
}

impl std::str::FromStr for Severity {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "info" => Ok(Severity::Info),
            "warning" | "warn" => Ok(Severity::Warning),
            "critical" | "crit" => Ok(Severity::Critical),
            "emergency" | "emerg" => Ok(Severity::Emergency),
            _ => Err(format!("unknown severity: {}", s)),
        }
    }
}

/// System events that the reactive loop can detect.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SystemEvent {
    /// A container has crashed or exited unexpectedly.
    ContainerCrashed {
        name: String,
        exit_code: Option<i64>,
    },
    /// Disk space is running low.
    DiskSpaceLow {
        mount_point: String,
        percent: f32,
        available_bytes: u64,
    },
    /// A service is unreachable.
    ServiceUnreachable {
        service: String,
        url: String,
        error: String,
    },
    /// Memory usage is high.
    MemoryPressure {
        percent: f32,
        used_bytes: u64,
        total_bytes: u64,
    },
    /// A backup operation failed.
    BackupFailed { error: String },
    /// An update is available.
    UpdateAvailable { current: String, latest: String },
    /// A container has been restarted too many times.
    ContainerRestartLoop {
        name: String,
        restart_count: u32,
        window_secs: u64,
    },
    /// CPU usage is consistently high.
    HighCpuUsage {
        percent: f32,
        duration_secs: u64,
    },
    /// A module health check failed.
    ModuleUnhealthy {
        module: String,
        message: Option<String>,
    },
}

impl SystemEvent {
    /// Get the event type as a string for pattern matching.
    pub fn event_type(&self) -> &'static str {
        match self {
            SystemEvent::ContainerCrashed { .. } => "container_crashed",
            SystemEvent::DiskSpaceLow { .. } => "disk_space_low",
            SystemEvent::ServiceUnreachable { .. } => "service_unreachable",
            SystemEvent::MemoryPressure { .. } => "memory_pressure",
            SystemEvent::BackupFailed { .. } => "backup_failed",
            SystemEvent::UpdateAvailable { .. } => "update_available",
            SystemEvent::ContainerRestartLoop { .. } => "container_restart_loop",
            SystemEvent::HighCpuUsage { .. } => "high_cpu_usage",
            SystemEvent::ModuleUnhealthy { .. } => "module_unhealthy",
        }
    }

    /// Get the default severity for this event type.
    pub fn default_severity(&self) -> Severity {
        match self {
            SystemEvent::ContainerCrashed { .. } => Severity::Critical,
            SystemEvent::DiskSpaceLow { percent, .. } => {
                if *percent >= 95.0 {
                    Severity::Emergency
                } else if *percent >= 90.0 {
                    Severity::Critical
                } else {
                    Severity::Warning
                }
            }
            SystemEvent::ServiceUnreachable { .. } => Severity::Critical,
            SystemEvent::MemoryPressure { percent, .. } => {
                if *percent >= 95.0 {
                    Severity::Emergency
                } else if *percent >= 90.0 {
                    Severity::Critical
                } else {
                    Severity::Warning
                }
            }
            SystemEvent::BackupFailed { .. } => Severity::Warning,
            SystemEvent::UpdateAvailable { .. } => Severity::Info,
            SystemEvent::ContainerRestartLoop { .. } => Severity::Critical,
            SystemEvent::HighCpuUsage { .. } => Severity::Warning,
            SystemEvent::ModuleUnhealthy { .. } => Severity::Warning,
        }
    }

    /// Get a human-readable description of the event.
    pub fn description(&self) -> String {
        match self {
            SystemEvent::ContainerCrashed { name, exit_code } => {
                match exit_code {
                    Some(code) => format!("Container '{}' crashed with exit code {}", name, code),
                    None => format!("Container '{}' crashed", name),
                }
            }
            SystemEvent::DiskSpaceLow { mount_point, percent, .. } => {
                format!("Disk space low on '{}' ({:.1}% used)", mount_point, percent)
            }
            SystemEvent::ServiceUnreachable { service, url, error } => {
                format!("Service '{}' unreachable at {}: {}", service, url, error)
            }
            SystemEvent::MemoryPressure { percent, .. } => {
                format!("Memory pressure high ({:.1}% used)", percent)
            }
            SystemEvent::BackupFailed { error } => {
                format!("Backup failed: {}", error)
            }
            SystemEvent::UpdateAvailable { current, latest } => {
                format!("Update available: {} -> {}", current, latest)
            }
            SystemEvent::ContainerRestartLoop { name, restart_count, .. } => {
                format!("Container '{}' in restart loop ({} restarts)", name, restart_count)
            }
            SystemEvent::HighCpuUsage { percent, duration_secs } => {
                format!("High CPU usage ({:.1}% for {}s)", percent, duration_secs)
            }
            SystemEvent::ModuleUnhealthy { module, message } => {
                match message {
                    Some(msg) => format!("Module '{}' unhealthy: {}", module, msg),
                    None => format!("Module '{}' unhealthy", module),
                }
            }
        }
    }
}

/// An anomaly detected by the observer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Anomaly {
    /// Unique identifier.
    pub id: String,
    /// The event that triggered this anomaly.
    pub event: SystemEvent,
    /// Severity of the anomaly.
    pub severity: Severity,
    /// When the anomaly was detected.
    pub detected_at: DateTime<Utc>,
    /// When the anomaly was resolved (if resolved).
    pub resolved_at: Option<DateTime<Utc>>,
    /// What resolved the anomaly (action ID or "manual").
    pub resolved_by: Option<String>,
    /// Suggested actions for this anomaly.
    pub suggested_actions: Vec<ActionType>,
}

impl Anomaly {
    /// Create a new anomaly from a system event.
    pub fn new(event: SystemEvent) -> Self {
        let severity = event.default_severity();
        let suggested_actions = Self::suggest_actions(&event);
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            event,
            severity,
            detected_at: Utc::now(),
            resolved_at: None,
            resolved_by: None,
            suggested_actions,
        }
    }

    /// Create a new anomaly with custom severity.
    pub fn with_severity(event: SystemEvent, severity: Severity) -> Self {
        let suggested_actions = Self::suggest_actions(&event);
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            event,
            severity,
            detected_at: Utc::now(),
            resolved_at: None,
            resolved_by: None,
            suggested_actions,
        }
    }

    /// Mark the anomaly as resolved.
    pub fn resolve(&mut self, resolved_by: impl Into<String>) {
        self.resolved_at = Some(Utc::now());
        self.resolved_by = Some(resolved_by.into());
    }

    /// Check if the anomaly is resolved.
    pub fn is_resolved(&self) -> bool {
        self.resolved_at.is_some()
    }

    /// Suggest actions based on the event type.
    fn suggest_actions(event: &SystemEvent) -> Vec<ActionType> {
        match event {
            SystemEvent::ContainerCrashed { name, .. } => {
                vec![ActionType::RestartContainer { name: name.clone() }]
            }
            SystemEvent::DiskSpaceLow { .. } => {
                vec![ActionType::RotateLogs]
            }
            SystemEvent::ServiceUnreachable { service, .. } => {
                vec![
                    ActionType::RestartContainer { name: service.clone() },
                    ActionType::NotifyUser {
                        message: format!("Service '{}' is unreachable", service),
                        urgency: NotifyUrgency::High,
                    },
                ]
            }
            SystemEvent::MemoryPressure { .. } => {
                vec![ActionType::NotifyUser {
                    message: "High memory pressure detected".to_string(),
                    urgency: NotifyUrgency::Medium,
                }]
            }
            SystemEvent::BackupFailed { .. } => {
                vec![ActionType::TriggerBackup]
            }
            SystemEvent::UpdateAvailable { .. } => {
                vec![ActionType::SelfUpdate]
            }
            SystemEvent::ContainerRestartLoop { name, .. } => {
                vec![ActionType::EscalateToHuman {
                    reason: format!("Container '{}' is in a restart loop", name),
                }]
            }
            SystemEvent::HighCpuUsage { .. } => {
                vec![ActionType::NotifyUser {
                    message: "High CPU usage detected".to_string(),
                    urgency: NotifyUrgency::Low,
                }]
            }
            SystemEvent::ModuleUnhealthy { module, .. } => {
                vec![ActionType::NotifyUser {
                    message: format!("Module '{}' is unhealthy", module),
                    urgency: NotifyUrgency::Medium,
                }]
            }
        }
    }
}

/// Types of actions that can be taken in response to anomalies.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionType {
    /// Restart a container.
    RestartContainer { name: String },
    /// Rotate and clean up logs.
    RotateLogs,
    /// Trigger a backup.
    TriggerBackup,
    /// Notify the user via configured channels.
    NotifyUser { message: String, urgency: NotifyUrgency },
    /// Escalate to human intervention.
    EscalateToHuman { reason: String },
    /// Run a custom shell command.
    RunCommand {
        command: String,
        args: Vec<String>,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },
    /// Self-update Henry (pull, build, restart).
    SelfUpdate,
}

fn default_timeout() -> u64 {
    60
}

impl ActionType {
    /// Get the action type as a string.
    pub fn action_type(&self) -> &'static str {
        match self {
            ActionType::RestartContainer { .. } => "restart_container",
            ActionType::RotateLogs => "rotate_logs",
            ActionType::TriggerBackup => "trigger_backup",
            ActionType::NotifyUser { .. } => "notify_user",
            ActionType::EscalateToHuman { .. } => "escalate_to_human",
            ActionType::RunCommand { .. } => "run_command",
            ActionType::SelfUpdate => "self_update",
        }
    }

    /// Get a human-readable description of the action.
    pub fn description(&self) -> String {
        match self {
            ActionType::RestartContainer { name } => format!("Restart container '{}'", name),
            ActionType::RotateLogs => "Rotate and clean up logs".to_string(),
            ActionType::TriggerBackup => "Trigger a backup".to_string(),
            ActionType::NotifyUser { message, .. } => format!("Notify user: {}", message),
            ActionType::EscalateToHuman { reason } => format!("Escalate to human: {}", reason),
            ActionType::RunCommand { command, args, .. } => {
                format!("Run command: {} {}", command, args.join(" "))
            }
            ActionType::SelfUpdate => "Self-update Henry".to_string(),
        }
    }
}

/// Urgency level for notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyUrgency {
    Low,
    Medium,
    High,
}

/// Status of an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    /// Action is pending execution.
    Pending,
    /// Action is waiting for human approval.
    AwaitingApproval,
    /// Action is currently executing.
    Executing,
    /// Action completed, verifying results.
    Verifying,
    /// Action completed successfully.
    Completed,
    /// Action failed.
    Failed,
    /// Action was cancelled.
    Cancelled,
}

impl std::fmt::Display for ActionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionStatus::Pending => write!(f, "pending"),
            ActionStatus::AwaitingApproval => write!(f, "awaiting_approval"),
            ActionStatus::Executing => write!(f, "executing"),
            ActionStatus::Verifying => write!(f, "verifying"),
            ActionStatus::Completed => write!(f, "completed"),
            ActionStatus::Failed => write!(f, "failed"),
            ActionStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::str::FromStr for ActionStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "pending" => Ok(ActionStatus::Pending),
            "awaiting_approval" => Ok(ActionStatus::AwaitingApproval),
            "executing" => Ok(ActionStatus::Executing),
            "verifying" => Ok(ActionStatus::Verifying),
            "completed" => Ok(ActionStatus::Completed),
            "failed" => Ok(ActionStatus::Failed),
            "cancelled" => Ok(ActionStatus::Cancelled),
            _ => Err(format!("unknown action status: {}", s)),
        }
    }
}

/// A recorded action taken (or to be taken) in response to an anomaly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    /// Unique identifier.
    pub id: String,
    /// The anomaly that triggered this action.
    pub anomaly_id: String,
    /// The type of action.
    pub action_type: ActionType,
    /// When the action was created.
    pub created_at: DateTime<Utc>,
    /// Whether the action requires human approval.
    pub requires_approval: bool,
    /// Who approved the action (if applicable).
    pub approved_by: Option<String>,
    /// When approval was given.
    pub approved_at: Option<DateTime<Utc>>,
    /// When the action started executing.
    pub executed_at: Option<DateTime<Utc>>,
    /// Current status of the action.
    pub status: ActionStatus,
    /// Result message (success or error).
    pub result: Option<String>,
    /// Number of retries attempted.
    pub retries: u32,
    /// Maximum retries allowed.
    pub max_retries: u32,
}

impl Action {
    /// Create a new action.
    pub fn new(anomaly_id: impl Into<String>, action_type: ActionType, requires_approval: bool) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            anomaly_id: anomaly_id.into(),
            action_type,
            created_at: Utc::now(),
            requires_approval,
            approved_by: None,
            approved_at: None,
            executed_at: None,
            status: if requires_approval {
                ActionStatus::AwaitingApproval
            } else {
                ActionStatus::Pending
            },
            result: None,
            retries: 0,
            max_retries: 3,
        }
    }

    /// Approve the action.
    pub fn approve(&mut self, approved_by: impl Into<String>) {
        self.approved_by = Some(approved_by.into());
        self.approved_at = Some(Utc::now());
        self.status = ActionStatus::Pending;
    }

    /// Mark the action as executing.
    pub fn start_execution(&mut self) {
        self.executed_at = Some(Utc::now());
        self.status = ActionStatus::Executing;
    }

    /// Mark the action as verifying.
    pub fn start_verification(&mut self) {
        self.status = ActionStatus::Verifying;
    }

    /// Mark the action as completed successfully.
    pub fn complete(&mut self, result: impl Into<String>) {
        self.status = ActionStatus::Completed;
        self.result = Some(result.into());
    }

    /// Mark the action as failed.
    pub fn fail(&mut self, error: impl Into<String>) {
        self.status = ActionStatus::Failed;
        self.result = Some(error.into());
        self.retries += 1;
    }

    /// Check if the action can be retried.
    pub fn can_retry(&self) -> bool {
        self.retries < self.max_retries && self.status == ActionStatus::Failed
    }

    /// Reset the action for retry.
    pub fn retry(&mut self) {
        self.status = ActionStatus::Pending;
        self.executed_at = None;
    }
}

/// State of the reactive control loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlLoopState {
    /// Whether the loop is running.
    pub running: bool,
    /// Whether the loop is paused.
    pub paused: bool,
    /// When the loop was started.
    pub started_at: Option<DateTime<Utc>>,
    /// Total number of checks performed.
    pub total_checks: u64,
    /// Total anomalies detected.
    pub anomalies_detected: u64,
    /// Total actions executed.
    pub actions_executed: u64,
    /// Actions executed in the last hour.
    pub actions_this_hour: u32,
    /// When the hour counter was last reset.
    pub hour_reset_at: DateTime<Utc>,
    /// Currently active anomalies.
    pub active_anomalies: usize,
    /// Actions awaiting approval.
    pub pending_approvals: usize,
    /// Last check timestamp.
    pub last_check: Option<DateTime<Utc>>,
}

impl Default for ControlLoopState {
    fn default() -> Self {
        Self {
            running: false,
            paused: false,
            started_at: None,
            total_checks: 0,
            anomalies_detected: 0,
            actions_executed: 0,
            actions_this_hour: 0,
            hour_reset_at: Utc::now(),
            active_anomalies: 0,
            pending_approvals: 0,
            last_check: None,
        }
    }
}

impl ControlLoopState {
    /// Increment the actions-this-hour counter, resetting if hour changed.
    pub fn record_action(&mut self) {
        let now = Utc::now();
        let hour_since_reset = now.signed_duration_since(self.hour_reset_at).num_hours();
        if hour_since_reset >= 1 {
            self.actions_this_hour = 0;
            self.hour_reset_at = now;
        }
        self.actions_this_hour += 1;
        self.actions_executed += 1;
    }

    /// Check if we're within the rate limit for actions per hour.
    pub fn can_execute_action(&self, max_per_hour: u32) -> bool {
        self.actions_this_hour < max_per_hour
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_severity_ordering() {
        assert!(Severity::Info < Severity::Warning);
        assert!(Severity::Warning < Severity::Critical);
        assert!(Severity::Critical < Severity::Emergency);
    }

    #[test]
    fn test_system_event_type() {
        let event = SystemEvent::ContainerCrashed {
            name: "test".to_string(),
            exit_code: Some(1),
        };
        assert_eq!(event.event_type(), "container_crashed");
    }

    #[test]
    fn test_anomaly_creation() {
        let event = SystemEvent::ContainerCrashed {
            name: "test".to_string(),
            exit_code: Some(1),
        };
        let anomaly = Anomaly::new(event);
        assert_eq!(anomaly.severity, Severity::Critical);
        assert!(!anomaly.is_resolved());
        assert!(!anomaly.suggested_actions.is_empty());
    }

    #[test]
    fn test_action_lifecycle() {
        let mut action = Action::new("anomaly-1", ActionType::RotateLogs, true);
        assert_eq!(action.status, ActionStatus::AwaitingApproval);

        action.approve("user");
        assert_eq!(action.status, ActionStatus::Pending);
        assert!(action.approved_by.is_some());

        action.start_execution();
        assert_eq!(action.status, ActionStatus::Executing);

        action.complete("Success");
        assert_eq!(action.status, ActionStatus::Completed);
    }

    #[test]
    fn test_action_retry() {
        let mut action = Action::new("anomaly-1", ActionType::RotateLogs, false);
        action.fail("Error 1");
        assert!(action.can_retry());

        action.retry();
        assert_eq!(action.status, ActionStatus::Pending);
    }

    #[test]
    fn test_control_loop_rate_limit() {
        let mut state = ControlLoopState::default();
        assert!(state.can_execute_action(20));

        for _ in 0..20 {
            state.record_action();
        }
        assert!(!state.can_execute_action(20));
    }
}
