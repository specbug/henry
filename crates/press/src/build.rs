//! Image builds, through the engine's own CLI: `podman build` on the Mac,
//! `docker build` in CI. A service with no Dockerfile is built by Railpack,
//! which needs a BuildKit daemon: Henry runs one as a container,
//! `henry-buildkit`, and starts it when a Railpack build first needs it.
//! Output goes to a log file per service per release.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::{Error, Result};

/// What to build from.
#[derive(Debug, Clone, Copy)]
pub enum Recipe<'a> {
    Dockerfile {
        context: &'a Path,
        dockerfile: &'a Path,
    },
    /// Railpack reads the source in this directory and works out the build.
    Railpack {
        dir: &'a Path,
    },
}

pub struct BuildRequest<'a> {
    pub recipe: Recipe<'a>,
    pub tag: &'a str,
    /// Fetch base images again even if they are cached: Mend's refresh.
    pub pull: bool,
    pub log: &'a Path,
    pub labels: &'a [(String, String)],
}

pub trait Builder: Send + Sync {
    /// Whether an image with this tag already exists, so the build can be skipped.
    fn has(&self, tag: &str) -> impl Future<Output = Result<bool>> + Send;
    fn build(&self, req: &BuildRequest<'_>) -> impl Future<Output = Result<()>> + Send;

    /// The image's content id, to tell whether a rebuild changed anything.
    fn image_id(&self, _tag: &str) -> impl Future<Output = Result<Option<String>>> + Send {
        async { Ok(None) }
    }

    /// Drop a tag nobody will run.
    fn remove(&self, _tag: &str) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }
}

pub struct CliBuilder {
    /// `podman` or `docker`.
    pub program: PathBuf,
    pub timeout: Duration,
    /// Unset: a Railpack build fails, saying how to install it.
    pub railpack: Option<Railpack>,
}

pub struct Railpack {
    pub program: PathBuf,
    /// The BuildKit image `henry-buildkit` runs.
    pub buildkit_image: String,
    /// A directory holding a `docker` that is really the engine's CLI.
    /// Railpack shells out to `docker` to reach BuildKit and to load the
    /// image; on the Mac that has to be Podman.
    pub shims: Option<PathBuf>,
}

/// The BuildKit container Henry keeps for Railpack. It is not one of the
/// apps' containers, so it carries no `dev.henry.managed` label and Plumb
/// leaves it alone.
pub const BUILDKIT: &str = "henry-buildkit";
/// Railpack's exit code for a failure that says nothing about the app.
const TEMPFAIL: i32 = 75;

impl Builder for CliBuilder {
    async fn image_id(&self, tag: &str) -> Result<Option<String>> {
        CliBuilder::image_id(self, tag).await
    }

    async fn remove(&self, tag: &str) -> Result<()> {
        self.engine(&["rmi", tag]).await.map(|_| ()).map_err(Error::Build)
    }

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
        let mut cmd = match req.recipe {
            Recipe::Dockerfile { context, dockerfile } => {
                let mut cmd = Command::new(&self.program);
                cmd.arg("build").arg("-f").arg(dockerfile).arg("-t").arg(req.tag);
                if req.pull {
                    cmd.arg("--pull");
                }
                for (k, v) in req.labels {
                    cmd.arg("--label").arg(format!("{k}={v}"));
                }
                cmd.arg(context);
                cmd
            }
            Recipe::Railpack { dir } => {
                let Some(rp) = &self.railpack else {
                    return Err(Error::Build(
                        "Railpack, which builds repos without a Dockerfile, is not installed on this Mac: \
                         run `henry tools adopt railpack` (scripts/setup.sh does), then try again"
                            .into(),
                    ));
                };
                self.ensure_buildkit(rp).await?;
                let mut cmd = Command::new(&rp.program);
                cmd.arg("build").arg(dir).arg("--name").arg(req.tag).arg("--progress").arg("plain");
                cmd.env("BUILDKIT_HOST", format!("docker-container://{BUILDKIT}"));
                if let Some(shims) = &rp.shims {
                    let path = std::env::var_os("PATH").unwrap_or_default();
                    let joined =
                        std::env::join_paths(std::iter::once(shims.clone()).chain(std::env::split_paths(&path)))
                            .map_err(|e| Error::Io(e.to_string()))?;
                    cmd.env("PATH", joined);
                }
                cmd
            }
        };
        cmd.stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| Error::Io(e.to_string()))?)
            .stderr(log)
            .kill_on_drop(true);
        let status = tokio::time::timeout(self.timeout, cmd.status())
            .await
            .map_err(|_| {
                Error::Build(format!("the build ran past {} minutes and was stopped", self.timeout.as_secs() / 60))
            })?
            .map_err(|e| Error::Transient(format!("starting the build: {e}")))?;
        match status.code() {
            Some(0) => Ok(()),
            Some(TEMPFAIL) if matches!(req.recipe, Recipe::Railpack { .. }) => {
                Err(Error::Transient(format!("Railpack failed for a reason worth retrying ({status})")))
            }
            _ => Err(Error::Build(format!("the build exited with {status}"))),
        }
    }
}

impl CliBuilder {
    pub async fn image_id(&self, tag: &str) -> Result<Option<String>> {
        Ok(self.engine(&["image", "inspect", "-f", "{{.Id}}", tag]).await.ok().map(|s| s.trim().to_owned()))
    }

    /// Start `henry-buildkit` unless it is running.
    async fn ensure_buildkit(&self, rp: &Railpack) -> Result<()> {
        let running = self.engine(&["inspect", "-f", "{{.State.Running}}", BUILDKIT]).await;
        if running.as_deref().map(str::trim) == Ok("true") {
            return Ok(());
        }
        let _ = self.engine(&["rm", "-f", BUILDKIT]).await;
        self.engine(&[
            "run",
            "-d",
            "--name",
            BUILDKIT,
            "--privileged",
            "--restart",
            "unless-stopped",
            "--label",
            "dev.henry.tool=buildkit",
            &rp.buildkit_image,
        ])
        .await
        .map(|_| ())
        .map_err(|e| Error::Transient(format!("starting {BUILDKIT}: {e}")))
    }

    async fn engine(&self, args: &[&str]) -> std::result::Result<String, String> {
        let out = Command::new(&self.program).args(args).stdin(Stdio::null()).kill_on_drop(true).output();
        let out = tokio::time::timeout(Duration::from_secs(300), out)
            .await
            .map_err(|_| format!("{} {} timed out", self.program.display(), args[0]))?
            .map_err(|e| format!("{}: {e}", self.program.display()))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
        }
    }
}

/// The last `n` lines of a log, for a failure message.
pub fn tail(path: &Path, n: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}
