//! Self-maintenance module for Henry daemon.
//!
//! Provides automated maintenance tasks including:
//! - Scheduled backups
//! - Log rotation
//! - Update checking
//! - Session cleanup

use chrono::{DateTime, Utc};
use henry_config::MaintConfig;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::info;

pub mod backup;
pub mod logs;
pub mod scheduler;
mod types;
pub mod update;

pub use backup::{format_backup_size, BackupError, BackupManager};
pub use logs::{LogRotationError, LogRotator, RotationResult};
pub use scheduler::{MaintScheduler, SchedulerError};
pub use types::{
    BackupInfo, JobStatus, JobType, MaintStatus, ScheduledJob, UpdateInfo,
};
pub use update::{UpdateChecker, UpdateCheckerConfig, UpdateError};

/// Maintenance module errors.
#[derive(Error, Debug)]
pub enum MaintError {
    #[error("backup error: {0}")]
    Backup(#[from] BackupError),

    #[error("log rotation error: {0}")]
    LogRotation(#[from] LogRotationError),

    #[error("scheduler error: {0}")]
    Scheduler(#[from] SchedulerError),

    #[error("update check error: {0}")]
    Update(#[from] UpdateError),

    #[error("module not configured")]
    NotConfigured,
}

/// Maintenance module manager.
///
/// Coordinates all maintenance tasks: backups, log rotation, updates.
pub struct MaintenanceManager {
    config: MaintConfig,
    backup_manager: BackupManager,
    log_rotator: LogRotator,
    update_checker: UpdateChecker,
    scheduler: Option<Arc<RwLock<MaintScheduler>>>,
    last_backup: Option<DateTime<Utc>>,
    last_log_rotation: Option<DateTime<Utc>>,
    last_update_check: Option<DateTime<Utc>>,
    available_update: Option<String>,
}

impl MaintenanceManager {
    /// Create a new maintenance manager.
    pub fn new(config: MaintConfig) -> Self {
        let backup_manager = BackupManager::new(
            config.backup_dir.clone(),
            config.max_backups,
        );

        let log_rotator = LogRotator::new(
            config.log_dir.clone(),
            config.max_log_files,
            config.max_log_size,
        );

        let update_checker = UpdateChecker::new(UpdateCheckerConfig {
            github_repo: config.github_repo.clone(),
            current_version: env!("CARGO_PKG_VERSION").to_string(),
        });

        Self {
            config,
            backup_manager,
            log_rotator,
            update_checker,
            scheduler: None,
            last_backup: None,
            last_log_rotation: None,
            last_update_check: None,
            available_update: None,
        }
    }

    /// Initialize and start the scheduler with default jobs.
    pub async fn start_scheduler(&mut self) -> Result<(), MaintError> {
        if !self.config.enabled {
            return Ok(());
        }

        let mut scheduler = MaintScheduler::new().await?;

        // Add backup job if enabled
        if self.config.backup_enabled {
            let backup_dir = self.config.backup_dir.clone();
            let data_dir = self.config.data_dir.clone();
            let max_backups = self.config.max_backups;

            scheduler
                .add_job(
                    "backup",
                    &self.config.backup_cron,
                    JobType::Backup,
                    move || {
                        let backup_dir = backup_dir.clone();
                        let data_dir = data_dir.clone();
                        async move {
                            let manager = BackupManager::new(backup_dir, max_backups);
                            let files = vec![
                                data_dir.join("state").join("henry.db"),
                                data_dir.join("config.toml"),
                            ];
                            manager
                                .create_backup(&files)
                                .map(|_| ())
                                .map_err(|e| e.to_string())
                        }
                    },
                )
                .await?;
        }

        // Add log rotation job if enabled
        if self.config.log_rotation_enabled {
            let log_dir = self.config.log_dir.clone();
            let max_files = self.config.max_log_files;
            let max_size = self.config.max_log_size;

            scheduler
                .add_job(
                    "log_rotation",
                    &self.config.log_rotation_cron,
                    JobType::LogRotation,
                    move || {
                        let log_dir = log_dir.clone();
                        async move {
                            let rotator = LogRotator::new(log_dir, max_files, max_size);
                            rotator.rotate().map(|_| ()).map_err(|e| e.to_string())
                        }
                    },
                )
                .await?;
        }

        // Add update check job if enabled
        if self.config.update_check_enabled {
            let github_repo = self.config.github_repo.clone();
            let current_version = env!("CARGO_PKG_VERSION").to_string();

            scheduler
                .add_job(
                    "update_check",
                    &self.config.update_check_cron,
                    JobType::UpdateCheck,
                    move || {
                        let github_repo = github_repo.clone();
                        let current_version = current_version.clone();
                        async move {
                            let checker = UpdateChecker::new(UpdateCheckerConfig {
                                github_repo,
                                current_version,
                            });
                            checker
                                .check_github()
                                .await
                                .map(|info| {
                                    if info.update_available {
                                        info!(
                                            "Update available: {} -> {}",
                                            info.current_version, info.latest_version
                                        );
                                    }
                                })
                                .map_err(|e| e.to_string())
                        }
                    },
                )
                .await?;
        }

        scheduler.start().await?;
        self.scheduler = Some(Arc::new(RwLock::new(scheduler)));

        info!("Maintenance scheduler started");
        Ok(())
    }

    /// Stop the scheduler.
    pub async fn stop_scheduler(&mut self) -> Result<(), MaintError> {
        if let Some(scheduler) = self.scheduler.take() {
            let mut scheduler = scheduler.write().await;
            scheduler.shutdown().await?;
        }
        Ok(())
    }

    /// Perform a manual backup.
    pub async fn backup_now(&mut self) -> Result<BackupInfo, MaintError> {
        let files = vec![
            self.config.data_dir.join("state").join("henry.db"),
            self.config.data_dir.join("config.toml"),
        ];

        let backup = self.backup_manager.create_backup(&files)?;
        self.last_backup = Some(Utc::now());
        Ok(backup)
    }

    /// List all backups.
    pub fn list_backups(&self) -> Result<Vec<BackupInfo>, MaintError> {
        Ok(self.backup_manager.list_backups()?)
    }

    /// Delete a backup.
    pub fn delete_backup(&self, name: &str) -> Result<(), MaintError> {
        Ok(self.backup_manager.delete_backup(name)?)
    }

    /// Perform manual log rotation.
    pub async fn rotate_logs_now(&mut self) -> Result<RotationResult, MaintError> {
        let result = self.log_rotator.rotate()?;
        self.last_log_rotation = Some(Utc::now());
        Ok(result)
    }

    /// Check for updates manually.
    pub async fn check_updates_now(&mut self) -> Result<UpdateInfo, MaintError> {
        let info = self.update_checker.check_github().await?;
        self.last_update_check = Some(Utc::now());
        if info.update_available {
            self.available_update = Some(info.latest_version.clone());
        }
        Ok(info)
    }

    /// Get the full status of the maintenance module.
    pub async fn status(&self) -> MaintStatus {
        let backups = self.backup_manager.list_backups().unwrap_or_default();
        let total_backup_size = backups.iter().map(|b| b.size).sum();

        let scheduled_jobs = if let Some(ref scheduler) = self.scheduler {
            scheduler.read().await.job_count().await
        } else {
            0
        };

        MaintStatus {
            enabled: self.config.enabled,
            scheduled_jobs,
            last_backup: self.last_backup,
            next_backup: None, // Would calculate from cron
            last_log_rotation: self.last_log_rotation,
            last_update_check: self.last_update_check,
            available_update: self.available_update.clone(),
            current_version: env!("CARGO_PKG_VERSION").to_string(),
            backup_dir: self.config.backup_dir.display().to_string(),
            backups_retained: backups.len(),
            total_backup_size,
        }
    }

    /// List scheduled jobs.
    pub async fn list_jobs(&self) -> Vec<ScheduledJob> {
        if let Some(ref scheduler) = self.scheduler {
            scheduler.read().await.list_jobs().await
        } else {
            vec![]
        }
    }

    /// Get the configuration.
    pub fn config(&self) -> &MaintConfig {
        &self.config
    }

    /// Check if the scheduler is running.
    pub async fn is_scheduler_running(&self) -> bool {
        if let Some(ref scheduler) = self.scheduler {
            scheduler.read().await.is_running()
        } else {
            false
        }
    }
}

/// Format a scheduled job for display.
pub fn format_job(job: &ScheduledJob) -> String {
    let status = job
        .last_status
        .as_ref()
        .map(|s| match s {
            JobStatus::Success => "OK",
            JobStatus::Failed(_) => "ERR",
            JobStatus::Skipped => "SKIP",
            JobStatus::Running => "RUN",
        })
        .unwrap_or("--");

    let last_run = job
        .last_run
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "never".to_string());

    format!(
        "{} {} [{}] last: {}",
        status, job.name, job.cron, last_run
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_config(dir: &std::path::Path) -> MaintConfig {
        MaintConfig {
            enabled: true,
            backup_enabled: true,
            backup_cron: "0 0 3 * * *".to_string(),
            backup_dir: dir.join("backups"),
            max_backups: 5,
            log_rotation_enabled: true,
            log_rotation_cron: "0 0 0 * * *".to_string(),
            log_dir: dir.join("logs"),
            max_log_files: 10,
            max_log_size: 10 * 1024 * 1024,
            update_check_enabled: false,
            update_check_cron: "0 0 12 * * *".to_string(),
            github_repo: None,
            data_dir: dir.to_path_buf(),
        }
    }

    #[test]
    fn test_maintenance_manager_creation() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let manager = MaintenanceManager::new(config.clone());

        assert_eq!(manager.config().backup_dir, config.backup_dir);
    }

    #[tokio::test]
    async fn test_status() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let manager = MaintenanceManager::new(config);

        let status = manager.status().await;
        assert!(status.enabled);
        assert_eq!(status.scheduled_jobs, 0);
        assert!(status.last_backup.is_none());
    }

    #[tokio::test]
    async fn test_backup_now() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let mut manager = MaintenanceManager::new(config);

        // Create a file to backup
        std::fs::create_dir_all(dir.path().join("state")).unwrap();
        std::fs::write(dir.path().join("state/henry.db"), "test db").unwrap();
        std::fs::write(dir.path().join("config.toml"), "test config").unwrap();

        let backup = manager.backup_now().await.unwrap();
        assert!(backup.name.starts_with("henry_backup_"));
        assert!(manager.last_backup.is_some());
    }

    #[test]
    fn test_format_job() {
        let job = ScheduledJob {
            name: "backup".to_string(),
            cron: "0 0 3 * * *".to_string(),
            job_type: JobType::Backup,
            enabled: true,
            next_run: None,
            last_run: None,
            last_status: Some(JobStatus::Success),
        };

        let formatted = format_job(&job);
        assert!(formatted.contains("OK"));
        assert!(formatted.contains("backup"));
        assert!(formatted.contains("0 0 3 * * *"));
    }
}
