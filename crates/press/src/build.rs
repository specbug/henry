//! Image builds, through the engine's own CLI: `podman build` on the Mac,
//! `docker build` in CI. Output goes to a log file per service per release.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::{Error, Result};

pub struct BuildRequest<'a> {
    pub context: &'a Path,
    pub dockerfile: &'a Path,
    pub tag: &'a str,
    pub log: &'a Path,
    pub labels: &'a [(String, String)],
}

pub trait Builder: Send + Sync {
    /// Whether an image with this tag already exists, so the build can be skipped.
    fn has(&self, tag: &str) -> impl Future<Output = Result<bool>> + Send;
    fn build(&self, req: &BuildRequest<'_>) -> impl Future<Output = Result<()>> + Send;
}

pub struct CliBuilder {
    /// `podman` or `docker`.
    pub program: PathBuf,
    pub timeout: Duration,
}

impl Builder for CliBuilder {
    async fn has(&self, tag: &str) -> Result<bool> {
        let status = Command::new(&self.program)
            .args(["image", "inspect", tag])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await
            .map_err(|e| Error::Transient(format!("{}: {e}", self.program.display())))?;
        Ok(status.success())
    }

    async fn build(&self, req: &BuildRequest<'_>) -> Result<()> {
        if let Some(dir) = req.log.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Error::Io(format!("{}: {e}", dir.display())))?;
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(req.log)
            .map_err(|e| Error::Io(format!("{}: {e}", req.log.display())))?;
        let mut cmd = Command::new(&self.program);
        cmd.arg("build").arg("-f").arg(req.dockerfile).arg("-t").arg(req.tag);
        for (k, v) in req.labels {
            cmd.arg("--label").arg(format!("{k}={v}"));
        }
        cmd.arg(req.context)
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| Error::Io(e.to_string()))?)
            .stderr(log)
            .kill_on_drop(true);
        let status = tokio::time::timeout(self.timeout, cmd.status())
            .await
            .map_err(|_| {
                Error::Build(format!("the build ran past {} minutes and was stopped", self.timeout.as_secs() / 60))
            })?
            .map_err(|e| Error::Transient(format!("{}: {e}", self.program.display())))?;
        if status.success() { Ok(()) } else { Err(Error::Build(format!("the build exited with {status}"))) }
    }
}

/// The last `n` lines of a log, for a failure message.
pub fn tail(path: &Path, n: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}
