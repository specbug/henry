//! Press: fetch, build and release on push.
//!
//! You press push; Press runs. It fetches the exact commit into Henry's own
//! checkout, takes the repo's `lease.toml` if it has one, builds an image per
//! service, and records a release on trial. Plumb runs it beside the live
//! release. Only Proof can make it live: printers pull a proof before the run.
//!
//! Press works as a Canon job, so it is crash-only: a build cut off by a
//! reboot is picked up again from the release it already recorded.

pub mod build;
pub mod git;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use canon::{Canon, Desired, Job, Release, ReleaseState, Source, serde_json};
use lease::{Build, Lease};
use tracing::{info, warn};

use crate::build::{BuildRequest, Builder, tail};
use crate::git::Git;

/// The Canon job kind Press runs.
pub const JOB: &str = "press";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Worth retrying later: the network, a timeout, the engine away.
    #[error("{0}")]
    Transient(String),
    #[error("git: {0}")]
    Git(String),
    #[error("{0}")]
    Build(String),
    #[error("{0}")]
    Io(String),
    #[error(transparent)]
    Canon(#[from] canon::Error),
}

impl Error {
    /// A git failure while talking to the remote is the network's fault
    /// until shown otherwise.
    pub(crate) fn transient(e: Error) -> Error {
        match e {
            Error::Git(m) => Error::Transient(m),
            other => other,
        }
    }

    pub fn is_transient(&self) -> bool {
        matches!(self, Error::Transient(_))
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone)]
pub struct Settings {
    /// Henry's own clones, one directory per app.
    pub checkouts: PathBuf,
    /// Build logs, one file per service per release.
    pub logs: PathBuf,
    /// The limits a repo's `lease.toml` is checked against.
    pub limits: lease::Limits,
    /// Retries for a failed build before the commit is called broken. One:
    /// a build started right after a checkout can see stale files in the VM.
    pub build_retries: u32,
    pub retry_pause: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// A new release is on trial.
    Released(i64),
    /// This commit was already handled; nothing to do.
    Already(i64),
    /// The commit cannot be released as it stands. The release says why.
    Failed(i64, String),
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Released(id) => write!(f, "release {id} is on trial"),
            Outcome::Already(id) => write!(f, "already handled by release {id}"),
            Outcome::Failed(id, why) => write!(f, "release {id} failed: {}", why.lines().next().unwrap_or_default()),
        }
    }
}

pub struct Press<B> {
    canon: Arc<Canon>,
    git: Git,
    builder: B,
    settings: Settings,
}

/// Queue a deploy for `app`. Without `commit`, Press releases whatever the
/// branch points at when the job runs, so a burst of pushes builds once.
pub fn request(canon: &Canon, app: &str, commit: Option<&str>, force: bool) -> Result<Job> {
    let payload = serde_json::json!({ "commit": commit, "force": force });
    Ok(canon.enqueue(JOB, Some(app), &payload)?)
}

impl<B: Builder> Press<B> {
    pub fn new(canon: Arc<Canon>, git: Git, builder: B, settings: Settings) -> Self {
        Press { canon, git, builder, settings }
    }

    /// Check every auto-deploying app's branch and queue a deploy for any
    /// commit Henry has not seen. Returns the apps queued.
    pub async fn poll(&self) -> Result<Vec<String>> {
        let mut queued = Vec::new();
        for app in self.canon.apps()? {
            if app.desired == Desired::Stopped {
                continue;
            }
            let Ok(lease) = app.lease() else { continue };
            if !lease.auto_deploy {
                continue;
            }
            let head = match self.git.head(&app.repo, &lease.branch).await {
                Ok(Some(sha)) => sha,
                Ok(None) => continue,
                Err(e) => {
                    warn!(app = %app.name, "cannot read {}: {e}", lease.branch);
                    continue;
                }
            };
            // Any release of this commit, even a failed one, means it was
            // seen. A failed commit waits for a new push or `henry deploy`.
            if self.canon.release_for_commit(&app.name, &head)?.is_none() {
                request(&self.canon, &app.name, None, false)?;
                queued.push(app.name);
            }
        }
        Ok(queued)
    }

