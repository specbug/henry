//! Mend: upkeep.
//!
//! To mend is to make a small repair before it becomes a large one. Mend
//! runs the house's chores on a schedule, each as a Canon job, and each one
//! either proves it made things no worse or undoes itself:
//!
//! - **disk**, daily: drop images no release can return to, old build logs,
//!   and checkouts of apps that are gone.
//! - **refresh**, weekly: rebuild every live app on freshly pulled base images.
//!   A changed image goes on trial and must pass the gate like a push.
//! - **host**, every six hours: check the macOS settings an unattended Mac
//!   depends on, and report any that drift. Mend never changes them.
//! - **tools**, weekly: upgrade the binaries Henry owns (Caddy, cloudflared,
//!   restic, Railpack). A new version is kept only if everything still works.
//! - **bench**, weekly: run the model eval (Bench) on the default model and
//!   any candidates, and say if another would do better.

pub mod disk;
pub mod host;
pub mod tools;

use std::time::Duration;

use canon::{Canon, now_ms};

/// The Canon job kind Mend runs. A Mend job's `app` names its task.
pub const JOB: &str = "mend";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Transient(String),
    #[error("{0}")]
    Failed(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Canon(#[from] canon::Error),
}

impl Error {
    pub fn is_transient(&self) -> bool {
        matches!(self, Error::Transient(_))
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Task {
    Disk,
    Refresh,
    Host,
    Tools,
    Bench,
}

impl Task {
    pub const ALL: [Task; 5] = [Task::Disk, Task::Refresh, Task::Host, Task::Tools, Task::Bench];

    pub fn as_str(self) -> &'static str {
        match self {
            Task::Disk => "disk",
            Task::Refresh => "refresh",
            Task::Host => "host",
            Task::Tools => "tools",
            Task::Bench => "bench",
        }
    }

    pub fn parse(s: &str) -> Option<Task> {
        Task::ALL.into_iter().find(|t| t.as_str() == s)
    }

    pub fn every(self) -> Duration {
        const DAY: u64 = 24 * 3600;
        Duration::from_secs(match self {
            Task::Disk => DAY,
            Task::Host => 6 * 3600,
            Task::Refresh | Task::Tools | Task::Bench => 7 * DAY,
        })
    }
}

/// How soon a failed task is tried again.
const RETRY: Duration = Duration::from_secs(6 * 3600);

/// Queue every enabled task that is due. Returns what was queued.
pub fn schedule(canon: &Canon, enabled: &[Task]) -> Result<Vec<Task>> {
    let now = now_ms();
    let mut queued = Vec::new();
    for &task in enabled {
        let due = match canon.upkeep(task.as_str())? {
            None => true,
            Some(u) if u.ok => now - u.last_at >= task.every().as_millis() as i64,
            Some(u) => now - u.last_at >= RETRY.as_millis() as i64,
        };
        if due {
            canon.enqueue(JOB, Some(task.as_str()), &serde_json::json!({}))?;
            queued.push(task);
        }
    }
    Ok(queued)
}

#[cfg(test)]
mod tests;
