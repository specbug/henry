//! The plan Sound can stand behind without AI, and the gaps it guessed at.
//!
//! The most deterministic source wins: the repo's own `lease.toml`, then its
//! compose file, then a Dockerfile, then a language manifest Railpack can
//! build. Backing services the app seems to need (Postgres, Redis) are added
//! with generated credentials, so an app gets its own private database.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::{ComposeService, Need, Survey, image_kind, looks_secret};

/// Where an app is going: its name, branch and public hostname.
#[derive(Debug, Clone)]
pub struct Ask<'a> {
    pub app: &'a str,
    pub branch: &'a str,
    pub hostname: Option<&'a str>,
}

/// A secret Henry makes up itself. `template` may name other generated
/// secrets in braces, e.g. `postgres://app:{POSTGRES_PASSWORD}@db:5432/app`;
/// without one, the value is 32 random bytes in hex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Generate {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

/// A proposed way to run an app: a lease, any files Henry must add to the
/// checkout (under `.henry/`), and the secrets Henry should generate.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub lease: String,
    pub files: Vec<(String, String)>,
    pub generate: Vec<Generate>,
    pub notes: String,
}

/// Where Henry's own files go in a checkout. Nothing else may be written.
pub const OVERLAY_DIR: &str = ".henry/";
const MAX_FILES: usize = 8;
const MAX_FILE_BYTES: usize = 64 * 1024;

impl Plan {
    /// Check a plan before anything runs: the lease passes policy and fits
    /// the ask, files stay under `.henry/`, and every generated secret is one
    /// the lease asks for. The error is written for whoever wrote the plan.
    pub fn check(&self, ask: &Ask<'_>, limits: &lease::Limits) -> Result<lease::Lease, String> {
        let lease = lease::Lease::parse_with(&self.lease, limits).map_err(|e| format!("the lease was refused: {e}"))?;
        let mut problems = Vec::new();
        if lease.name != ask.app {
            problems.push(format!("the lease must be named `{}`, not `{}`", ask.app, lease.name));
        }
        if lease.branch != ask.branch {
            problems.push(format!("the lease must deploy branch `{}`, not `{}`", ask.branch, lease.branch));
        }
        if let Some(host) = ask.hostname
            && !lease.services.iter().any(|s| s.public.as_ref().is_some_and(|p| p.hostname == host))
        {
            problems.push(format!("one service must be public at `{host}`"));
        }
        if self.files.len() > MAX_FILES {
            problems.push(format!("at most {MAX_FILES} files"));
        }
        for (path, content) in &self.files {
            let inside = path.starts_with(OVERLAY_DIR)
                && path.len() > OVERLAY_DIR.len()
                && !path.split('/').any(|p| p == ".." || p.is_empty() || p == ".");
            if !inside {
                problems.push(format!("`{path}`: files may only be added under {OVERLAY_DIR}"));
            }
            if content.len() > MAX_FILE_BYTES {
                problems.push(format!("`{path}` is over {} KiB", MAX_FILE_BYTES / 1024));
            }
        }
        for s in &lease.services {
            if let lease::Build::Dockerfile { path, .. } = &s.build
                && path.starts_with(OVERLAY_DIR)
                && !self.files.iter().any(|(p, _)| p == path)
            {
                problems.push(format!("`{}` builds from `{path}`, which the plan does not include", s.name));
            }
        }
        let wanted: BTreeSet<&str> = lease.services.iter().flat_map(|s| s.secrets.iter().map(String::as_str)).collect();
        let generated: BTreeSet<&str> = self.generate.iter().map(|g| g.name.as_str()).collect();
        for g in &self.generate {
            if !wanted.contains(g.name.as_str()) {
                problems.push(format!("`{}` is generated but no service lists it in `secrets`", g.name));
            }
            for name in g.template.as_deref().map(placeholders).unwrap_or_default() {
                if !generated.contains(name) || name == g.name {
                    problems.push(format!(
                        "`{}`'s template uses {{{name}}}, which is not another generated secret",
                        g.name
                    ));
                }
            }
        }
        if problems.is_empty() { Ok(lease) } else { Err(problems.join("; ")) }
    }
}

