//! The lease as written, before any checks. Every struct rejects unknown
//! fields: a typo or an invented `privileged = true` is a refusal, not a
//! silent no-op.

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub name: String,
    pub branch: Option<String>,
    pub auto_deploy: Option<bool>,
    pub ai: Option<Ai>,
    #[serde(default, rename = "service")]
    pub services: Vec<Service>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ai {
    /// `provider/model`, e.g. `claude/claude-sonnet-5-5`.
    pub model: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub name: String,
    pub build: Build,
    pub port: Option<u16>,
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub secrets: Vec<String>,
    pub health: Option<Health>,
    pub public: Option<Public>,
    #[serde(default, rename = "volume")]
    pub volumes: Vec<Volume>,
    #[serde(default)]
    pub resources: Resources,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Build {
    pub image: Option<String>,
    pub dockerfile: Option<String>,
    pub context: Option<String>,
    pub railpack: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Health {
    pub http: Option<String>,
    pub tcp: Option<bool>,
    pub postgres: Option<bool>,
    pub redis: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Public {
    pub hostname: String,
    pub path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Volume {
    pub name: String,
    pub mount: String,
    pub backup: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub memory: Option<String>,
    pub cpus: Option<f64>,
}
