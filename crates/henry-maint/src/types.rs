//! Types for the maintenance module.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Full maintenance module status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintStatus {
    /// Whether maintenance is enabled.
    pub enabled: bool,
    /// Number of scheduled jobs.
    pub scheduled_jobs: usize,
    /// Last backup timestamp.
    pub last_backup: Option<DateTime<Utc>>,
    /// Next scheduled backup.
    pub next_backup: Option<DateTime<Utc>>,
    /// Last log rotation.
    pub last_log_rotation: Option<DateTime<Utc>>,
    /// Last update check.
    pub last_update_check: Option<DateTime<Utc>>,
    /// Available update version (if any).
    pub available_update: Option<String>,
    /// Current version.
    pub current_version: String,
    /// Backup directory.
    pub backup_dir: String,
    /// Number of backups retained.
    pub backups_retained: usize,
    /// Total backup size in bytes.
    pub total_backup_size: u64,
}

impl Default for MaintStatus {
    fn default() -> Self {
        Self {
            enabled: false,
            scheduled_jobs: 0,
            last_backup: None,
            next_backup: None,
            last_log_rotation: None,
            last_update_check: None,
            available_update: None,
            current_version: env!("CARGO_PKG_VERSION").to_string(),
            backup_dir: String::new(),
            backups_retained: 0,
            total_backup_size: 0,
        }
    }
}

/// Information about a backup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupInfo {
    /// Backup file name.
    pub name: String,
    /// Full path to backup file.
    pub path: String,
    /// Backup timestamp.
    pub created_at: DateTime<Utc>,
    /// Size in bytes.
    pub size: u64,
    /// Whether this is a compressed backup.
    pub compressed: bool,
}

/// A scheduled maintenance job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledJob {
    /// Job name/identifier.
    pub name: String,
    /// Cron expression.
    pub cron: String,
    /// Job type.
    pub job_type: JobType,
    /// Whether the job is enabled.
    pub enabled: bool,
    /// Next scheduled run.
    pub next_run: Option<DateTime<Utc>>,
    /// Last run timestamp.
    pub last_run: Option<DateTime<Utc>>,
    /// Last run status.
    pub last_status: Option<JobStatus>,
}

/// Types of maintenance jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobType {
    /// Database and config backup.
    Backup,
    /// Log rotation.
    LogRotation,
    /// Update check.
    UpdateCheck,
    /// Session cleanup.
    SessionCleanup,
    /// Cache cleanup.
    CacheCleanup,
}

impl std::fmt::Display for JobType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobType::Backup => write!(f, "backup"),
            JobType::LogRotation => write!(f, "log_rotation"),
            JobType::UpdateCheck => write!(f, "update_check"),
            JobType::SessionCleanup => write!(f, "session_cleanup"),
            JobType::CacheCleanup => write!(f, "cache_cleanup"),
        }
    }
}

/// Status of a job execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Job completed successfully.
    Success,
    /// Job failed with an error.
    Failed(String),
    /// Job was skipped.
    Skipped,
    /// Job is currently running.
    Running,
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobStatus::Success => write!(f, "success"),
            JobStatus::Failed(msg) => write!(f, "failed: {}", msg),
            JobStatus::Skipped => write!(f, "skipped"),
            JobStatus::Running => write!(f, "running"),
        }
    }
}

/// Result of an update check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateInfo {
    /// Current installed version.
    pub current_version: String,
    /// Latest available version.
    pub latest_version: String,
    /// Whether an update is available.
    pub update_available: bool,
    /// Release notes URL (if available).
    pub release_url: Option<String>,
    /// When the check was performed.
    pub checked_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_maint_status_default() {
        let status = MaintStatus::default();
        assert!(!status.enabled);
        assert_eq!(status.scheduled_jobs, 0);
        assert!(status.last_backup.is_none());
    }

    #[test]
    fn test_job_type_display() {
        assert_eq!(JobType::Backup.to_string(), "backup");
        assert_eq!(JobType::LogRotation.to_string(), "log_rotation");
        assert_eq!(JobType::UpdateCheck.to_string(), "update_check");
    }

    #[test]
    fn test_job_status_display() {
        assert_eq!(JobStatus::Success.to_string(), "success");
        assert_eq!(JobStatus::Running.to_string(), "running");
        assert_eq!(
            JobStatus::Failed("test error".to_string()).to_string(),
            "failed: test error"
        );
    }

    #[test]
    fn test_backup_info_serialization() {
        let info = BackupInfo {
            name: "backup_2024.tar.gz".to_string(),
            path: "/backups/backup_2024.tar.gz".to_string(),
            created_at: Utc::now(),
            size: 1024,
            compressed: true,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("backup_2024.tar.gz"));
        assert!(json.contains("compressed"));
    }
}
