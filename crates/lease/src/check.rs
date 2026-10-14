//! The Check step: turn a raw lease into a [`Lease`] or say exactly why
//! not. Every violation is collected, not just the first, so one round trip
//! to Ghost can fix them all.

use std::collections::{BTreeMap, BTreeSet};

use crate::raw;
use crate::size::{format_bytes, parse_bytes};
use crate::{
    AiPrefs, Backup, Build, Health, Lease, Provider, Public, Resources, SCRIPT_MAX, Service, Violation, Volume, Worker,
};

/// Ceilings a lease must stay under. The defaults suit one Mac mini.
#[derive(Debug, Clone)]
pub struct Limits {
    pub max_services: usize,
    pub max_memory_bytes: u64,
    pub max_cpus: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_services: 16, max_memory_bytes: 8 << 30, max_cpus: 8.0 }
    }
}

const DEFAULT_BRANCH: &str = "main";
const DEFAULT_MEMORY: u64 = 512 << 20;
const MIN_MEMORY: u64 = 16 << 20;
const DEFAULT_CPUS: f64 = 1.0;
const NAME_MAX: usize = 32;

/// The scope of Henry's own secrets, so no app may take the name.
const RESERVED_NAME: &str = "henry";

/// Henry injects variables under this prefix (service URLs and the like).
const RESERVED_PREFIX: &str = "HENRY_";

/// Paths no volume may be mounted over.
const FORBIDDEN_MOUNTS: [&str; 5] = ["/proc", "/sys", "/dev", "/run", "/tmp"];

struct Report(Vec<Violation>);

impl Report {
    fn add(&mut self, path: impl Into<String>, code: &'static str, message: impl Into<String>) {
        self.0.push(Violation { path: path.into(), code, message: message.into() });
    }
}

pub fn check(raw: raw::Lease, limits: &Limits) -> Result<Lease, Vec<Violation>> {
    let mut r = Report(Vec::new());

    if raw.name == RESERVED_NAME {
        r.add("name", "name_reserved", format!("`{RESERVED_NAME}` names Henry itself"));
    } else if !is_name(&raw.name) {
        r.add(
            "name",
            "name_invalid",
            format!(
                "`{}` must be 1-{NAME_MAX} chars of a-z, 0-9 and '-', starting with a letter and not ending with '-'",
                raw.name
            ),
        );
    }

    let branch = raw.branch.unwrap_or_else(|| DEFAULT_BRANCH.to_owned());
    if !is_branch(&branch) {
        r.add("branch", "branch_invalid", format!("`{branch}` is not a usable branch name"));
    }

    let ai = raw.ai.and_then(|ai| match ai.model.split_once('/') {
        Some((p, m)) if !m.is_empty() => match Provider::parse(p) {
            Some(provider) => Some(AiPrefs { provider, model: m.to_owned() }),
            None => {
                r.add(
                    "ai.model",
                    "ai_provider_unknown",
                    format!("`{p}` is not one of {}", Provider::ALL.map(Provider::as_str).join(", ")),
                );
                None
            }
        },
        _ => {
            r.add("ai.model", "ai_model_invalid", "write it as `provider/model`, e.g. `claude/claude-sonnet-5-5`");
            None
        }
    });

    if raw.services.is_empty() {
        r.add("service", "services_missing", "a lease needs at least one [[service]]");
    }
    if raw.services.len() > limits.max_services {
        r.add(
            "service",
            "services_too_many",
            format!("{} services is over the limit of {}", raw.services.len(), limits.max_services),
        );
    }

    let names: Vec<&str> = raw.services.iter().map(|s| s.name.as_str()).collect();
    let mut seen = BTreeSet::new();
    for n in &names {
        if !seen.insert(*n) {
            r.add(format!("service[{n}]"), "service_duplicate", format!("service `{n}` is defined twice"));
        }
    }

    let mut services = Vec::with_capacity(raw.services.len());
    for s in &raw.services {
        if let Some(svc) = check_service(s, &names, limits, &mut r) {
            services.push(svc);
        }
    }

    check_routes(&services, &mut r);
    check_shared_volumes(&services, &mut r);
    if r.0.is_empty() {
        check_cycles(&services, &mut r);
    }
    let workers = check_workers(&raw.name, &raw.workers, &mut r);

    if r.0.is_empty() {
        Ok(Lease { name: raw.name, branch, auto_deploy: raw.auto_deploy.unwrap_or(true), ai, services, workers })
    } else {
        Err(r.0)
    }
}

