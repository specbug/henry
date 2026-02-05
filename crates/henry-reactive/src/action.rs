//! Action executor with timeout, retry, and verification.

use crate::types::{Action, ActionType};
use henry_maint::MaintenanceManager;
use henry_server::ContainerManager;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

#[derive(Error, Debug)]
pub enum ExecutorError {
    #[error("action timed out after {0} seconds")]
    Timeout(u64),

    #[error("action execution failed: {0}")]
    ExecutionFailed(String),

    #[error("verification failed: {0}")]
    VerificationFailed(String),

    #[error("container manager not available")]
    NoContainerManager,

    #[error("maintenance manager not available")]
    NoMaintenanceManager,

    #[error("action cancelled")]
    Cancelled,

    #[error("command blocked by policy: {0}")]
    CommandBlocked(String),
}

/// Result of action execution.
#[derive(Debug)]
pub struct ExecutionResult {
    /// Whether the action succeeded.
    pub success: bool,
    /// Result message or error.
    pub message: String,
    /// How long the action took.
    pub duration: Duration,
    /// Whether verification passed.
    pub verified: bool,
}

/// Notification callback type.
pub type NotifyCallback = Box<dyn Fn(String, crate::types::NotifyUrgency) + Send + Sync>;

/// The action executor.
pub struct ActionExecutor {
    /// Container manager for container operations.
    containers: Option<Arc<RwLock<ContainerManager>>>,
    /// Maintenance manager for backups and log rotation.
    maint: Option<Arc<RwLock<MaintenanceManager>>>,
    /// Callback for user notifications.
    notify_callback: Option<NotifyCallback>,
    /// Default command timeout.
    default_timeout: Duration,
    /// Verification delay after action.
    verification_delay: Duration,
    /// Commands blocked from execution.
    blocked_commands: Vec<String>,
}

impl ActionExecutor {
    /// Create a new executor.
    pub fn new(
        containers: Option<Arc<RwLock<ContainerManager>>>,
        maint: Option<Arc<RwLock<MaintenanceManager>>>,
    ) -> Self {
        Self {
            containers,
            maint,
            notify_callback: None,
            default_timeout: Duration::from_secs(60),
            verification_delay: Duration::from_secs(5),
            blocked_commands: vec![
                "rm -rf".to_string(),
                "rm -r /".to_string(),
                "dd if=".to_string(),
                "mkfs".to_string(),
                "chmod 777".to_string(),
            ],
        }
    }

    /// Set the notification callback.
    pub fn set_notify_callback(&mut self, callback: NotifyCallback) {
        self.notify_callback = Some(callback);
    }

    /// Set blocked commands.
    pub fn set_blocked_commands(&mut self, commands: Vec<String>) {
        self.blocked_commands = commands;
    }

    /// Execute an action.
    pub async fn execute(&self, action: &mut Action) -> Result<ExecutionResult, ExecutorError> {
        let start = std::time::Instant::now();

        // Mark as executing
        action.start_execution();

        info!("Executing action {}: {:?}", action.id, action.action_type);

        let result = match &action.action_type {
            ActionType::RestartContainer { name } => {
                self.restart_container(name).await
            }
            ActionType::RotateLogs => {
                self.rotate_logs().await
            }
            ActionType::TriggerBackup => {
                self.trigger_backup().await
            }
            ActionType::NotifyUser { message, urgency } => {
                self.notify_user(message, *urgency).await
            }
            ActionType::EscalateToHuman { reason } => {
                self.escalate_to_human(reason).await
            }
            ActionType::RunCommand { command, args, timeout_secs } => {
                self.run_command(command, args, Duration::from_secs(*timeout_secs)).await
            }
            ActionType::SelfUpdate => {
                self.self_update().await
            }
        };

        let duration = start.elapsed();

        match result {
            Ok(message) => {
                // Verification phase
                action.start_verification();
                tokio::time::sleep(self.verification_delay).await;

                let verified = self.verify_action(&action.action_type).await;
                if verified {
                    action.complete(&message);
                    info!("Action {} completed successfully: {}", action.id, message);
                    Ok(ExecutionResult {
                        success: true,
                        message,
                        duration,
                        verified: true,
                    })
                } else {
                    let error_msg = format!("Verification failed after: {}", message);
                    action.fail(&error_msg);
                    warn!("Action {} verification failed", action.id);
                    Ok(ExecutionResult {
                        success: false,
                        message: error_msg,
                        duration,
                        verified: false,
                    })
                }
            }
            Err(e) => {
                let error_msg = e.to_string();
                action.fail(&error_msg);
                error!("Action {} failed: {}", action.id, error_msg);
                Ok(ExecutionResult {
                    success: false,
                    message: error_msg,
                    duration,
                    verified: false,
                })
            }
        }
    }

