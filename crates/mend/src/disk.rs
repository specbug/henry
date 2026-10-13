//! Disk upkeep: images no release will run again, old build logs, and
//! checkouts of apps that are gone.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use canon::Canon;
use tokio::process::Command;
use tracing::warn;

use crate::{Error, Result};

pub struct Disk {
    /// `podman` or `docker`.
    pub engine: PathBuf,
    pub logs: PathBuf,
    pub checkouts: PathBuf,
    /// Releases per app whose images are kept, for a rollback.
    pub keep_releases: usize,
    pub log_age: Duration,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Pruned {
    pub images: Vec<String>,
    pub logs: usize,
    pub checkouts: Vec<String>,
}

impl std::fmt::Display for Pruned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} image(s), {} log(s), {} checkout(s) removed", self.images.len(), self.logs, self.checkouts.len())
    }
}

/// Every image a release could still run: each app's live release, its
/// candidate, and its newest `keep` releases.
pub fn keep(canon: &Canon, keep: usize) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for app in canon.apps()? {
        let mut releases = canon.releases(&app.name, keep)?;
        let wanted = canon.wanted_releases(&app.name)?;
        releases.extend(wanted.live);
        releases.extend(wanted.candidate);
        for r in releases {
            if let Some(images) = r.images.as_object() {
                out.extend(images.values().filter_map(|i| i.as_str()).map(str::to_owned));
            }
        }
    }
    Ok(out)
}

/// Henry's own build images not in `keep`. Images Henry did not build (a
/// published `postgres:17`) and its helpers are never touched.
pub fn stale(images: &[String], keep: &BTreeSet<String>) -> Vec<String> {
    images
        .iter()
        .filter(|i| i.starts_with("henry/") && !i.starts_with("henry/ark-helper") && !keep.contains(*i))
        .cloned()
        .collect()
}

impl Disk {
    async fn engine(&self, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.engine)
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| Error::Transient(format!("{}: {e}", self.engine.display())))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(Error::Failed(String::from_utf8_lossy(&out.stderr).trim().to_owned()))
        }
    }

    pub async fn prune(&self, canon: &Canon) -> Result<Pruned> {
        let mut pruned = Pruned::default();
        let keep = keep(canon, self.keep_releases)?;
        let listed = self.engine(&["images", "--format", "{{.Repository}}:{{.Tag}}"]).await?;
        let images: Vec<String> = listed.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect();
        for image in stale(&images, &keep) {
            // An image a container still uses refuses to go, which is right.
            match self.engine(&["rmi", &image]).await {
                Ok(_) => pruned.images.push(image),
                Err(e) => warn!(%image, "kept: {e}"),
            }
        }
        let _ = self.engine(&["image", "prune", "-f"]).await;

        pruned.logs = remove_old(&self.logs, self.log_age)?;

        let mut known: BTreeSet<String> = canon.apps()?.into_iter().map(|a| a.name).collect();
        known.extend(canon.drafts()?.into_iter().map(|d| d.app));
        if let Ok(entries) = std::fs::read_dir(&self.checkouts) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.path().is_dir() && !known.contains(&name) {
                    std::fs::remove_dir_all(entry.path())?;
                    pruned.checkouts.push(name);
                }
            }
        }
        Ok(pruned)
    }
}

/// Remove files under `dir` older than `age`; returns how many.
fn remove_old(dir: &std::path::Path, age: Duration) -> Result<usize> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(0) };
    let cutoff = SystemTime::now().checked_sub(age).unwrap_or(SystemTime::UNIX_EPOCH);
    let mut n = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            n += remove_old(&path, age)?;
        } else if entry.metadata()?.modified()? < cutoff {
            std::fs::remove_file(&path)?;
            n += 1;
        }
    }
    Ok(n)
}