fn check_service(s: &raw::Service, all: &[&str], limits: &Limits, r: &mut Report) -> Option<Service> {
    let at = format!("service[{}]", s.name);
    let before = r.0.len();

    if !is_name(&s.name) {
        r.add(
            &at,
            "service_name_invalid",
            format!("`{}` must be 1-{NAME_MAX} chars of a-z, 0-9 and '-', starting with a letter", s.name),
        );
    }

    let build = check_build(&s.build, &format!("{at}.build"), r);

    if s.port == Some(0) {
        r.add(format!("{at}.port"), "port_invalid", "port 0 cannot be probed or routed");
    }

    if let Some(cmd) = &s.command
        && cmd.first().is_none_or(|c| c.trim().is_empty())
    {
        r.add(format!("{at}.command"), "command_empty", "a command needs at least a program name");
    }

    for key in s.env.keys() {
        check_var(key, &format!("{at}.env.{key}"), r);
    }
    let mut secrets = BTreeSet::new();
    for name in &s.secrets {
        check_var(name, &format!("{at}.secrets.{name}"), r);
        if !secrets.insert(name) {
            r.add(format!("{at}.secrets.{name}"), "secret_duplicate", format!("`{name}` is listed twice"));
        }
        if s.env.contains_key(name) {
            r.add(
                format!("{at}.secrets.{name}"),
                "secret_in_env",
                format!("`{name}` is both a plain env value and a secret; a secret's value never goes in the lease"),
            );
        }
    }

    let health = check_health(s.health.as_ref(), s.port, &format!("{at}.health"), r);
    let public = s.public.as_ref().and_then(|p| check_public(p, s, &format!("{at}.public"), r));
    let volumes = check_volumes(&s.volumes, &health, &at, r);
    let resources = check_resources(&s.resources, limits, &format!("{at}.resources"), r);

    let mut deps = BTreeSet::new();
    for d in &s.depends_on {
        if d == &s.name {
            r.add(format!("{at}.depends_on"), "depends_on_self", "a service cannot depend on itself");
        } else if !all.contains(&d.as_str()) {
            r.add(format!("{at}.depends_on"), "depends_on_unknown", format!("there is no service `{d}`"));
        } else if !deps.insert(d) {
            r.add(format!("{at}.depends_on"), "depends_on_duplicate", format!("`{d}` is listed twice"));
        }
    }

    if r.0.len() > before {
        return None;
    }
    Some(Service {
        name: s.name.clone(),
        build: build?,
        port: s.port,
        command: s.command.clone(),
        env: s.env.clone(),
        secrets: s.secrets.clone(),
        health: health?,
        public,
        volumes,
        resources: resources?,
        depends_on: s.depends_on.clone(),
    })
}

fn check_build(b: &raw::Build, at: &str, r: &mut Report) -> Option<Build> {
    let chosen = [b.image.is_some(), b.dockerfile.is_some(), b.railpack.is_some()].into_iter().filter(|x| *x).count();
    if chosen != 1 {
        r.add(at, "build_ambiguous", "set exactly one of `image`, `dockerfile` or `railpack`");
        return None;
    }
    if b.context.is_some() && b.dockerfile.is_none() {
        r.add(format!("{at}.context"), "build_context_unused", "`context` only applies to `dockerfile`");
        return None;
    }
    if let Some(image) = &b.image {
        if image.is_empty() || image.chars().any(char::is_whitespace) {
            r.add(format!("{at}.image"), "image_invalid", format!("`{image}` is not an image reference"));
            return None;
        }
        return Some(Build::Image(image.clone()));
    }
    if let Some(path) = &b.dockerfile {
        let context = b.context.clone().unwrap_or_else(|| ".".to_owned());
        let ok_path = check_repo_path(path, &format!("{at}.dockerfile"), r);
        let ok_ctx = check_repo_path(&context, &format!("{at}.context"), r);
        return (ok_path && ok_ctx).then(|| Build::Dockerfile { path: path.clone(), context });
    }
    let path = b.railpack.clone()?;
    check_repo_path(&path, &format!("{at}.railpack"), r).then_some(Build::Railpack { path })
}

