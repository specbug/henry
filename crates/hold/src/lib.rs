//! Hold: the cargo space below deck.
//!
//! On the Mac, containers live in a Linux VM that Podman manages. Hold is the
//! narrow interface Henry uses to reach them: a [`Machine`] that brings the VM
//! up, and a [`Runtime`] that creates, starts, stops and lists containers.
//!
//! [`Engine`] speaks the Docker-compatible API, which Podman serves, so the
//! same code runs against Podman on the Mac and Docker in CI. [`fake::Fake`]
//! is an in-memory runtime for testing Plumb without either.

mod engine;
pub mod fake;
mod machine;
mod spec;

use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

pub use engine::Engine;
pub use machine::{Machine, MachineState, PodmanMachine, Socket};
pub use spec::{ContainerSpec, EnvVar, Mount, Publish};

/// Label keys Henry puts on everything it creates. Anything without
/// [`label::MANAGED`] is not Henry's and is never touched.
pub mod label {
    pub const MANAGED: &str = "dev.henry.managed";
    pub const APP: &str = "dev.henry.app";
    pub const SERVICE: &str = "dev.henry.service";
    /// Absent on a stateful container, which belongs to no single release.
    pub const RELEASE: &str = "dev.henry.release";
    pub const SPEC: &str = "dev.henry.spec";
}

pub type Labels = BTreeMap<String, String>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0} not found")]
    NotFound(String),
    #[error("{0} already exists")]
    Conflict(String),
    #[error("the container engine is unreachable: {0}")]
    Unreachable(String),
    #[error("the container engine refused: {0}")]
    Engine(String),
    #[error("the Podman machine: {0}")]
    Machine(String),
    #[error("{0} timed out")]
    Timeout(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Created,
    Running,
    Paused,
    Restarting,
    Exited,
    Other,
}

/// A container as the engine reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub state: State,
    pub labels: Labels,
    /// Network name to the aliases the container answers to there.
    pub networks: BTreeMap<String, Vec<String>>,
}

impl Container {
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.get(key).map(String::as_str)
    }

    /// The hash of the spec it was created from.
    pub fn spec_hash(&self) -> Option<&str> {
        self.label(label::SPEC)
    }
}

/// What `inspect` adds to a listing: what Proof and Plumb need to judge health.
#[derive(Debug, Clone, PartialEq)]
pub struct Inspection {
    pub state: State,
    pub restart_count: i64,
    pub exit_code: Option<i64>,
    pub oom_killed: bool,
}

/// The container engine. Every call is idempotent where it can be: ensuring
/// something that exists is a no-op, removing something already gone is fine.
pub trait Runtime: Send + Sync {
    fn ping(&self) -> impl Future<Output = Result<()>> + Send;

    /// Every container Henry manages, running or not. Nothing else.
    fn list(&self) -> impl Future<Output = Result<Vec<Container>>> + Send;

    fn ensure_network(&self, name: &str, labels: &Labels) -> impl Future<Output = Result<()>> + Send;
    fn ensure_volume(&self, name: &str, labels: &Labels) -> impl Future<Output = Result<()>> + Send;

    /// Pull the image unless it is already present.
    fn ensure_image(&self, image: &str) -> impl Future<Output = Result<()>> + Send;

    /// Create (but do not start) a container. Returns its id.
    fn create(&self, spec: &ContainerSpec) -> impl Future<Output = Result<String>> + Send;
    fn start(&self, id: &str) -> impl Future<Output = Result<()>> + Send;
    fn stop(&self, id: &str, grace: Duration) -> impl Future<Output = Result<()>> + Send;
    /// Remove a container, stopping it first if needed. Its volumes stay.
    fn remove(&self, id: &str) -> impl Future<Output = Result<()>> + Send;

    /// Change the names a running container answers to on a network,
    /// without restarting it. This is how a release goes live in place.
    fn set_aliases(&self, id: &str, network: &str, aliases: &[String]) -> impl Future<Output = Result<()>> + Send;

    fn inspect(&self, id: &str) -> impl Future<Output = Result<Inspection>> + Send;
    /// Log lines, at most `tail` of them, and only those written since
    /// `since` (Unix seconds) when given.
    fn logs(&self, id: &str, tail: usize, since: Option<i64>) -> impl Future<Output = Result<String>> + Send;
}

/// Share one runtime between Plumb and whoever else needs to look at it.
impl<T: Runtime> Runtime for std::sync::Arc<T> {
    async fn ping(&self) -> Result<()> {
        (**self).ping().await
    }
    async fn list(&self) -> Result<Vec<Container>> {
        (**self).list().await
    }
    async fn ensure_network(&self, name: &str, labels: &Labels) -> Result<()> {
        (**self).ensure_network(name, labels).await
    }
    async fn ensure_volume(&self, name: &str, labels: &Labels) -> Result<()> {
        (**self).ensure_volume(name, labels).await
    }
    async fn ensure_image(&self, image: &str) -> Result<()> {
        (**self).ensure_image(image).await
    }
    async fn create(&self, spec: &ContainerSpec) -> Result<String> {
        (**self).create(spec).await
    }
    async fn start(&self, id: &str) -> Result<()> {
        (**self).start(id).await
    }
    async fn stop(&self, id: &str, grace: Duration) -> Result<()> {
        (**self).stop(id, grace).await
    }
    async fn remove(&self, id: &str) -> Result<()> {
        (**self).remove(id).await
    }
    async fn set_aliases(&self, id: &str, network: &str, aliases: &[String]) -> Result<()> {
        (**self).set_aliases(id, network, aliases).await
    }
    async fn inspect(&self, id: &str) -> Result<Inspection> {
        (**self).inspect(id).await
    }
    async fn logs(&self, id: &str, tail: usize, since: Option<i64>) -> Result<String> {
        (**self).logs(id, tail, since).await
    }
}

impl<T: Machine> Machine for std::sync::Arc<T> {
    async fn ensure_up(&self) -> Result<MachineState> {
        (**self).ensure_up().await
    }
    async fn socket(&self) -> Result<std::path::PathBuf> {
        (**self).socket().await
    }
}