    /// Run one press job.
    pub async fn run(&self, job: &Job) -> Result<Outcome> {
        let name = job.app.as_deref().ok_or_else(|| Error::Io("a press job names no app".into()))?;
        let app = self.canon.app(name)?;
        let stored = app.lease()?;
        let force = job.payload.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
        let commit = match job.payload.get("commit").and_then(|v| v.as_str()) {
            Some(c) => c.to_owned(),
            None => self
                .git
                .head(&app.repo, &stored.branch)
                .await?
                .ok_or_else(|| Error::Git(format!("{} has no branch `{}`", app.repo, stored.branch)))?,
        };
        let short = &commit[..commit.len().min(12)];

        let resume = match self.canon.release_for_commit(name, &commit)? {
            Some(r) if matches!(r.state, ReleaseState::Pending | ReleaseState::Building) => Some(r),
            Some(r) if !force => return Ok(Outcome::Already(r.id)),
            _ => None,
        };

        let dir = self.settings.checkouts.join(name);
        self.git.checkout(&dir, &app.repo, &stored.branch, &commit).await?;

        // The repo's own lease wins over the stored one, after the same checks.
        let (lease, text) = match self.git.show(&dir, &commit, "lease.toml").await? {
            Some(text) => match Lease::parse_with(&text, &self.settings.limits) {
                Ok(l) if l.name == name => (l, text),
                Ok(l) => {
                    let why = format!("lease.toml names the app `{}`, but this app is `{name}`", l.name);
                    return self.refuse(resume, name, &commit, &app.lease, why);
                }
                Err(e) => return self.refuse(resume, name, &commit, &app.lease, format!("lease.toml at {short}: {e}")),
            },
            None => (stored, app.lease.clone()),
        };
        if text != app.lease {
            self.canon.update_lease(&lease, &text, Source::Repo)?;
        }

        let release = match resume {
            Some(r) => r,
            None => self.canon.create_release(name, &commit, &text)?,
        };
        self.canon.set_release_state(release.id, ReleaseState::Building, None)?;
        info!(app = name, release = release.id, commit = short, "building");

        let mut images = serde_json::Map::new();
        for service in &lease.services {
            let image = match &service.build {
                Build::Image(image) => image.clone(),
                Build::Railpack { .. } => {
                    let why = format!(
                        "`{}` asks for a Railpack build, which arrives with Draft; add a Dockerfile for now",
                        service.name
                    );
                    return self.fail(&release, why);
                }
                Build::Dockerfile { path, context } => {
                    let tag = format!("henry/{name}-{}:{short}", service.name);
                    if !self.builder.has(&tag).await? {
                        let log = self.settings.logs.join(format!("{name}-r{}-{}.log", release.id, service.name));
                        let labels = vec![
                            ("dev.henry.managed".to_owned(), "1".to_owned()),
                            ("dev.henry.app".to_owned(), name.to_owned()),
                            ("dev.henry.commit".to_owned(), commit.clone()),
                        ];
                        let req = BuildRequest {
                            context: &dir.join(context),
                            dockerfile: &dir.join(path),
                            tag: &tag,
                            log: &log,
                            labels: &labels,
                        };
                        if let Err(e) = self.build_with_retry(&req).await {
                            if e.is_transient() {
                                return Err(e);
                            }
                            let last = tail(&log, 15);
                            let why = format!("`{}` did not build: {e}. Log: {}\n{last}", service.name, log.display());
                            return self.fail(&release, why);
                        }
                    }
                    tag
                }
            };
            images.insert(service.name.clone(), image.into());
        }

        self.canon.set_release_images(release.id, &serde_json::Value::Object(images))?;
        self.canon.supersede_trials(name, release.id)?;
        self.canon.set_release_state(release.id, ReleaseState::Trial, Some(&format!("built from {short}")))?;
        info!(app = name, release = release.id, "on trial");
        Ok(Outcome::Released(release.id))
    }

    async fn build_with_retry(&self, req: &BuildRequest<'_>) -> Result<()> {
        let mut attempt = 0;
        loop {
            match self.builder.build(req).await {
                Ok(()) => return Ok(()),
                Err(e) if attempt < self.settings.build_retries && !e.is_transient() => {
                    attempt += 1;
                    warn!(tag = req.tag, "build failed, retrying once in case the VM saw stale files: {e}");
                    tokio::time::sleep(self.settings.retry_pause).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Record a refusal against a release for this commit, so it is seen and
    /// not retried on every poll.
    fn refuse(&self, resume: Option<Release>, app: &str, commit: &str, lease: &str, why: String) -> Result<Outcome> {
        let release = match resume {
            Some(r) => r,
            None => self.canon.create_release(app, commit, lease)?,
        };
        self.fail(&release, why)
    }

    fn fail(&self, release: &Release, why: String) -> Result<Outcome> {
        warn!(app = %release.app, release = release.id, "release failed: {why}");
        self.canon.set_release_state(release.id, ReleaseState::Failed, Some(&why))?;
        Ok(Outcome::Failed(release.id, why))
    }
}

#[cfg(test)]
mod tests;