fn check_health(h: Option<&raw::Health>, port: Option<u16>, at: &str, r: &mut Report) -> Option<Health> {
    let Some(h) = h else {
        r.add(at, "health_missing", "every service owes a health check: `http`, `tcp`, `postgres` or `redis`");
        return None;
    };
    let mut set = Vec::new();
    if let Some(path) = &h.http {
        set.push(Health::Http { path: path.clone() });
    }
    for (flag, kind) in [(h.tcp, Health::Tcp), (h.postgres, Health::Postgres), (h.redis, Health::Redis)] {
        match flag {
            Some(true) => set.push(kind),
            Some(false) => {
                r.add(at, "health_false", "a probe set to `false` checks nothing; name the one probe to run");
                return None;
            }
            None => {}
        }
    }
    if set.len() != 1 {
        r.add(at, "health_ambiguous", "set exactly one of `http`, `tcp`, `postgres` or `redis`");
        return None;
    }
    let health = set.remove(0);
    if let Health::Http { path } = &health
        && !path.starts_with('/')
    {
        r.add(format!("{at}.http"), "health_path_invalid", format!("`{path}` must start with '/'"));
        return None;
    }
    if port.is_none() {
        r.add(at, "health_needs_port", "a health probe needs the service's `port`");
        return None;
    }
    Some(health)
}

fn check_public(p: &raw::Public, s: &raw::Service, at: &str, r: &mut Report) -> Option<Public> {
    let before = r.0.len();
    if !is_hostname(&p.hostname) {
        r.add(format!("{at}.hostname"), "hostname_invalid", format!("`{}` is not a lowercase DNS name", p.hostname));
    }
    let path = p.path.clone().unwrap_or_else(|| "/".to_owned());
    if !path.starts_with('/') || path.contains("//") || path.split('/').any(|seg| seg == "..") {
        r.add(format!("{at}.path"), "public_path_invalid", format!("`{path}` must be an absolute URL path"));
    }
    if s.port.is_none() {
        r.add(at, "public_needs_port", "a public service needs a `port` to route to");
    }
    if !s.health.as_ref().is_some_and(|h| h.http.is_some()) {
        r.add(at, "public_needs_http_health", "a public service must answer an `http` health check");
    }
    (r.0.len() == before).then(|| Public { hostname: p.hostname.clone(), path })
}

fn check_volumes(vols: &[raw::Volume], health: &Option<Health>, at: &str, r: &mut Report) -> Vec<Volume> {
    let mut out = Vec::new();
    let mut mounts = BTreeSet::new();
    let mut names = BTreeSet::new();
    for v in vols {
        let vat = format!("{at}.volume[{}]", v.name);
        let before = r.0.len();
        if !is_name(&v.name) {
            r.add(&vat, "volume_name_invalid", format!("`{}` must be a-z, 0-9 and '-'", v.name));
        }
        if !names.insert(&v.name) {
            r.add(&vat, "volume_duplicate", format!("volume `{}` is mounted twice in one service", v.name));
        }
        if !is_abs_path(&v.mount) || v.mount == "/" {
            r.add(format!("{vat}.mount"), "mount_invalid", format!("`{}` must be an absolute path below '/'", v.mount));
        } else if let Some(bad) =
            FORBIDDEN_MOUNTS.iter().find(|f| v.mount == **f || v.mount.starts_with(&format!("{f}/")))
        {
            r.add(format!("{vat}.mount"), "mount_forbidden", format!("nothing may be mounted under {bad}"));
        }
        if !mounts.insert(&v.mount) {
            r.add(format!("{vat}.mount"), "mount_duplicate", format!("two volumes mount at {}", v.mount));
        }
        let backup = v.backup.as_deref().and_then(|b| check_backup(b, health, &format!("{vat}.backup"), r));
        if r.0.len() == before {
            out.push(Volume { name: v.name.clone(), mount: v.mount.clone(), backup });
        }
    }
    out
}

