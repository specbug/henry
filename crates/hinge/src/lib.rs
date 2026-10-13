//! Hinge: what every request turns on.
//!
//! A request arrives at Cloudflare, comes down the tunnel to cloudflared,
//! and Caddy sends it to the container. Hinge keeps both ends in line with
//! Canon: Caddy's routes, through its admin API, and the tunnel's ingress
//! and DNS, through Cloudflare's. It never carries a request itself, so Henry
//! can restart without dropping one.
//!
//! Only live releases are routed. A release on trial has no public route
//! until Proof accepts it.

pub mod caddy;
pub mod cloudflare;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use canon::{Canon, Desired, serde_json};
use lease::Lease;
use tracing::{info, warn};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Canon(#[from] canon::Error),
    #[error("{0} is unreachable: {1}")]
    Unreachable(&'static str, String),
    #[error("{0} refused: {1}")]
    Refused(&'static str, String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The owner of Henry's own routes.
pub const HENRY: &str = "henry";

/// One public route: requests for `host` under `path` go to `upstream`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Route {
    pub host: String,
    pub path: String,
    /// A port on the loopback interface.
    pub upstream: u16,
    /// The app it belongs to, or `henry` for Henry's own endpoints.
    pub owner: String,
}

/// The local proxy that holds the routes.
pub trait Proxy: Send + Sync {
    /// Make the proxy route exactly these. Returns whether anything changed.
    fn apply(&self, routes: &[Route]) -> impl Future<Output = Result<bool>> + Send;
}

/// The tunnel that brings public hostnames to the proxy.
pub trait Tunnel: Send + Sync {
    /// Send every host to `origin`, with a 404 for anything else.
    fn set_ingress(&self, hosts: &[String], origin: &str) -> impl Future<Output = Result<()>> + Send;
    /// Make each host resolve to the tunnel. Remove only records Henry made.
    fn sync_dns(&self, hosts: &[String]) -> impl Future<Output = Result<cloudflare::DnsReport>> + Send;
}

#[derive(Debug, Clone)]
pub struct Settings {
    /// Where Caddy listens for the tunnel, e.g. `http://127.0.0.1:8080`.
    pub origin: String,
    /// Henry's own routes (the webhook path, later the UI).
    pub own: Vec<Route>,
    /// How long a new upstream gets to accept a connection before a route
    /// is moved to it.
    pub gate: Duration,
}

/// What one pass changed.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub proxy_changed: bool,
    pub tunnel_changed: bool,
    pub dns: cloudflare::DnsReport,
    /// Routes left on their old upstream because the new one did not answer.
    pub held: Vec<String>,
}

/// The routes Canon implies: every public service of every live release of
/// every app that should be running.
pub fn desired(canon: &Canon) -> Result<Vec<Route>> {
    let mut routes = Vec::new();
    for app in canon.apps()? {
        if app.desired == Desired::Stopped {
            continue;
        }
        let Some(live) = app.live_release else { continue };
        let release = canon.release(live)?;
        let Ok(lease) = Lease::parse(&release.lease) else {
            warn!(app = %app.name, "the live release's lease no longer parses; it keeps no route");
            continue;
        };
        for service in &lease.services {
            let Some(public) = &service.public else { continue };
            // Plumb's slots: the release for a stateless service, 0 for a stateful one.
            let slot = if service.volumes.is_empty() { live } else { 0 };
            match canon.existing_port(&app.name, &service.name, slot)? {
                Some(upstream) => routes.push(Route {
                    host: public.hostname.clone(),
                    path: public.path.clone(),
                    upstream,
                    owner: app.name.clone(),
                }),
                None => warn!(app = %app.name, service = %service.name, "live but not yet published by Plumb"),
            }
        }
    }
    routes.sort();
    Ok(routes)
}

#[derive(Default)]
struct Applied {
    routes: Vec<Route>,
    hosts: Option<Vec<String>>,
}

pub struct Hinge<P, T> {
    canon: Arc<Canon>,
    proxy: P,
    tunnel: Option<T>,
    settings: Settings,
    applied: Mutex<Applied>,
}

impl<P: Proxy, T: Tunnel> Hinge<P, T> {
    pub fn new(canon: Arc<Canon>, proxy: P, tunnel: Option<T>, settings: Settings) -> Self {
        Hinge { canon, proxy, tunnel, settings, applied: Mutex::new(Applied::default()) }
    }

    /// One pass: routes to the proxy, then hosts to the tunnel and DNS.
    pub async fn pass(&self) -> Result<Report> {
        let mut report = Report::default();
        let mut wanted = self.settings.own.clone();
        // An app may not claim a hostname Henry serves its own endpoints on.
        let own: BTreeSet<&str> = self.settings.own.iter().map(|r| r.host.as_str()).collect();
        for r in desired(&self.canon)? {
            if own.contains(r.host.as_str()) {
                warn!(app = %r.owner, host = %r.host, "not routed: the hostname is Henry's own");
            } else {
                wanted.push(r);
            }
        }

        // The gate: a route moves to a new upstream only once something is
        // listening there. Until then it stays where it was.
        let previous: BTreeMap<(String, String), u16> = {
            let applied = self.applied.lock().unwrap();
            applied.routes.iter().map(|r| ((r.host.clone(), r.path.clone()), r.upstream)).collect()
        };
        let mut routes = Vec::with_capacity(wanted.len());
        for mut route in wanted {
            let key = (route.host.clone(), route.path.clone());
            if let Some(&old) = previous.get(&key)
                && old != route.upstream
                && !answers(route.upstream, self.settings.gate).await
            {
                report.held.push(format!("{}{}", route.host, route.path));
                route.upstream = old;
            }
            routes.push(route);
        }
        routes.sort();

        report.proxy_changed = self.proxy.apply(&routes).await?;
        if report.proxy_changed {
            let summary: Vec<String> =
                routes.iter().map(|r| format!("{}{} → :{}", r.host, r.path, r.upstream)).collect();
            info!(routes = ?summary, "routes updated");
            self.canon.record(
                None,
                "hinge.routes",
                &format!("{} route(s) applied", routes.len()),
                &serde_json::json!(summary),
            )?;
        }

        let hosts: Vec<String> = routes.iter().map(|r| r.host.clone()).collect::<BTreeSet<_>>().into_iter().collect();
        if let Some(tunnel) = &self.tunnel {
            let unchanged = self.applied.lock().unwrap().hosts.as_ref() == Some(&hosts);
            if !unchanged {
                tunnel.set_ingress(&hosts, &self.settings.origin).await?;
                report.dns = tunnel.sync_dns(&hosts).await?;
                report.tunnel_changed = true;
                self.canon.record(
                    None,
                    "hinge.tunnel",
                    &format!(
                        "tunnel serves {} host(s); DNS: {} created, {} removed",
                        hosts.len(),
                        report.dns.created.len(),
                        report.dns.deleted.len()
                    ),
                    &serde_json::json!({ "hosts": hosts, "dns": { "created": report.dns.created, "deleted": report.dns.deleted, "skipped": report.dns.skipped } }),
                )?;
                for (host, why) in &report.dns.skipped {
                    warn!(%host, "DNS left alone: {why}");
                }
            }
        }

        let mut applied = self.applied.lock().unwrap();
        applied.routes = routes;
        if self.tunnel.is_some() {
            applied.hosts = Some(hosts);
        }
        Ok(report)
    }
}

/// Whether something accepts connections on this loopback port.
async fn answers(port: u16, within: Duration) -> bool {
    matches!(tokio::time::timeout(within, tokio::net::TcpStream::connect(("127.0.0.1", port))).await, Ok(Ok(_)))
}

#[cfg(test)]
mod tests;