    /// Restart a container.
    async fn restart_container(&self, name: &str) -> Result<String, ExecutorError> {
        let containers = self.containers.as_ref().ok_or(ExecutorError::NoContainerManager)?;
        let manager = containers.read().await;

        match manager.restart(name).await {
            Ok(()) => Ok(format!("Container '{}' restarted", name)),
            Err(e) => Err(ExecutorError::ExecutionFailed(format!(
                "Failed to restart container '{}': {}",
                name, e
            ))),
        }
    }

    /// Rotate logs.
    async fn rotate_logs(&self) -> Result<String, ExecutorError> {
        let maint = self.maint.as_ref().ok_or(ExecutorError::NoMaintenanceManager)?;
        let mut manager = maint.write().await;

        match manager.rotate_logs_now().await {
            Ok(result) => Ok(format!(
                "Rotated {} files, deleted {} old files",
                result.files_rotated, result.files_deleted
            )),
            Err(e) => Err(ExecutorError::ExecutionFailed(format!(
                "Failed to rotate logs: {}",
                e
            ))),
        }
    }

    /// Trigger a backup.
    async fn trigger_backup(&self) -> Result<String, ExecutorError> {
        let maint = self.maint.as_ref().ok_or(ExecutorError::NoMaintenanceManager)?;
        let mut manager = maint.write().await;

        match manager.backup_now().await {
            Ok(backup) => Ok(format!("Backup created: {}", backup.name)),
            Err(e) => Err(ExecutorError::ExecutionFailed(format!(
                "Failed to create backup: {}",
                e
            ))),
        }
    }

    /// Notify the user.
    async fn notify_user(&self, message: &str, urgency: crate::types::NotifyUrgency) -> Result<String, ExecutorError> {
        if let Some(ref callback) = self.notify_callback {
            callback(message.to_string(), urgency);
            Ok(format!("User notified: {}", message))
        } else {
            // Log the notification if no callback set
            info!("Notification (no callback): {} [{:?}]", message, urgency);
            Ok(format!("Notification logged: {}", message))
        }
    }

    /// Escalate to human.
    async fn escalate_to_human(&self, reason: &str) -> Result<String, ExecutorError> {
        let message = format!("[ESCALATION REQUIRED] {}", reason);
        if let Some(ref callback) = self.notify_callback {
            callback(message.clone(), crate::types::NotifyUrgency::High);
            Ok(format!("Escalated to human: {}", reason))
        } else {
            warn!("Escalation (no callback): {}", message);
            Ok(format!("Escalation logged: {}", reason))
        }
    }

