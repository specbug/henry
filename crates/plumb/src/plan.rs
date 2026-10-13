//! What should be running, worked out from Canon alone. No engine calls
//! happen here, so the plan is easy to test and easy to explain.
//!
//! The rule that keeps data safe: a service with volumes is never run twice.
//! Stateless services get one container per wanted release, so a new
//! release can run beside the live one. A stateful service gets exactly one
//! container, named without a release, built from the newest wanted release
//! and replaced in place when that changes.

use std::collections::BTreeMap;

use canon::{App, Canon, Desired, Release};
use hold::{ContainerSpec, EnvVar, Labels, Mount, Publish, label};
use hush::Hush;
use lease::{Build, Lease, Service};

use crate::{Error, Result, Settings};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The release Proof accepted.
    Live,
    /// A newer release being tried beside it.
    Candidate,
    /// A service with volumes: one container, whatever the release.
    Stateful,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Live => "live",
            Role::Candidate => "candidate",
            Role::Stateful => "stateful",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Planned {
    pub spec: ContainerSpec,
    pub service: String,
    pub release: i64,
    pub role: Role,
    /// Port slot: the release id, or 0 for a stateful service.
    pub slot: i64,
    /// Whether the app counts as up only when this one runs: everything that
    /// serves the live release.
    pub serves_live: bool,
    /// Services that must be running before this one is started.
    pub depends_on: Vec<String>,
}

impl Planned {
    /// Who this container is to its dependents: a stateful service is the
    /// same container for every release.
    pub fn key(&self) -> (i64, &str) {
        (if self.role == Role::Stateful { 0 } else { self.release }, &self.service)
    }
}

#[derive(Debug, Clone)]
pub struct AppPlan {
    pub app: String,
    pub desired: Desired,
    pub network: String,
    pub volumes: Vec<String>,
    /// In start order: each container after everything it depends on.
    pub containers: Vec<Planned>,
    /// Secrets the app needs and Hush does not have. While any are missing,
    /// nothing new starts and nothing running is touched.
    pub missing_secrets: Vec<String>,
    pub has_live: bool,
    /// Things Henry cannot run as written, e.g. a release with no image.
    pub problems: Vec<String>,
}

impl AppPlan {
    pub fn frozen(&self) -> bool {
        !self.missing_secrets.is_empty()
    }
}

pub fn network_name(app: &str) -> String {
    format!("henry-{app}")
}

pub fn volume_name(app: &str, volume: &str) -> String {
    format!("henry-{app}-{volume}")
}

pub fn container_name(app: &str, service: &str, release: Option<i64>) -> String {
    match release {
        Some(r) => format!("{app}-{service}-r{r}"),
        None => format!("{app}-{service}"),
    }
}

/// `HENRY_<NAME>_HOST`, the variable a peer's hostname arrives in.
fn host_var(service: &str) -> String {
    format!("HENRY_{}_HOST", service.to_ascii_uppercase().replace('-', "_"))
}

