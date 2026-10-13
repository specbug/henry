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

use crate::build::{BuildRequest, Builder, Recipe, tail};
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

/// Six hex digits naming how an image was built, beyond its commit: the
/// recipe and Henry's overlay files. During onboarding one commit is built
/// several ways, and an image from one way must never stand in for another.
fn recipe_hash(build: &Build, overlays: &[(String, String)]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(format!("{build:?}"));
    for (path, content) in overlays {
        h.update([0]);
        h.update(path);
        h.update([0]);
        h.update(content);
    }
    hex::encode(&h.finalize()[..3])
}

/// What a refresh found.
#[derive(Debug, Clone, PartialEq)]
pub enum Refresh {
    /// Nothing is live, so there is nothing to refresh.
    NothingLive,
    /// A release is already on trial; refreshing waits for it.
    Busy,
    /// The rebuilt images are the ones running.
    Unchanged,
    /// A new release is on trial, with these services' images changed.
    Released(i64, Vec<String>),
    /// The live commit could not be rebuilt: a registry or the network
    /// failing, or a base image that moved on in a way the build cannot
    /// follow. The live release is untouched either way.
    Failed(String),
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
        let force = job.payload.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
        self.release(name, job.payload.get("commit").and_then(|v| v.as_str()), force).await
    }

    /// Build `commit` (the branch's head when unset) and put it on trial.
    /// `force` builds a commit again even if it was handled before.
    pub async fn release(&self, name: &str, commit: Option<&str>, force: bool) -> Result<Outcome> {
        let app = self.canon.app(name)?;
        let stored = app.lease()?;
        let commit = match commit {
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
        self.overlay(name, &dir)?;

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

        let images = match self.build_all(name, &format!("r{}", release.id), &commit, &lease, &dir, None).await? {
            Ok(images) => images,
            Err(why) => return self.fail(&release, why),
        };

        self.canon.set_release_images(release.id, &serde_json::Value::Object(images))?;
        self.canon.supersede_trials(name, release.id)?;
        self.canon.set_release_state(release.id, ReleaseState::Trial, Some(&format!("built from {short}")))?;
        info!(app = name, release = release.id, "on trial");
        Ok(Outcome::Released(release.id))
    }

    /// Build every service that needs a build. The outer error is worth
    /// retrying; the inner one is why this commit does not build. `fresh`
    /// pulls base images again and tags the result apart, for a refresh.
    async fn build_all(
        &self,
        name: &str,
        log_id: &str,
        commit: &str,
        lease: &Lease,
        dir: &std::path::Path,
        fresh: Option<&str>,
    ) -> Result<Result<serde_json::Map<String, serde_json::Value>, String>> {
        let short = &commit[..commit.len().min(12)];
        let overlays = self.canon.overlays(name)?;
        let mut images = serde_json::Map::new();
        for service in &lease.services {
            let (context, dockerfile);
            let recipe = match &service.build {
                Build::Image(image) => {
                    images.insert(service.name.clone(), image.clone().into());
                    continue;
                }
                Build::Railpack { path } => {
                    context = dir.join(path);
                    Recipe::Railpack { dir: &context }
                }
                Build::Dockerfile { path, context: ctx } => {
                    (context, dockerfile) = (dir.join(ctx), dir.join(path));
                    Recipe::Dockerfile { context: &context, dockerfile: &dockerfile }
                }
            };
            let mut tag = format!("henry/{name}-{}:{short}-{}", service.name, recipe_hash(&service.build, &overlays));
            if let Some(stamp) = fresh {
                tag.push_str(&format!("-{stamp}"));
            }
            if !self.builder.has(&tag).await? {
                let log = self.settings.logs.join(format!("{name}-{log_id}-{}.log", service.name));
                let labels = vec![
                    ("dev.henry.managed".to_owned(), "1".to_owned()),
                    ("dev.henry.app".to_owned(), name.to_owned()),
                    ("dev.henry.commit".to_owned(), commit.to_owned()),
                ];
                let req = BuildRequest { recipe, tag: &tag, pull: fresh.is_some(), log: &log, labels: &labels };
                if let Err(e) = self.build_with_retry(&req).await {
                    if e.is_transient() {
                        return Err(e);
                    }
                    let last = tail(&log, 15);
                    return Ok(Err(format!("`{}` did not build: {e}. Log: {}\n{last}", service.name, log.display())));
                }
            }
            images.insert(service.name.clone(), tag.into());
        }
        Ok(Ok(images))
    }

    /// Rebuild the live release's commit with base images pulled again. If
    /// any image changed, the result goes on trial like any push: security
    /// patches arrive without one, and still pass the gate. If none changed,
    /// the new tags are dropped and nothing happens.
    pub async fn refresh(&self, name: &str) -> Result<Refresh> {
        let app = self.canon.app(name)?;
        let Some(live) = app.live_release else { return Ok(Refresh::NothingLive) };
        if self.canon.wanted_releases(name)?.candidate.is_some() {
            return Ok(Refresh::Busy);
        }
        let release = self.canon.release(live)?;
        let lease = Lease::parse(&release.lease).map_err(|e| Error::Build(format!("the live lease: {e}")))?;
        if lease.services.iter().all(|s| matches!(s.build, Build::Image(_))) {
            return Ok(Refresh::Unchanged);
        }
        let dir = self.settings.checkouts.join(name);
        self.git.checkout(&dir, &app.repo, &lease.branch, &release.commit_sha).await?;
        self.overlay(name, &dir)?;
        let stamp = format!("f{}", canon::now_ms());
        let fresh = match self.build_all(name, "refresh", &release.commit_sha, &lease, &dir, Some(&stamp)).await? {
            Ok(images) => images,
            Err(why) => return Ok(Refresh::Failed(why)),
        };
        let mut images = serde_json::Map::new();
        let mut changed = Vec::new();
        for (service, image) in fresh {
            let new = image.as_str().unwrap_or_default().to_owned();
            let old = release.images.get(&service).and_then(|i| i.as_str()).unwrap_or_default().to_owned();
            let differs = new != old && {
                let (a, b) = (self.builder.image_id(&new).await?, self.builder.image_id(&old).await?);
                a.is_none() || a != b
            };
            if differs {
                changed.push(service.clone());
                images.insert(service, new.into());
            } else {
                if new != old {
                    let _ = self.builder.remove(&new).await;
                }
                images.insert(service, old.into());
            }
        }
        if changed.is_empty() {
            return Ok(Refresh::Unchanged);
        }
        let next = self.canon.create_release(name, &release.commit_sha, &release.lease)?;
        self.canon.set_release_images(next.id, &serde_json::Value::Object(images))?;
        self.canon.supersede_trials(name, next.id)?;
        let why = format!("base images refreshed: {}", changed.join(", "));
        self.canon.set_release_state(next.id, ReleaseState::Trial, Some(&why))?;
        info!(app = name, release = next.id, "{why}");
        Ok(Refresh::Released(next.id, changed))
    }

    /// Write Henry's own files (a Dockerfile Ghost wrote, say) into the
    /// checkout. They live in Canon, since Henry never commits to your repo,
    /// and only ever under `.henry/`.
    fn overlay(&self, app: &str, dir: &std::path::Path) -> Result<()> {
        for (path, content) in self.canon.overlays(app)? {
            let safe = path.starts_with(".henry/") && !path.split('/').any(|p| p == ".." || p.is_empty());
            if !safe {
                warn!(app, path, "skipping an overlay outside .henry/");
                continue;
            }
            let target = dir.join(&path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| Error::Io(format!("{}: {e}", parent.display())))?;
            }
            std::fs::write(&target, content).map_err(|e| Error::Io(format!("{}: {e}", target.display())))?;
        }
        Ok(())
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