fn check_backup(text: &str, health: &Option<Health>, at: &str, r: &mut Report) -> Option<Backup> {
    let backup = match text {
        "files" => Backup::Files,
        "postgres" => Backup::Postgres,
        "redis" => Backup::Redis,
        _ => match text.strip_prefix("sqlite:") {
            Some(path) if check_repo_path(path, at, r) => Backup::Sqlite { path: path.to_owned() },
            Some(_) => return None,
            None => {
                r.add(
                    at,
                    "backup_unknown",
                    format!("`{text}` is not one of `files`, `postgres`, `redis` or `sqlite:<path>`"),
                );
                return None;
            }
        },
    };
    let needs = match backup {
        Backup::Postgres => Some(Health::Postgres),
        Backup::Redis => Some(Health::Redis),
        _ => None,
    };
    if let Some(need) = needs
        && health.as_ref().is_some_and(|h| *h != need)
    {
        r.add(
            at,
            "backup_engine_mismatch",
            format!("a `{text}` backup dumps through the server, so the service's health check must be `{text}` too"),
        );
        return None;
    }
    Some(backup)
}

fn check_resources(res: &raw::Resources, limits: &Limits, at: &str, r: &mut Report) -> Option<Resources> {
    let memory_bytes = match &res.memory {
        None => DEFAULT_MEMORY,
        Some(text) => match parse_bytes(text) {
            Some(n) if n < MIN_MEMORY => {
                r.add(format!("{at}.memory"), "memory_too_small", format!("at least {}", format_bytes(MIN_MEMORY)));
                return None;
            }
            Some(n) if n > limits.max_memory_bytes => {
                r.add(
                    format!("{at}.memory"),
                    "memory_over_limit",
                    format!("{text} is over the limit of {}", format_bytes(limits.max_memory_bytes)),
                );
                return None;
            }
            Some(n) => n,
            None => {
                r.add(format!("{at}.memory"), "memory_invalid", format!("`{text}` is not a size like `512MiB`"));
                return None;
            }
        },
    };
    let cpus = res.cpus.unwrap_or(DEFAULT_CPUS);
    if !(cpus > 0.0 && cpus <= limits.max_cpus) {
        r.add(format!("{at}.cpus"), "cpus_out_of_range", format!("must be above 0 and at most {}", limits.max_cpus));
        return None;
    }
    Some(Resources { memory_bytes, cpus })
}

/// Two services may not claim the same hostname and path.
fn check_routes(services: &[Service], r: &mut Report) {
    let mut routes: BTreeMap<(&str, &str), &str> = BTreeMap::new();
    for s in services {
        if let Some(p) = &s.public
            && let Some(other) = routes.insert((&p.hostname, &p.path), &s.name)
        {
            r.add(
                format!("service[{}].public", s.name),
                "route_duplicate",
                format!("{}{} is already routed to `{other}`", p.hostname, p.path),
            );
        }
    }
}

/// A volume shared between services must be backed up one way.
fn check_shared_volumes(services: &[Service], r: &mut Report) {
    let mut seen: BTreeMap<&str, (&str, &Option<Backup>)> = BTreeMap::new();
    for s in services {
        for v in &s.volumes {
            match seen.get(v.name.as_str()) {
                Some((other, backup)) if *backup != &v.backup => r.add(
                    format!("service[{}].volume[{}].backup", s.name, v.name),
                    "backup_conflict",
                    format!("`{other}` declares a different backup for the same volume"),
                ),
                Some(_) => {}
                None => {
                    seen.insert(&v.name, (&s.name, &v.backup));
                }
            }
        }
    }
}

fn check_cycles(services: &[Service], r: &mut Report) {
    let mut done: BTreeSet<&str> = BTreeSet::new();
    loop {
        let before = done.len();
        for s in services {
            if s.depends_on.iter().all(|d| done.contains(d.as_str())) {
                done.insert(&s.name);
            }
        }
        if done.len() == services.len() {
            return;
        }
        if done.len() == before {
            let stuck: Vec<&str> = services.iter().map(|s| s.name.as_str()).filter(|n| !done.contains(n)).collect();
            r.add(
                "service",
                "depends_on_cycle",
                format!("these services depend on each other in a loop: {}", stuck.join(", ")),
            );
            return;
        }
    }
}