pub fn plan_app(canon: &Canon, hush: &Hush, app: &App, settings: &Settings) -> Result<AppPlan> {
    let mut plan = AppPlan {
        app: app.name.clone(),
        desired: app.desired,
        network: network_name(&app.name),
        volumes: Vec::new(),
        containers: Vec::new(),
        missing_secrets: Vec::new(),
        has_live: app.live_release.is_some(),
        problems: Vec::new(),
    };
    if app.desired == Desired::Stopped {
        return Ok(plan);
    }

    let wanted = canon.wanted_releases(&app.name)?;
    // Newest first, so a stateful service is built from the newest release.
    let releases: Vec<(Release, Role)> =
        [wanted.candidate.map(|r| (r, Role::Candidate)), wanted.live.map(|r| (r, Role::Live))]
            .into_iter()
            .flatten()
            .collect();
    if releases.is_empty() {
        return Ok(plan);
    }

    let mut needed: Vec<String> = Vec::new();
    let mut leases = Vec::with_capacity(releases.len());
    for (release, _) in &releases {
        let lease = Lease::parse(&release.lease).map_err(|e| Error::Lease(release.id, e.to_string()))?;
        for s in &lease.services {
            for name in &s.secrets {
                if !needed.contains(name) {
                    needed.push(name.clone());
                }
            }
        }
        leases.push(lease);
    }

    let provision = hush.provision(&app.name, &needed)?;
    if !provision.missing.is_empty() {
        plan.missing_secrets = provision.missing;
        return Ok(plan);
    }
    let secrets: BTreeMap<&str, &str> = provision.env.iter().map(|(n, v)| (n.as_str(), v.expose())).collect();
    let versions = canon.sealed_versions(&app.name)?;

    let mut stateful_done: Vec<String> = Vec::new();
    for ((release, role), lease) in releases.iter().zip(&leases) {
        for service in lease.start_order() {
            let stateful = !service.volumes.is_empty();
            if stateful {
                if stateful_done.contains(&service.name) {
                    continue;
                }
                stateful_done.push(service.name.clone());
            }
            let Some(image) = image_for(release, service) else {
                plan.problems.push(format!("release {} has no image for `{}` yet", release.id, service.name));
                continue;
            };
            let role = if stateful { Role::Stateful } else { *role };
            let serves_live = role == Role::Live || (role == Role::Stateful && plan.has_live);
            let slot = if stateful { 0 } else { release.id };
            let name = container_name(&app.name, &service.name, (!stateful).then_some(release.id));

            // Every service with a port is published on loopback: Hinge routes
            // the public ones, and Proof probes all of them from the host.
            let publish = match service.port {
                Some(port) => Some(Publish {
                    container_port: port,
                    host_ip: settings.publish_ip.clone(),
                    host_port: canon.port_for(&app.name, &service.name, slot, settings.ports.clone())?,
                }),
                None => None,
            };

            let mut env: Vec<EnvVar> =
                service.env.iter().map(|(k, v)| EnvVar { name: k.clone(), value: v.clone(), secret: false }).collect();
            if let Some(port) = service.port
                && !service.env.contains_key("PORT")
            {
                env.push(EnvVar { name: "PORT".into(), value: port.to_string(), secret: false });
            }
            // A stateful container outlives releases, so nothing in its spec
            // may name one: no release id, and no stateless peers (whose names
            // carry a release). Otherwise every push would restart the database.
            let mut injected =
                vec![("HENRY_APP".to_owned(), app.name.clone()), ("HENRY_SERVICE".to_owned(), service.name.clone())];
            if !stateful {
                injected.push(("HENRY_RELEASE".to_owned(), release.id.to_string()));
            }
            for peer in &lease.services {
                let peer_stateful = !peer.volumes.is_empty();
                if peer.name != service.name && (peer_stateful || !stateful) {
                    let peer_release = (!peer_stateful).then_some(release.id);
                    injected.push((host_var(&peer.name), container_name(&app.name, &peer.name, peer_release)));
                }
            }
            env.extend(injected.into_iter().map(|(name, value)| EnvVar { name, value, secret: false }));
            let mut stamp = Vec::new();
            for name in &service.secrets {
                let value = secrets.get(name.as_str()).copied().unwrap_or_default();
                env.push(EnvVar { name: name.clone(), value: value.to_owned(), secret: true });
                stamp.push(format!("{name}@{}", versions.get(name).copied().unwrap_or(0)));
            }

            let mut mounts = Vec::new();
            for v in &service.volumes {
                let volume = volume_name(&app.name, &v.name);
                if !plan.volumes.contains(&volume) {
                    plan.volumes.push(volume.clone());
                }
                mounts.push(Mount { volume, target: v.mount.clone() });
            }

            let mut labels = Labels::from([
                (label::APP.to_owned(), app.name.clone()),
                (label::SERVICE.to_owned(), service.name.clone()),
            ]);
            if !stateful {
                labels.insert(label::RELEASE.to_owned(), release.id.to_string());
            }
            // Live and stateful containers answer to the plain service name, so
            // an app that hardcodes `db` still finds it. A candidate does not,
            // so it never steals the live release's traffic.
            let aliases = if role == Role::Candidate { vec![] } else { vec![service.name.clone()] };

            plan.containers.push(Planned {
                spec: ContainerSpec {
                    name,
                    image,
                    command: service.command.clone(),
                    env,
                    secret_stamp: stamp.join(","),
                    labels,
                    network: plan.network.clone(),
                    aliases,
                    mounts,
                    publish,
                    memory_bytes: service.resources.memory_bytes,
                    cpus: service.resources.cpus,
                },
                service: service.name.clone(),
                release: release.id,
                role,
                slot,
                serves_live,
                depends_on: service.depends_on.clone(),
            });
        }
    }
    Ok(plan)
}

/// The image a release runs for a service: what its build produced, or the
/// published image the lease names.
fn image_for(release: &Release, service: &Service) -> Option<String> {
    if let Some(image) = release.images.get(&service.name).and_then(|v| v.as_str()) {
        return Some(image.to_owned());
    }
    match &service.build {
        Build::Image(image) => Some(image.clone()),
        _ => None,
    }
}
