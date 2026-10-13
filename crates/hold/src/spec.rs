//! What a container should be. Plumb builds these from leases; Hold turns
//! them into engine calls.

use std::fmt;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::Labels;

#[derive(Clone, PartialEq, Serialize)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
    /// A secret's value never prints and never enters the spec hash.
    pub secret: bool,
}

impl fmt::Debug for EnvVar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.secret { write!(f, "{}=***", self.name) } else { write!(f, "{}={}", self.name, self.value) }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Mount {
    /// A named volume.
    pub volume: String,
    pub target: String,
}

/// Publish a container port on the host, where Caddy can reach it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Publish {
    pub container_port: u16,
    pub host_ip: String,
    pub host_port: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub command: Option<Vec<String>>,
    pub env: Vec<EnvVar>,
    /// Changes when any secret this container uses is set again, so a new
    /// value recreates the container without the value entering the hash.
    pub secret_stamp: String,
    pub labels: Labels,
    pub network: String,
    pub aliases: Vec<String>,
    pub mounts: Vec<Mount>,
    pub publish: Option<Publish>,
    pub memory_bytes: u64,
    pub cpus: f64,
}

/// The parts of a spec that decide whether a running container is still
/// the right one. Secret values are replaced by the stamp.
#[derive(Serialize)]
struct Hashed<'a> {
    image: &'a str,
    command: &'a Option<Vec<String>>,
    env: Vec<(&'a str, Option<&'a str>)>,
    secret_stamp: &'a str,
    labels: &'a Labels,
    network: &'a str,
    mounts: &'a [Mount],
    publish: &'a Option<Publish>,
    memory_bytes: u64,
    cpus: String,
}

impl ContainerSpec {
    /// A short, stable fingerprint. A running container whose label differs
    /// from this is stale and gets recreated. Aliases are left out: they
    /// change in place, with `Runtime::set_aliases`, when a release goes live.
    pub fn hash(&self) -> String {
        let mut env: Vec<_> =
            self.env.iter().map(|e| (e.name.as_str(), (!e.secret).then_some(e.value.as_str()))).collect();
        env.sort();
        let hashed = Hashed {
            image: &self.image,
            command: &self.command,
            env,
            secret_stamp: &self.secret_stamp,
            labels: &self.labels,
            network: &self.network,
            mounts: &self.mounts,
            publish: &self.publish,
            memory_bytes: self.memory_bytes,
            cpus: format!("{:.3}", self.cpus),
        };
        let bytes = serde_json::to_vec(&hashed).expect("a spec always serializes");
        hex::encode(&Sha256::digest(&bytes)[..8])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ContainerSpec {
        ContainerSpec {
            name: "draw-api-r1".into(),
            image: "draw:abc".into(),
            command: None,
            env: vec![
                EnvVar { name: "LOG".into(), value: "info".into(), secret: false },
                EnvVar { name: "TOKEN".into(), value: "hunter2".into(), secret: true },
            ],
            secret_stamp: "1".into(),
            labels: Labels::new(),
            network: "henry-draw".into(),
            aliases: vec![],
            mounts: vec![],
            publish: None,
            memory_bytes: 512 << 20,
            cpus: 1.0,
        }
    }

    #[test]
    fn the_hash_follows_what_matters() {
        let a = spec();
        assert_eq!(a.hash(), spec().hash());
        assert_eq!(a.hash().len(), 16);

        let mut b = spec();
        b.image = "draw:def".into();
        assert_ne!(a.hash(), b.hash());

        let mut c = spec();
        c.env[0].value = "debug".into();
        assert_ne!(a.hash(), c.hash());

        let mut d = spec();
        d.env.reverse();
        assert_eq!(a.hash(), d.hash(), "env order does not matter");

        let mut e = spec();
        e.aliases = vec!["web".into()];
        assert_eq!(a.hash(), e.hash(), "aliases move in place, so they do not recreate");
    }

    #[test]
    fn secret_values_stay_out_of_the_hash_and_debug() {
        let a = spec();
        let mut b = spec();
        b.env[1].value = "different".into();
        assert_eq!(a.hash(), b.hash(), "the stamp, not the value, tracks a secret");
        b.secret_stamp = "2".into();
        assert_ne!(a.hash(), b.hash());
        assert!(!format!("{a:?}").contains("hunter2"));
    }
}
