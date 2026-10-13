//! The VM the engine lives in.
//!
//! Only Plumb ever brings the machine up. Two concurrent `podman machine
//! start` calls SIGTERM each other's gvproxy and collapse the whole stack;
//! hostd learnt this the hard way, so here there is exactly one caller.

use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use tokio::process::Command;

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineState {
    /// It was already running.
    Up,
    /// It was down and Henry started it.
    Started,
}

pub trait Machine: Send + Sync {
    /// Make sure the engine's VM is running.
    fn ensure_up(&self) -> impl Future<Output = Result<MachineState>> + Send;
    /// Where the engine's Docker-compatible socket is.
    fn socket(&self) -> impl Future<Output = Result<PathBuf>> + Send;
}

/// A Podman machine on macOS, driven through the `podman` CLI.
pub struct PodmanMachine {
    pub podman: PathBuf,
    /// `None` uses the default machine.
    pub name: Option<String>,
}

impl Default for PodmanMachine {
    fn default() -> Self {
        PodmanMachine { podman: "/opt/homebrew/bin/podman".into(), name: None }
    }
}

const INSPECT_TIMEOUT: Duration = Duration::from_secs(30);
const START_TIMEOUT: Duration = Duration::from_secs(300);

impl PodmanMachine {
    async fn podman(&self, args: &[&str], timeout: Duration) -> Result<String> {
        let mut cmd = Command::new(&self.podman);
        cmd.args(args);
        if let Some(name) = &self.name {
            cmd.arg(name);
        }
        cmd.kill_on_drop(true);
        let what = format!("podman {}", args.join(" "));
        let out = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| Error::Timeout(what.clone()))?
            .map_err(|e| Error::Machine(format!("{what}: {e}")))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(Error::Machine(format!("{what}: {}", err.trim())));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }
}

impl Machine for PodmanMachine {
    async fn ensure_up(&self) -> Result<MachineState> {
        let state = self.podman(&["machine", "inspect", "--format", "{{.State}}"], INSPECT_TIMEOUT).await?;
        match state.as_str() {
            "running" => Ok(MachineState::Up),
            // Someone else is starting it (Podman Desktop at login). Do not
            // race it; the next pass will find it running.
            "starting" => Err(Error::Machine("the machine is already starting".into())),
            _ => {
                self.podman(&["machine", "start"], START_TIMEOUT).await?;
                Ok(MachineState::Started)
            }
        }
    }

    async fn socket(&self) -> Result<PathBuf> {
        let path = self
            .podman(&["machine", "inspect", "--format", "{{.ConnectionInfo.PodmanSocket.Path}}"], INSPECT_TIMEOUT)
            .await?;
        if path.is_empty() {
            return Err(Error::Machine("the machine reports no socket".into()));
        }
        Ok(path.into())
    }
}

/// An engine with no VM to manage: Podman or Docker on Linux. It is up when
/// its socket exists.
pub struct Socket {
    pub path: PathBuf,
}

impl Socket {
    /// The first socket that exists, in the order Henry prefers them.
    pub fn discover() -> Option<Socket> {
        let mut candidates = Vec::new();
        if let Some(host) = std::env::var_os("DOCKER_HOST") {
            let host = host.to_string_lossy().into_owned();
            if let Some(path) = host.strip_prefix("unix://") {
                candidates.push(PathBuf::from(path));
            }
        }
        if let Some(run) = std::env::var_os("XDG_RUNTIME_DIR") {
            candidates.push(PathBuf::from(run).join("podman/podman.sock"));
        }
        candidates.push("/run/podman/podman.sock".into());
        candidates.push("/var/run/docker.sock".into());
        candidates.into_iter().find(|p| p.exists()).map(|path| Socket { path })
    }
}

impl Machine for Socket {
    async fn ensure_up(&self) -> Result<MachineState> {
        if self.path.exists() {
            Ok(MachineState::Up)
        } else {
            Err(Error::Unreachable(format!("{} does not exist", self.path.display())))
        }
    }

    async fn socket(&self) -> Result<PathBuf> {
        Ok(self.path.clone())
    }
}