    /// Run a custom command.
    async fn run_command(&self, command: &str, args: &[String], cmd_timeout: Duration) -> Result<String, ExecutorError> {
        // Check if command is blocked
        let full_cmd = format!("{} {}", command, args.join(" "));
        for blocked in &self.blocked_commands {
            if full_cmd.to_lowercase().contains(&blocked.to_lowercase()) {
                return Err(ExecutorError::CommandBlocked(blocked.clone()));
            }
        }

        debug!("Running command: {} {:?}", command, args);

        let mut cmd = Command::new(command);
        cmd.args(args);
        cmd.kill_on_drop(true);

        let output = timeout(cmd_timeout, cmd.output())
            .await
            .map_err(|_| ExecutorError::Timeout(cmd_timeout.as_secs()))?
            .map_err(|e| ExecutorError::ExecutionFailed(e.to_string()))?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            Ok(format!(
                "Command succeeded: {}",
                if stdout.len() > 200 {
                    format!("{}...", &stdout[..200])
                } else {
                    stdout.to_string()
                }
            ))
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(ExecutorError::ExecutionFailed(format!(
                "Command failed (exit {}): {}",
                output.status.code().unwrap_or(-1),
                stderr
            )))
        }
    }

    /// Self-update Henry (git pull, cargo build, restart).
    async fn self_update(&self) -> Result<String, ExecutorError> {
        // Get current executable path
        let exe_path = std::env::current_exe()
            .map_err(|e| ExecutorError::ExecutionFailed(format!("Cannot get executable path: {}", e)))?;
        let exe_dir = exe_path.parent()
            .ok_or_else(|| ExecutorError::ExecutionFailed("Cannot get executable directory".to_string()))?;

        // For now, we assume we're running from a git repository
        // In production, this would be more sophisticated

        info!("Starting self-update from {:?}", exe_dir);

        // Step 1: Git pull
        let git_output = Command::new("git")
            .current_dir(exe_dir)
            .args(["pull", "--rebase", "origin", "main"])
            .output()
            .await
            .map_err(|e| ExecutorError::ExecutionFailed(format!("Git pull failed: {}", e)))?;

        if !git_output.status.success() {
            let stderr = String::from_utf8_lossy(&git_output.stderr);
            return Err(ExecutorError::ExecutionFailed(format!(
                "Git pull failed: {}",
                stderr
            )));
        }

        // Step 2: Cargo build
        let build_output = Command::new("cargo")
            .current_dir(exe_dir)
            .args(["build", "--release"])
            .output()
            .await
            .map_err(|e| ExecutorError::ExecutionFailed(format!("Cargo build failed: {}", e)))?;

        if !build_output.status.success() {
            let stderr = String::from_utf8_lossy(&build_output.stderr);
            return Err(ExecutorError::ExecutionFailed(format!(
                "Cargo build failed: {}",
                stderr
            )));
        }

        // Note: The actual restart would typically be handled by a supervisor (systemd, etc.)
        // We just signal that an update is ready
        Ok("Self-update complete. Restart required to apply changes.".to_string())
    }

    /// Verify that an action had the intended effect.
    async fn verify_action(&self, action_type: &ActionType) -> bool {
        match action_type {
            ActionType::RestartContainer { name } => {
                // Verify container is running
                if let Some(ref containers) = self.containers {
                    let manager = containers.read().await;
                    if let Ok(list) = manager.list(true).await {
                        return list.iter().any(|c| {
                            c.name == *name && c.status == henry_server::ContainerStatus::Running
                        });
                    }
                }
                false
            }
            ActionType::RotateLogs => {
                // Log rotation is fire-and-forget, assume success
                true
            }
            ActionType::TriggerBackup => {
                // Backup verification could check if backup file exists
                // For now, assume success
                true
            }
            ActionType::NotifyUser { .. } | ActionType::EscalateToHuman { .. } => {
                // Notifications are fire-and-forget
                true
            }
            ActionType::RunCommand { .. } => {
                // Custom commands are verified by their exit code
                true
            }
            ActionType::SelfUpdate => {
                // Self-update is verified by the restart
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_blocked_command() {
        let executor = ActionExecutor::new(None, None);
        let result = executor.run_command("rm", &["-rf".to_string(), "/".to_string()], Duration::from_secs(10)).await;
        assert!(matches!(result, Err(ExecutorError::CommandBlocked(_))));
    }

    #[tokio::test]
    async fn test_notify_without_callback() {
        let executor = ActionExecutor::new(None, None);
        let result = executor.notify_user("test message", crate::types::NotifyUrgency::Low).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("logged"));
    }

    #[tokio::test]
    async fn test_execute_simple_command() {
        let executor = ActionExecutor::new(None, None);
        let result = executor.run_command("echo", &["hello".to_string()], Duration::from_secs(10)).await;
        assert!(result.is_ok());
    }
}