/// The `{NAME}` placeholders in a template.
pub fn placeholders(template: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let Some(len) = rest[start + 1..].find('}') else { break };
        out.push(&rest[start + 1..start + 1 + len]);
        rest = &rest[start + 1 + len + 1..];
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkeletonSource {
    /// The repo's own `lease.toml`, used as is.
    Repo,
    /// Drafted by Sound from what the repo has.
    Sound,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Skeleton {
    pub plan: Plan,
    pub source: SkeletonSource,
    /// What Sound guessed, for Ghost to check. Empty means nothing was.
    pub gaps: Vec<String>,
}

/// One service as it will be written into the lease.
#[derive(Debug, Default)]
struct Svc {
    name: String,
    build: String,
    port: Option<u16>,
    command: Option<Vec<String>>,
    health: Option<String>,
    public: bool,
    env: BTreeMap<String, String>,
    secrets: BTreeSet<String>,
    volumes: Vec<(String, String, Option<&'static str>)>,
    depends_on: BTreeSet<String>,
}

impl Survey {
    /// The best plan Sound can make without AI, or none when the repo gives
    /// it nothing to build from.
    pub fn skeleton(&self, ask: &Ask<'_>) -> Option<Skeleton> {
        if let Some(text) = &self.lease {
            let plan = Plan { lease: text.clone(), notes: "the repo's own lease.toml".into(), ..Plan::default() };
            return Some(Skeleton { plan, source: SkeletonSource::Repo, gaps: vec![] });
        }
        let mut gaps = Vec::new();
        let mut services = if let Some(c) = self.compose.as_ref().filter(|c| !c.services.is_empty()) {
            from_compose(&c.services, &mut gaps)
        } else {
            vec![self.dockerfile_service(&mut gaps).or_else(|| self.stack_service(&mut gaps))?]
        };
        if services.is_empty() {
            return None;
        }

        let mut generate = Vec::new();
        let main = main_service(&services);
        self.add_backing(&mut services, main.as_deref(), &mut generate, &mut gaps);
        // A compose file already says which service gets which variable: only
        // a service that loads an `env_file` gets what the repo documents.
        let configured: Vec<String> = match &self.compose {
            Some(c) if !c.services.is_empty() => {
                c.services.iter().filter(|c| c.env_file).map(|c| lease_name(&c.name)).collect()
            }
            _ => main.iter().cloned().collect(),
        };
        for name in &configured {
            if let Some(svc) = services.iter_mut().find(|s| &s.name == name) {
                self.add_config(svc, &mut gaps);
            }
        }
        if let Some(main) = &main {
            let svc = services.iter_mut().find(|s| &s.name == main).expect("the main service exists");
            if let Some(host) = ask.hostname {
                if svc.port.is_some() {
                    svc.public = true;
                    if svc.health.as_deref().is_none_or(|h| !h.starts_with("http")) {
                        svc.health = Some("http = \"/\"".into());
                        gaps.push(format!("`{}` is probed at GET / ; a real health path would prove more", svc.name));
                    }
                } else {
                    gaps.push(format!("{host} needs a service with a port; none was found"));
                }
            }
        }

        let lease = write_lease(ask, &services);
        let notes = format!("drafted by Sound from the repo; {} guess(es)", gaps.len());
        Some(Skeleton { plan: Plan { lease, files: vec![], generate, notes }, source: SkeletonSource::Sound, gaps })
    }

    fn dockerfile_service(&self, gaps: &mut Vec<String>) -> Option<Svc> {
        let root = ["Dockerfile", "Containerfile"];
        let d = self
            .dockerfiles
            .iter()
            .find(|d| root.contains(&d.path.as_str()))
            .or_else(|| (self.dockerfiles.len() == 1).then(|| &self.dockerfiles[0]))?;
        let context = d.path.rsplit_once('/').map_or(".", |(dir, _)| dir);
        let mut svc = Svc {
            name: "web".into(),
            build: format!("{{ dockerfile = {}, context = {} }}", q(&d.path), q(context)),
            ..Svc::default()
        };
        match d.expose.first() {
            Some(&p) => svc.port = Some(p),
            None => {
                svc.port = Some(8080);
                svc.env.insert("PORT".into(), "8080".into());
                gaps.push(format!("{} has no EXPOSE; guessed port 8080 and set PORT", d.path));
            }
        }
        svc.health = Some("tcp = true".into());
        Some(svc)
    }

    fn stack_service(&self, gaps: &mut Vec<String>) -> Option<Svc> {
        let s = self.stacks.iter().find(|s| s.dir == ".").or_else(|| self.stacks.first())?;
        let mut svc = Svc {
            name: "web".into(),
            build: format!("{{ railpack = {} }}", q(&s.dir)),
            port: Some(8080),
            health: Some("tcp = true".into()),
            ..Svc::default()
        };
        svc.env.insert("PORT".into(), "8080".into());
        gaps.push(format!(
            "no Dockerfile: Railpack builds {} from {}; the app is assumed to listen on $PORT (8080)",
            s.kind.as_str(),
            s.manifest
        ));
        Some(svc)
    }

    /// Postgres and Redis, with generated credentials and connection URLs.
    fn add_backing(
        &self,
        services: &mut Vec<Svc>,
        main: Option<&str>,
        generate: &mut Vec<Generate>,
        gaps: &mut Vec<String>,
    ) {
        let has = |services: &Vec<Svc>, need: Need| {
            services.iter().find(|s| svc_kind(s) == Some(need)).map(|s| s.name.clone())
        };
        // A database URL the repo spells out for another engine (SQLite, say)
        // means the code's Postgres support is not what this app uses.
        let other_db = services.iter().find_map(|s| {
            ["DATABASE_URL", "POSTGRES_URL"]
                .iter()
                .find_map(|k| s.env.get(*k).filter(|v| !v.starts_with("postgres")).map(|v| (s.name.clone(), v.clone())))
        });
        if let Some((svc, url)) = &other_db
            && self.needs.contains(&Need::Postgres)
        {
            let scheme = url.split(':').next().unwrap_or_default();
            gaps.push(format!("`{svc}` is given a {scheme} database by the repo, so no Postgres was added"));
        }
        if self.needs.contains(&Need::Postgres) && other_db.is_none() {
            let db = match has(services, Need::Postgres) {
                Some(name) => name,
                None => {
                    let name = free_name(services, "db");
                    let mut env = BTreeMap::new();
                    env.insert("POSTGRES_USER".into(), "app".into());
                    env.insert("POSTGRES_DB".into(), "app".into());
                    services.push(Svc {
                        name: name.clone(),
                        build: "{ image = \"postgres:17-alpine\" }".into(),
                        port: Some(5432),
                        health: Some("postgres = true".into()),
                        env,
                        volumes: vec![("pgdata".into(), "/var/lib/postgresql/data".into(), Some("postgres"))],
                        ..Svc::default()
                    });
                    name
                }
            };
            let svc = services.iter_mut().find(|s| s.name == db).expect("just found or added");
            svc.secrets.insert("POSTGRES_PASSWORD".into());
            svc.env.remove("POSTGRES_PASSWORD");
            let user = svc.env.get("POSTGRES_USER").cloned().unwrap_or_else(|| "postgres".into());
            let database = svc.env.get("POSTGRES_DB").cloned().unwrap_or_else(|| user.clone());
            push_generate(generate, "POSTGRES_PASSWORD", None);
            if let Some(main) = main.filter(|m| *m != db) {
                let url = if self.env.contains_key("POSTGRES_URL") { "POSTGRES_URL" } else { "DATABASE_URL" };
                push_generate(
                    generate,
                    url,
                    Some(format!("postgres://{user}:{{POSTGRES_PASSWORD}}@{db}:5432/{database}")),
                );
                let m = services.iter_mut().find(|s| s.name == main).expect("the main service exists");
                m.env.remove(url);
                m.secrets.insert(url.into());
                m.depends_on.insert(db.clone());
                gaps.push(format!("`{main}` gets a private Postgres as {url}"));
            }
        }
        if self.needs.contains(&Need::Redis) {
            let cache = match has(services, Need::Redis) {
                Some(name) => name,
                None => {
                    let name = free_name(services, "cache");
                    services.push(Svc {
                        name: name.clone(),
                        build: "{ image = \"redis:7-alpine\" }".into(),
                        port: Some(6379),
                        health: Some("redis = true".into()),
                        ..Svc::default()
                    });
                    name
                }
            };
            if let Some(main) = main.filter(|m| *m != cache) {
                push_generate(generate, "REDIS_URL", Some(format!("redis://{cache}:6379")));
                let m = services.iter_mut().find(|s| s.name == main).expect("the main service exists");
                m.env.remove("REDIS_URL");
                m.secrets.insert("REDIS_URL".into());
                m.depends_on.insert(cache);
            }
        }
    }

    /// Config the authors documented: secrets by name, plain values as env.
    fn add_config(&self, svc: &mut Svc, gaps: &mut Vec<String>) {
        let mut unknown = Vec::new();
        for (name, var) in &self.env {
            if svc.env.contains_key(name) || svc.secrets.contains(name) || is_generated_url(name) {
                continue;
            }
            if var.documented && var.secret {
                svc.secrets.insert(name.clone());
            } else if let Some(example) = var.example.as_ref().filter(|_| var.documented) {
                svc.env.insert(name.clone(), example.clone());
            } else {
                unknown.push(name.as_str());
            }
        }
        if !unknown.is_empty() {
            gaps.push(format!("the code reads {} without documenting them; left unset", unknown.join(", ")));
        }
    }
}

fn is_generated_url(name: &str) -> bool {
    ["DATABASE_URL", "POSTGRES_URL", "REDIS_URL"].contains(&name)
}

fn push_generate(generate: &mut Vec<Generate>, name: &str, template: Option<String>) {
    if !generate.iter().any(|g| g.name == name) {
        generate.push(Generate { name: name.into(), template });
    }
}

fn svc_kind(s: &Svc) -> Option<Need> {
    match s.health.as_deref() {
        Some("postgres = true") => Some(Need::Postgres),
        Some("redis = true") => Some(Need::Redis),
        _ => None,
    }
}

fn free_name(services: &[Svc], want: &str) -> String {
    (0..)
        .map(|i| if i == 0 { want.to_owned() } else { format!("{want}-{i}") })
        .find(|n| !services.iter().any(|s| &s.name == n))
        .expect("an unbounded range finds a free name")
}

/// The service a hostname should point at: the conventional web names
/// first, then the first service with a port that is not a database.
fn main_service(services: &[Svc]) -> Option<String> {
    let candidates: Vec<&Svc> = services.iter().filter(|s| svc_kind(s).is_none()).collect();
    ["web", "app", "frontend", "server", "api", "site"]
        .iter()
        .find_map(|n| candidates.iter().find(|s| &s.name == n && s.port.is_some()))
        .or_else(|| candidates.iter().find(|s| s.port.is_some()))
        .or_else(|| candidates.first())
        .map(|s| s.name.clone())
}

fn lease_name(name: &str) -> String {
    let n: String =
        name.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    n.trim_matches('-').to_owned()
}

fn from_compose(compose: &[ComposeService], gaps: &mut Vec<String>) -> Vec<Svc> {
    let mut out = Vec::new();
    let kept: BTreeSet<String> =
        compose.iter().filter(|c| c.image.is_some() || c.build.is_some()).map(|c| lease_name(&c.name)).collect();
    for c in compose {
        let name = lease_name(&c.name);
        if c.image.as_deref().is_some_and(|i| i.contains("cloudflared")) {
            gaps.push(format!("`{name}` is the app's own tunnel; Henry's tunnel and proxy take its place"));
            continue;
        }
        let kind = c.image.as_deref().and_then(image_kind);
        let build = match (&c.build, &c.image) {
            (Some((context, dockerfile)), _) => {
                format!("{{ dockerfile = {}, context = {} }}", q(dockerfile), q(context))
            }
            (None, Some(image)) if image.contains('$') => {
                gaps.push(format!("`{name}` uses an interpolated image `{image}`; skipped"));
                continue;
            }
            (None, Some(image)) => format!("{{ image = {} }}", q(image)),
            (None, None) => continue,
        };
        let port = c.ports.first().copied().or(match kind {
            Some(Need::Postgres) => Some(5432),
            Some(Need::Redis) => Some(6379),
            None => None,
        });
        let Some(port) = port else {
            gaps.push(format!(
                "`{name}` has no port; a lease cannot run a service Henry cannot probe, so it was left out"
            ));
            continue;
        };
        let health = match (kind, &c.health) {
            (Some(Need::Postgres), _) => "postgres = true".to_owned(),
            (Some(Need::Redis), _) => "redis = true".to_owned(),
            // The repo's own healthcheck, if it asks this port over HTTP.
            (None, Some((p, path))) if *p == port => format!("http = {}", q(path)),
            (None, _) => "tcp = true".to_owned(),
        };
        let mut svc = Svc {
            name: name.clone(),
            build,
            port: Some(port),
            command: c.command.clone(),
            health: Some(health),
            ..Svc::default()
        };
        for (k, v) in &c.env {
            match v {
                // Written into the repo, so not a secret; a database URL is
                // kept only when it carries no credentials.
                Some(v) if !looks_secret(k) || (is_generated_url(k) && !v.contains('@')) => {
                    svc.env.insert(k.clone(), v.clone());
                }
                _ if k == "POSTGRES_PASSWORD" || is_generated_url(k) => {} // generated below
                _ => {
                    svc.secrets.insert(k.clone());
                }
            }
        }
        for (vol, mount) in &c.volumes {
            let backup = match kind {
                Some(Need::Postgres) => Some("postgres"),
                Some(Need::Redis) => Some("redis"),
                None => Some("files"),
            };
            svc.volumes.push((lease_name(vol), mount.clone(), backup));
        }
        if c.bind_mounts {
            gaps.push(format!("`{name}` bind-mounts host paths, which a lease does not allow; dropped"));
        }
        svc.depends_on = c.depends_on.iter().map(|d| lease_name(d)).filter(|d| kept.contains(d)).collect();
        out.push(svc);
    }
    // A dependency on a dropped service would refuse the lease.
    let names: BTreeSet<String> = out.iter().map(|s| s.name.clone()).collect();
    for s in &mut out {
        s.depends_on.retain(|d| names.contains(d));
    }
    out
}

/// TOML string quoting.
fn q(s: &str) -> String {
    toml::Value::String(s.to_owned()).to_string()
}

fn write_lease(ask: &Ask<'_>, services: &[Svc]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "name = {}\nbranch = {}\n", q(ask.app), q(ask.branch));
    for s in services {
        let _ = writeln!(out, "[[service]]\nname = {}", q(&s.name));
        if let Some(p) = s.port {
            let _ = writeln!(out, "port = {p}");
        }
        let _ = writeln!(out, "build = {}", s.build);
        if let Some(cmd) = &s.command {
            let items: Vec<String> = cmd.iter().map(|c| q(c)).collect();
            let _ = writeln!(out, "command = [{}]", items.join(", "));
        }
        if let Some(h) = &s.health {
            let _ = writeln!(out, "health = {{ {h} }}");
        }
        if s.public
            && let Some(host) = ask.hostname
        {
            let _ = writeln!(out, "public = {{ hostname = {} }}", q(host));
        }
        if !s.env.is_empty() {
            let items: Vec<String> = s.env.iter().map(|(k, v)| format!("{k} = {}", q(v))).collect();
            let _ = writeln!(out, "env = {{ {} }}", items.join(", "));
        }
        if !s.secrets.is_empty() {
            let items: Vec<String> = s.secrets.iter().map(|k| q(k)).collect();
            let _ = writeln!(out, "secrets = [{}]", items.join(", "));
        }
        if !s.depends_on.is_empty() {
            let items: Vec<String> = s.depends_on.iter().map(|k| q(k)).collect();
            let _ = writeln!(out, "depends_on = [{}]", items.join(", "));
        }
        for (name, mount, backup) in &s.volumes {
            let _ = write!(out, "\n[[service.volume]]\nname = {}\nmount = {}\n", q(name), q(mount));
            if let Some(b) = backup {
                let _ = writeln!(out, "backup = {}", q(b));
            }
        }
        out.push('\n');
    }
    out
}
