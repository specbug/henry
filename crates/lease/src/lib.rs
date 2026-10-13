//! Lease: the per-app spec, `lease.toml`.
//!
//! An app leases room on the Mac. The lease says what it may do (build this,
//! listen there, keep these volumes, answer to that hostname) and the rent it
//! owes in return: a health check for every service. Whoever wrote it — you, the
//! repo, or Ghost — it passes through [`Lease::parse`] before anything
//! acts on it, and the parse is the policy check.
//!
//! The lease deliberately cannot express privileged containers, host
//! networking or host paths. There are no fields for them, and unknown fields
//! are rejected, so they cannot be smuggled in.

mod check;
mod raw;
mod size;

use std::collections::BTreeMap;
use std::fmt;

pub use check::Limits;
pub use size::{format_bytes, parse_bytes};

/// A validated lease.
#[derive(Debug, Clone, PartialEq)]
pub struct Lease {
    pub name: String,
    pub branch: String,
    pub auto_deploy: bool,
    pub ai: Option<AiPrefs>,
    pub services: Vec<Service>,
}

/// The model Ghost uses for this app, overriding the global default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiPrefs {
    pub provider: Provider,
    pub model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Meta,
    Gemini,
    Claude,
    OpenAi,
}

impl Provider {
    pub const ALL: [Provider; 4] = [Provider::Meta, Provider::Gemini, Provider::Claude, Provider::OpenAi];

    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Meta => "meta",
            Provider::Gemini => "gemini",
            Provider::Claude => "claude",
            Provider::OpenAi => "openai",
        }
    }

    pub fn parse(s: &str) -> Option<Provider> {
        Provider::ALL.into_iter().find(|p| p.as_str() == s)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Service {
    pub name: String,
    pub build: Build,
    pub port: Option<u16>,
    pub command: Option<Vec<String>>,
    /// Plain, non-secret values.
    pub env: BTreeMap<String, String>,
    /// Names only. Values live with the Hush.
    pub secrets: Vec<String>,
    pub health: Health,
    pub public: Option<Public>,
    pub volumes: Vec<Volume>,
    pub resources: Resources,
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Build {
    /// A published image, used as is.
    Image(String),
    /// A Dockerfile in the repo. Both paths are relative to the repo root.
    Dockerfile { path: String, context: String },
    /// No Dockerfile: Railpack builds from source in this directory.
    Railpack { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// HTTP 2xx on this path, on the service's port.
    Http { path: String },
    /// A TCP connect to the service's port.
    Tcp,
    /// A Postgres protocol handshake.
    Postgres,
    /// A Redis `PING`.
    Redis,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Public {
    pub hostname: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    pub name: String,
    pub mount: String,
    pub backup: Option<Backup>,
}

/// How Ark takes a consistent snapshot of a volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backup {
    /// Copy the files as they are, pausing the container for the snapshot.
    Files,
    /// `sqlite3 .backup` of this database, relative to the volume root.
    Sqlite { path: String },
    /// `pg_dump` through the service itself.
    Postgres,
    /// `BGSAVE`, then the dump file.
    Redis,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resources {
    pub memory_bytes: u64,
    pub cpus: f64,
}

/// One reason a lease was refused. `path` points at the offending field,
/// `code` is stable so Ghost can be told exactly what to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub path: String,
    pub code: &'static str,
    pub message: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} ({})", self.path, self.message, self.code)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("lease is not valid TOML for this schema: {0}")]
    Syntax(#[from] toml::de::Error),
    #[error("lease refused: {}", list(.0))]
    Policy(Vec<Violation>),
}

fn list(v: &[Violation]) -> String {
    v.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ")
}

impl Lease {
    /// Parse and check a lease against the default limits.
    pub fn parse(text: &str) -> Result<Lease, Refusal> {
        Lease::parse_with(text, &Limits::default())
    }

    pub fn parse_with(text: &str, limits: &Limits) -> Result<Lease, Refusal> {
        let raw: raw::Lease = toml::from_str(text)?;
        check::check(raw, limits).map_err(Refusal::Policy)
    }

    pub fn service(&self, name: &str) -> Option<&Service> {
        self.services.iter().find(|s| s.name == name)
    }

    /// Services in an order where each comes after everything it depends on.
    /// A parsed lease has no cycles, so this always covers every service.
    pub fn start_order(&self) -> Vec<&Service> {
        let mut done: Vec<&str> = Vec::with_capacity(self.services.len());
        let mut order = Vec::with_capacity(self.services.len());
        while order.len() < self.services.len() {
            for s in &self.services {
                if !done.contains(&s.name.as_str()) && s.depends_on.iter().all(|d| done.contains(&d.as_str())) {
                    done.push(&s.name);
                    order.push(s);
                }
            }
        }
        order
    }

    /// Every public hostname this lease claims.
    pub fn hostnames(&self) -> impl Iterator<Item = &str> {
        self.services.iter().filter_map(|s| s.public.as_ref().map(|p| p.hostname.as_str()))
    }
}

#[cfg(test)]
mod tests;
