//! The engine Henry drives, chosen from config: the Podman machine on the
//! Mac, a plain socket elsewhere.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use hold::{Engine, Machine, MachineState, PodmanMachine, Socket};

use crate::home::EngineConfig;

pub enum Host {
    Podman(PodmanMachine),
    Socket(Socket),
}

impl Host {
    pub fn from_config(config: &EngineConfig) -> Result<Host> {
        if let Some(path) = &config.socket {
            return Ok(Host::Socket(Socket { path: path.clone() }));
        }
        if cfg!(target_os = "macos") {
            let mut machine = PodmanMachine { name: config.machine.clone(), ..PodmanMachine::default() };
            if let Some(podman) = &config.podman {
                machine.podman = podman.clone();
            }
            return Ok(Host::Podman(machine));
        }
        Socket::discover()
            .map(Host::Socket)
            .ok_or_else(|| anyhow!("no Podman or Docker socket found; set [engine] socket in henry.toml"))
    }

    /// Connect to the engine without starting anything. For the CLI, which
    /// must never race the daemon to start the machine.
    pub async fn connect(&self) -> Result<Engine> {
        let socket = self.socket().await.context("finding the engine socket")?;
        Engine::connect(&socket).context("connecting to the engine")
    }
}

impl Machine for Host {
    async fn ensure_up(&self) -> hold::Result<MachineState> {
        match self {
            Host::Podman(m) => m.ensure_up().await,
            Host::Socket(s) => s.ensure_up().await,
        }
    }

    async fn socket(&self) -> hold::Result<PathBuf> {
        match self {
            Host::Podman(m) => m.socket().await,
            Host::Socket(s) => s.socket().await,
        }
    }
}