const MAX_WORKERS: usize = 8;

fn check_workers(app: &str, raw: &[raw::Worker], r: &mut Report) -> Vec<Worker> {
    if raw.len() > MAX_WORKERS {
        r.add("worker", "workers_too_many", format!("{} workers is over the limit of {MAX_WORKERS}", raw.len()));
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for w in raw {
        let at = format!("worker[{}]", w.name);
        let before = r.0.len();
        if !seen.insert(w.name.as_str()) {
            r.add(&at, "worker_duplicate", format!("worker `{}` is defined twice", w.name));
        }
        if !is_name(&w.name) {
            r.add(&at, "name_invalid", format!("`{}` must be a-z, 0-9 and '-', starting with a letter", w.name));
        } else if format!("henry-{app}-{}", w.name).len() > SCRIPT_MAX {
            r.add(
                &at,
                "worker_name_too_long",
                format!("henry-{app}-{} is over Cloudflare's {SCRIPT_MAX} characters", w.name),
            );
        }
        check_repo_path(&w.dir, &format!("{at}.dir"), r);
        let mut bindings = BTreeSet::new();
        for b in &w.buckets {
            check_var(b, &format!("{at}.buckets"), r);
            if !bindings.insert(b.as_str()) {
                r.add(format!("{at}.buckets"), "worker_binding_duplicate", format!("`{b}` is bound twice"));
            }
        }
        for (name, from) in &w.secrets {
            check_var(name, &format!("{at}.secrets"), r);
            check_var(from, &format!("{at}.secrets.{name}"), r);
            if !bindings.insert(name.as_str()) {
                r.add(format!("{at}.secrets"), "worker_binding_duplicate", format!("`{name}` is bound twice"));
            }
        }
        if let Some(url) = &w.url {
            check_var(url, &format!("{at}.url"), r);
            if w.secrets.values().any(|v| v == url) {
                r.add(
                    format!("{at}.url"),
                    "worker_url_conflict",
                    format!("`{url}` is the Worker's URL, which Henry sets; it cannot also be one of its secrets"),
                );
            }
        }
        if r.0.len() == before {
            out.push(Worker {
                name: w.name.clone(),
                dir: w.dir.trim_end_matches('/').to_owned(),
                buckets: w.buckets.clone(),
                secrets: w.secrets.clone(),
                url: w.url.clone(),
            });
        }
    }
    out
}

fn check_var(name: &str, at: &str, r: &mut Report) {
    let mut chars = name.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        r.add(at, "var_name_invalid", format!("`{name}` is not a valid environment variable name"));
    } else if name.starts_with(RESERVED_PREFIX) {
        r.add(at, "var_name_reserved", format!("names starting with {RESERVED_PREFIX} are set by Henry"));
    }
}

/// A path inside the repo: relative, and never climbing out of it.
fn check_repo_path(path: &str, at: &str, r: &mut Report) -> bool {
    let ok =
        !path.is_empty() && !path.starts_with('/') && !path.contains('\\') && !path.split('/').any(|seg| seg == "..");
    if !ok {
        r.add(at, "path_outside_repo", format!("`{path}` must be a relative path that stays inside the repo"));
    }
    ok
}

/// Whether a name may name an app, a service or a worker.
pub fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= NAME_MAX
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && !s.ends_with('-')
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_branch(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(['-', '/'])
        && !s.ends_with(['/', '.'])
        && !s.contains("..")
        && !s.contains("//")
        && !s.ends_with(".lock")
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
}

fn is_hostname(s: &str) -> bool {
    let labels: Vec<&str> = s.split('.').collect();
    s.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

fn is_abs_path(s: &str) -> bool {
    s.starts_with('/')
        && (s == "/" || !s.ends_with('/'))
        && !s.contains("//")
        && !s.split('/').any(|seg| seg == "." || seg == "..")
}
