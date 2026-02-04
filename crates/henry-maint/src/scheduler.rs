//! Job scheduler for Henry maintenance tasks.
//!
//! Uses tokio-cron-scheduler for cron-based job scheduling.

use crate::types::{JobStatus, JobType, ScheduledJob};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_cron_scheduler::{Job, JobScheduler, JobSchedulerError};
use tracing::{debug, info};

/// Error type for scheduler operations.
#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error("scheduler error: {0}")]
    Scheduler(#[from] JobSchedulerError),

    #[error("job not found: {0}")]
    JobNotFound(String),

    #[error("invalid cron expression: {0}")]
    InvalidCron(String),
}

/// Job metadata stored alongside the scheduler.
#[derive(Debug, Clone)]
pub struct JobMetadata {
    pub name: String,
    pub cron: String,
    pub job_type: JobType,
    pub enabled: bool,
    pub last_run: Option<DateTime<Utc>>,
    pub last_status: Option<JobStatus>,
}

/// Maintenance job scheduler.
pub struct MaintScheduler {
    scheduler: JobScheduler,
    jobs: Arc<RwLock<HashMap<uuid::Uuid, JobMetadata>>>,
    running: bool,
}

impl MaintScheduler {
    /// Create a new maintenance scheduler.
    pub async fn new() -> Result<Self, SchedulerError> {
        let scheduler = JobScheduler::new().await?;

        Ok(Self {
            scheduler,
            jobs: Arc::new(RwLock::new(HashMap::new())),
            running: false,
        })
    }

    /// Start the scheduler.
    pub async fn start(&mut self) -> Result<(), SchedulerError> {
        if self.running {
            debug!("Scheduler already running");
            return Ok(());
        }

        self.scheduler.start().await?;
        self.running = true;
        info!("Maintenance scheduler started");

        Ok(())
    }

    /// Shutdown the scheduler.
    pub async fn shutdown(&mut self) -> Result<(), SchedulerError> {
        if !self.running {
            return Ok(());
        }

        self.scheduler.shutdown().await?;
        self.running = false;
        info!("Maintenance scheduler stopped");

        Ok(())
    }

    /// Add a job to the scheduler.
    pub async fn add_job<F, Fut>(
        &self,
        name: &str,
        cron: &str,
        job_type: JobType,
        callback: F,
    ) -> Result<uuid::Uuid, SchedulerError>
    where
        F: Fn() -> Fut + Send + Sync + Clone + 'static,
        Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        let jobs = self.jobs.clone();
        let job_name = name.to_string();
        let cron_expr = cron.to_string();
        let jt = job_type;

        // Create the job
        let job = Job::new_async(cron, move |_uuid, _lock| {
            let jobs = jobs.clone();
            let name = job_name.clone();
            let callback = callback.clone();

            Box::pin(async move {
                debug!("Running scheduled job: {}", name);

                // Mark as running
                {
                    let mut jobs = jobs.write().await;
                    if let Some((_, meta)) = jobs.iter_mut().find(|(_, m)| m.name == name) {
                        meta.last_status = Some(JobStatus::Running);
                    }
                }

                // Execute the callback
                let result = callback().await;

                // Update status
                {
                    let mut jobs = jobs.write().await;
                    if let Some((_, meta)) = jobs.iter_mut().find(|(_, m)| m.name == name) {
                        meta.last_run = Some(Utc::now());
                        meta.last_status = Some(match result {
                            Ok(()) => JobStatus::Success,
                            Err(msg) => JobStatus::Failed(msg),
                        });
                    }
                }
            })
        })
        .map_err(|e| SchedulerError::InvalidCron(e.to_string()))?;

        let job_id = job.guid();

        // Store metadata
        {
            let mut jobs = self.jobs.write().await;
            jobs.insert(
                job_id,
                JobMetadata {
                    name: name.to_string(),
                    cron: cron_expr,
                    job_type: jt,
                    enabled: true,
                    last_run: None,
                    last_status: None,
                },
            );
        }

        // Add to scheduler
        self.scheduler.add(job).await?;

        info!("Added maintenance job: {} ({})", name, cron);

        Ok(job_id)
    }

    /// Remove a job from the scheduler.
    pub async fn remove_job(&self, job_id: uuid::Uuid) -> Result<(), SchedulerError> {
        self.scheduler.remove(&job_id).await?;

        let mut jobs = self.jobs.write().await;
        if let Some(meta) = jobs.remove(&job_id) {
            info!("Removed maintenance job: {}", meta.name);
        }

        Ok(())
    }

    /// List all scheduled jobs.
    pub async fn list_jobs(&self) -> Vec<ScheduledJob> {
        let jobs = self.jobs.read().await;

        jobs.iter()
            .map(|(_, meta)| ScheduledJob {
                name: meta.name.clone(),
                cron: meta.cron.clone(),
                job_type: meta.job_type,
                enabled: meta.enabled,
                next_run: None, // Would need to calculate from cron expression
                last_run: meta.last_run,
                last_status: meta.last_status.clone(),
            })
            .collect()
    }

    /// Get the number of scheduled jobs.
    pub async fn job_count(&self) -> usize {
        self.jobs.read().await.len()
    }

    /// Check if the scheduler is running.
    pub fn is_running(&self) -> bool {
        self.running
    }
}

/// Parse a cron expression and calculate the next run time.
pub fn next_run_time(cron: &str) -> Option<DateTime<Utc>> {
    // This is a simplified implementation
    // In production, you'd use a proper cron parser
    use cron::Schedule;
    use std::str::FromStr;

    Schedule::from_str(cron)
        .ok()
        .and_then(|schedule| schedule.upcoming(Utc).next())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_scheduler_creation() {
        let scheduler = MaintScheduler::new().await.unwrap();
        assert!(!scheduler.is_running());
        assert_eq!(scheduler.job_count().await, 0);
    }

    #[tokio::test]
    async fn test_add_job() {
        let scheduler = MaintScheduler::new().await.unwrap();

        let job_id = scheduler
            .add_job("test_job", "0 0 * * * *", JobType::Backup, || async {
                Ok(())
            })
            .await
            .unwrap();

        assert_eq!(scheduler.job_count().await, 1);

        let jobs = scheduler.list_jobs().await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "test_job");
        assert_eq!(jobs[0].job_type, JobType::Backup);
    }

    #[tokio::test]
    async fn test_remove_job() {
        let scheduler = MaintScheduler::new().await.unwrap();

        let job_id = scheduler
            .add_job("test_job", "0 0 * * * *", JobType::Backup, || async {
                Ok(())
            })
            .await
            .unwrap();

        assert_eq!(scheduler.job_count().await, 1);

        scheduler.remove_job(job_id).await.unwrap();
        assert_eq!(scheduler.job_count().await, 0);
    }

    #[test]
    fn test_job_metadata() {
        let meta = JobMetadata {
            name: "test".to_string(),
            cron: "0 0 * * * *".to_string(),
            job_type: JobType::Backup,
            enabled: true,
            last_run: None,
            last_status: None,
        };

        assert_eq!(meta.name, "test");
        assert!(meta.enabled);
    }
}
