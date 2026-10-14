//! The plan Sound can stand behind without AI, and the gaps it guessed at.
//!
//! The most deterministic source wins: the repo's own `lease.toml`, then its
//! compose file, then a Dockerfile, then a language manifest Railpack can
//! build. Backing services the app seems to need (Postgres, Redis) are added
//! with generated credentials, so an app gets its own private database.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::{ComposeService, Need, Site, Survey, image_kind, is_worker_module, looks_secret};

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
        let wanted: BTreeSet<&str> = lease
            .services
            .iter()
            .flat_map(|s| s.secrets.iter())
            .chain(lease.workers.iter().flat_map(|w| w.secrets.values()))
            .map(String::as_str)
            .collect();
        let generated: BTreeSet<&str> = self.generate.iter().map(|g| g.name.as_str()).collect();
        for g in &self.generate {
            if !wanted.contains(g.name.as_str()) {
                problems.push(format!("`{}` is generated but no service or worker lists it in `secrets`", g.name));
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
        let mut files = Vec::new();
        let workers = self.plan_workers(ask.app, &mut gaps);
        // What Henry itself gives the app: Workers' shared secrets and URLs.
        let from_workers: BTreeSet<String> =
            workers.iter().flat_map(|w| w.secrets.values().chain(&w.url)).cloned().collect();
        let mut services = if let Some(c) = self.compose.as_ref().filter(|c| !c.services.is_empty()) {
            from_compose(&c.services, &mut gaps)
        } else {
            vec![
                self.dockerfile_service(&mut gaps)
                    .or_else(|| {
                        self.site.as_ref().map(|site| self.site_service(site, &from_workers, &mut files, &mut gaps))
                    })
                    .or_else(|| self.stack_service(&mut gaps))?,
            ]
        };
        if services.is_empty() {
            return None;
        }

        let mut generate = Vec::new();
        let main = main_service(&services);
        self.add_backing(&mut services, main.as_deref(), &mut generate, &mut gaps);
        for w in &workers {
            for app_secret in w.secrets.values() {
                push_generate(&mut generate, app_secret, None);
            }
        }
        // The app's code reads what its Workers share: it gets them as secrets.
        if let Some(main) = &main {
            let svc = services.iter_mut().find(|s| &s.name == main).expect("the main service exists");
            svc.secrets.extend(from_workers.iter().filter(|v| self.env.contains_key(*v)).cloned());
        }
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

        let lease = write_lease(ask, &services, &workers);
        let notes = format!("drafted by Sound from the repo; {} guess(es)", gaps.len());
        Some(Skeleton { plan: Plan { lease, files, generate, notes }, source: SkeletonSource::Sound, gaps })
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

    /// A static site, served by Caddy from an image Henry writes: only the
    /// published files, as Vercel would serve them. A build command runs as
    /// the container starts, so it sees the app's secrets as a Vercel build
    /// sees its env, and no image holds them.
    fn site_service(
        &self,
        site: &Site,
        from_workers: &BTreeSet<String>,
        files: &mut Vec<(String, String)>,
        gaps: &mut Vec<String>,
    ) -> Svc {
        let dir = format!("{OVERLAY_DIR}site");
        let mut svc = Svc {
            name: "web".into(),
            build: format!("{{ dockerfile = {}, context = \".\" }}", q(&format!("{dir}/Dockerfile"))),
            port: Some(8080),
            health: Some("http = \"/\"".into()),
            ..Svc::default()
        };
        let mut docker = format!("# Written by Henry for a static site (from {}); see site.sh.\n", site.from);
        docker.push_str("FROM alpine:3.22 AS source\nCOPY . /src\n");
        match &site.build {
            None => {
                let _ = write!(
                    docker,
                    "RUN sh /src/{dir}/site.sh prune && SITE_OUT={out} sh /src/{dir}/site.sh publish /site\n\n\
                     FROM caddy:2-alpine\nCOPY --from=source /site /srv\nEXPOSE 8080\n\
                     CMD [\"caddy\", \"file-server\", \"--root\", \"/srv\", \"--listen\", \":8080\"]\n",
                    out = site.output
                );
            }
            Some(build) => {
                let _ = write!(
                    docker,
                    "RUN sh /src/{dir}/site.sh prune\n\nFROM node:22-alpine\n\
                     COPY --from=caddy:2-alpine /usr/bin/caddy /usr/bin/caddy\n\
                     COPY --from=source /src /src\nWORKDIR /src\n"
                );
                if let Some(install) = self.site_install(site) {
                    files.push((
                        format!("{dir}/install.sh"),
                        script(&format!("installs the site's build tools ({})", site.from), &install),
                    ));
                    let _ = writeln!(docker, "RUN sh {dir}/install.sh");
                }
                let _ = write!(
                    docker,
                    "ENV SITE_OUT={}\nEXPOSE 8080\nCMD [\"sh\", \"/src/{dir}/site.sh\", \"serve\"]\n",
                    site.output
                );
                files
                    .push((format!("{dir}/build.sh"), script(&format!("the site's build, from {}", site.from), build)));
                let tool = build.split_whitespace().next().unwrap_or_default();
                if !["node", "npm", "npx", "pnpm", "yarn", "corepack", "sh"].contains(&tool) {
                    gaps.push(format!("the build `{build}` runs in node:22-alpine, which may not have `{tool}`"));
                }
                let all = self.build_reads(build);
                let reads: Vec<String> = all.iter().filter(|r| !from_workers.contains(*r)).cloned().collect();
                svc.secrets.extend(all);
                if !reads.is_empty() {
                    gaps.push(format!(
                        "the build reads {}: each is a secret the app waits on, as a Vercel build reads its env",
                        reads.join(", ")
                    ));
                }
            }
        }
        files.push((format!("{dir}/Dockerfile"), docker));
        files.push((format!("{dir}/site.sh"), include_str!("site.sh").to_owned()));
        files.sort();
        if !site.unsupported.is_empty() {
            gaps.push(format!("vercel.json's {} are not carried over", site.unsupported.join(", ")));
        }
        if site.reincludes {
            gaps.push(".vercelignore re-includes files with `!`; Henry leaves them out too".into());
        }
        svc
    }

    /// How the site's build gets its tools: what vercel.json says, else the
    /// lockfile's own installer. None without a root package.json.
    fn site_install(&self, site: &Site) -> Option<String> {
        if let Some(install) = &site.install {
            return Some(install.clone());
        }
        let has = |f: &str| self.files.iter().any(|x| x == f);
        if !has("package.json") {
            return None;
        }
        Some(
            if has("package-lock.json") {
                "npm ci"
            } else if has("pnpm-lock.yaml") {
                "corepack enable && pnpm install --frozen-lockfile"
            } else if has("yarn.lock") {
                "corepack enable && yarn install --frozen-lockfile"
            } else {
                "npm install"
            }
            .into(),
        )
    }

    /// The env vars a build command reads: those read in a file the command
    /// names, or, for a command naming none (`npm run build`), every one the
    /// code reads.
    fn build_reads(&self, build: &str) -> Vec<String> {
        let named: Vec<&str> = build
            .split_whitespace()
            .map(|w| w.trim_matches(['"', '\'']).trim_start_matches("./"))
            .filter(|w| self.files.iter().any(|f| f == w))
            .collect();
        self.env
            .iter()
            .filter(|(_, v)| !v.read_in.is_empty())
            .filter(|(_, v)| named.is_empty() || v.read_in.iter().any(|f| named.contains(&f.as_str())))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// The Workers Henry can deploy as they are, linked to the app: a secret
    /// the Worker reads that the app also reads (`CAIRN_SYNC_TOKEN` for
    /// `SYNC_TOKEN`) is one value Henry makes for both, and an app variable
    /// named for the Worker's URL (`CAIRN_SYNC_URL` for `cairn-sync`) is set
    /// to it. Each link is a guess, and said so.
    fn plan_workers(&self, app: &str, gaps: &mut Vec<String>) -> Vec<WorkerPlan> {
        let mut out: Vec<WorkerPlan> = Vec::new();
        let mut used = BTreeSet::new();
        for w in &self.workers {
            let c = &w.config;
            let why_not = if !c.unsupported.is_empty() {
                Some(format!("it uses {}, which Henry does not set up", c.unsupported.join(", ")))
            } else if !w.packages.is_empty() {
                Some(format!(
                    "it imports {}; Henry uploads plain modules, so bundle it into one file first",
                    w.packages.join(", ")
                ))
            } else if !is_worker_module(&c.main) || !self.files.iter().any(|f| *f == join(&c.dir, &c.main)) {
                Some(format!("its main `{}` is not a JavaScript module in the repo", c.main))
            } else {
                None
            };
            if let Some(why) = why_not {
                gaps.push(format!("the Worker in `{}` is not deployed: {why}", c.dir));
                continue;
            }
            let short = c.name.strip_prefix(&format!("{app}-")).unwrap_or(&c.name);
            let dir_name = c.dir.rsplit('/').next().unwrap_or(&c.dir);
            let name = [lease_name(short), lease_name(dir_name), "worker".to_owned()]
                .into_iter()
                .filter(|n| lease::is_name(n))
                .map(|n| free_worker_name(&out, &n))
                .find(|n| format!("henry-{app}-{n}").len() <= lease::SCRIPT_MAX);
            let Some(name) = name else {
                gaps.push(format!("the Worker in `{}` is not deployed: no name fits Cloudflare's limit", c.dir));
                continue;
            };
            let mut plan = WorkerPlan {
                name: name.clone(),
                dir: c.dir.clone(),
                buckets: c.buckets.clone(),
                secrets: BTreeMap::new(),
                url: None,
            };
            for s in &w.secrets {
                let link = self.env.keys().find(|v| (*v == s || v.ends_with(&format!("_{s}"))) && !used.contains(*v));
                match link {
                    Some(v) => {
                        gaps.push(format!("`{v}` is made by Henry and given to Worker `{name}` as {s}"));
                        plan.secrets.insert(s.clone(), v.clone());
                    }
                    None => {
                        gaps.push(format!("Worker `{name}`'s {s} is made by Henry; nothing else in the repo reads it"));
                        plan.secrets.insert(s.clone(), s.clone());
                    }
                }
            }
            used.extend(plan.secrets.values().cloned());
            let stems: Vec<String> = [c.name.as_str(), short, dir_name]
                .iter()
                .map(|n| n.to_ascii_uppercase().replace(['-', '.'], "_"))
                .filter(|n| !n.is_empty())
                .collect();
            plan.url = self
                .env
                .keys()
                .filter(|v| !used.contains(*v))
                .find(|v| {
                    v.strip_suffix("_URL")
                        .is_some_and(|stem| stems.iter().any(|t| stem == t || stem.ends_with(&format!("_{t}"))))
                })
                .cloned();
            if let Some(url) = &plan.url {
                gaps.push(format!("Henry sets `{url}` to Worker `{name}`'s URL once it is deployed"));
                used.insert(url.clone());
            }
            if !c.buckets.is_empty() {
                gaps.push(format!(
                    "Worker `{name}`'s {} stores in Henry's R2 bucket, under apps/{app}/{name}/",
                    c.buckets.join(", ")
                ));
            }
            out.push(plan);
        }
        out
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

/// A shell script Henry writes into the checkout: what it is, then `body`.
fn script(what: &str, body: &str) -> String {
    format!("#!/bin/sh\n# Written by Henry: {what}.\nset -e\n{}\n", body.trim_end())
}

/// A Worker as it will be written into the lease.
#[derive(Debug)]
struct WorkerPlan {
    name: String,
    dir: String,
    buckets: Vec<String>,
    secrets: BTreeMap<String, String>,
    url: Option<String>,
}

fn free_worker_name(taken: &[WorkerPlan], want: &str) -> String {
    (0..)
        .map(|i| if i == 0 { want.to_owned() } else { format!("{want}-{i}") })
        .find(|n| !taken.iter().any(|w| &w.name == n))
        .expect("an unbounded range finds a free name")
}

/// `dir/file`, where `.` is the root.
fn join(dir: &str, file: &str) -> String {
    if dir == "." { file.to_owned() } else { format!("{dir}/{file}") }
}

/// TOML string quoting.
fn q(s: &str) -> String {
    toml::Value::String(s.to_owned()).to_string()
}

fn write_lease(ask: &Ask<'_>, services: &[Svc], workers: &[WorkerPlan]) -> String {
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
    for w in workers {
        let _ = writeln!(out, "[[worker]]\nname = {}\ndir = {}", q(&w.name), q(&w.dir));
        if !w.buckets.is_empty() {
            let items: Vec<String> = w.buckets.iter().map(|b| q(b)).collect();
            let _ = writeln!(out, "buckets = [{}]", items.join(", "));
        }
        if !w.secrets.is_empty() {
            let items: Vec<String> = w.secrets.iter().map(|(k, v)| format!("{k} = {}", q(v))).collect();
            let _ = writeln!(out, "secrets = {{ {} }}", items.join(", "));
        }
        if let Some(url) = &w.url {
            let _ = writeln!(out, "url = {}", q(url));
        }
        out.push('\n');
    }
    out
}
