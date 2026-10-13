//! Sound: the repo scan that uses no AI.
//!
//! Taking soundings is measuring the depth before you sail in. Sound reads a
//! checkout and reports what is there: a `lease.toml`, Dockerfiles, a compose
//! file, the language manifests, the env var names the code reads, and hints
//! from CI and Procfiles. From that it drafts the plan it can stand behind
//! ([`Survey::skeleton`]) and says where it guessed, so Ghost only fills gaps.
//!
//! Sound runs nothing from the repo, and it keeps env var *names*: an example
//! value is kept only when the name does not look like a secret.

mod skeleton;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

pub use skeleton::{Ask, Generate, OVERLAY_DIR, Plan, Skeleton, SkeletonSource, placeholders};

/// Directories never worth reading: dependencies, build output, VCS.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".henry",
    "node_modules",
    "vendor",
    "target",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".svelte-kit",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    ".mypy_cache",
    "coverage",
    ".cache",
    ".turbo",
    "deps",
    "_build",
];
const MAX_FILES: usize = 5000;
const MAX_DEPTH: usize = 8;
const MAX_SOURCE_BYTES: u64 = 256 * 1024;
const MAX_HINTS: usize = 40;

/// Env vars every platform sets or every app reads in passing. Not config.
const AMBIENT: &[&str] = &[
    "PORT",
    "HOST",
    "HOSTNAME",
    "NODE_ENV",
    "PATH",
    "HOME",
    "PWD",
    "USER",
    "SHELL",
    "TZ",
    "LANG",
    "TERM",
    "CI",
    "TMPDIR",
    "PYTHONPATH",
    "RUST_LOG",
    "RUST_BACKTRACE",
    "GOPATH",
    "DEBUG",
];

/// What Sound found in one checkout.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Survey {
    /// The repo's own `lease.toml`, which wins over anything drafted.
    pub lease: Option<String>,
    pub dockerfiles: Vec<Dockerfile>,
    pub compose: Option<Compose>,
    pub stacks: Vec<Stack>,
    pub env: BTreeMap<String, EnvVar>,
    pub needs: BTreeSet<Need>,
    /// Commands from CI and Procfiles: how the authors build, test and start.
    pub hints: Vec<String>,
    /// Every file read, relative to the root, in walk order.
    pub files: Vec<String>,
    /// The walk stopped at its limit; `files` is not the whole repo.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Dockerfile {
    pub path: String,
    pub expose: Vec<u16>,
    pub from: Vec<String>,
    pub has_cmd: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Compose {
    pub path: String,
    pub services: Vec<ComposeService>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ComposeService {
    pub name: String,
    pub image: Option<String>,
    /// `(context, dockerfile)`, both relative to the repo root.
    pub build: Option<(String, String)>,
    /// Container-side ports, from `ports` and `expose`.
    pub ports: Vec<u16>,
    /// Literal values; `None` where the file interpolates or leaves it blank.
    pub env: BTreeMap<String, Option<String>>,
    /// Variables whose whole value is one interpolation, `K: ${S:-d}`: where
    /// compose would take each from (`S`, from `.env`), and the default it
    /// would fall back to. Defaults are committed text, never secrets.
    pub interpolated: BTreeMap<String, (String, Option<String>)>,
    /// Whether it loads an `env_file`, which hands it every variable there.
    pub env_file: bool,
    /// The path its healthcheck asks for over HTTP on its own port, if any:
    /// `curl -f http://localhost:8000/health` gives `(8000, "/health")`.
    pub health: Option<(u16, String)>,
    /// Named volumes: `(name, mount)`. Bind mounts are not kept.
    pub volumes: Vec<(String, String)>,
    pub bind_mounts: bool,
    pub depends_on: Vec<String>,
    pub command: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StackKind {
    Node,
    Python,
    Go,
    Rust,
    Ruby,
    Php,
    Elixir,
    Java,
    Deno,
}

impl StackKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StackKind::Node => "node",
            StackKind::Python => "python",
            StackKind::Go => "go",
            StackKind::Rust => "rust",
            StackKind::Ruby => "ruby",
            StackKind::Php => "php",
            StackKind::Elixir => "elixir",
            StackKind::Java => "java",
            StackKind::Deno => "deno",
        }
    }
}

/// A language manifest, which Railpack can build from.
#[derive(Debug, Clone, PartialEq)]
pub struct Stack {
    pub kind: StackKind,
    /// Its directory, relative to the root; `.` for the root.
    pub dir: String,
    pub manifest: String,
    /// The start command the manifest declares, if any.
    pub start: Option<String>,
    pub framework: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EnvVar {
    pub secret: bool,
    /// Listed in `.env.example` or similar: the authors say it is config.
    pub documented: bool,
    /// The example value, only for names that do not look secret.
    pub example: Option<String>,
    /// Up to three files that read it.
    pub read_in: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Need {
    Postgres,
    Redis,
}

/// Whether an env var name looks like it holds a secret. Connection URLs
/// count: they carry passwords.
pub fn looks_secret(name: &str) -> bool {
    const MARKS: &[&str] =
        &["SECRET", "TOKEN", "PASSWORD", "PASSWD", "PASS", "KEY", "PRIVATE", "CREDENTIAL", "DSN", "AUTH", "SALT"];
    let n = name.to_ascii_uppercase();
    MARKS.iter().any(|m| n.contains(m))
        || (n.ends_with("_URL")
            && ["DATABASE", "POSTGRES", "PG", "REDIS", "MONGO", "AMQP", "MYSQL"].iter().any(|d| n.contains(d)))
}

/// Whether a file may be shown to Ghost. Real env files, keys and
/// credentials never are, whatever the repo claims they hold.
pub fn readable(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    if name == ".env" || (name.starts_with(".env.") && !is_env_example(&name)) {
        return false;
    }
    let banned_ext = [".pem", ".key", ".p12", ".pfx", ".keystore", ".jks", ".kdbx"];
    let banned_names = [".npmrc", ".pypirc", ".netrc", "id_rsa", "id_ed25519", "id_ecdsa", "credentials.json"];
    !(banned_ext.iter().any(|e| name.ends_with(e)) || banned_names.contains(&name.as_str()))
}

fn is_env_example(name: &str) -> bool {
    [".env.example", ".env.sample", ".env.template", ".env.dist", ".env.defaults", "env.example", ".example.env"]
        .contains(&name)
}

static ENV_REF: LazyLock<Regex> = LazyLock::new(|| {
    let name = r"([A-Z][A-Z0-9_]{1,63})";
    Regex::new(
        &[
            format!(r"process\.env\.{name}\b"),
            format!(r#"process\.env\[\s*['"]{name}['"]\s*\]"#),
            format!(r"import\.meta\.env\.{name}\b"),
            format!(r#"Deno\.env\.get\(\s*['"]{name}['"]"#),
            format!(r#"os\.environ(?:\.get)?\s*[\[(]\s*['"]{name}['"]"#),
            format!(r#"(?:os\.)?getenv\(\s*['"]{name}['"]"#),
            format!(r#"env::var(?:_os)?\(\s*"{name}""#),
            format!(r#"os\.(?:Getenv|LookupEnv)\(\s*"{name}""#),
            format!(r#"ENV(?:\.fetch\(\s*|\[\s*)['"]{name}['"]"#),
            format!(r#"System\.get_?[eE]nv\(\s*"{name}""#),
            format!(r#"env\(\s*['"]{name}['"]"#),
        ]
        .join("|"),
    )
    .expect("the env pattern compiles")
});

impl Survey {
    /// Scan a checkout. Unreadable files are skipped, not fatal.
    pub fn scan(root: &Path) -> std::io::Result<Survey> {
        let mut s = Survey::default();
        let mut stack = vec![(root.to_path_buf(), 0usize)];
        let mut seen = 0usize;
        while let Some((dir, depth)) = stack.pop() {
            let mut entries: Vec<_> = std::fs::read_dir(&dir)?.filter_map(Result::ok).collect();
            entries.sort_by_key(|e| e.file_name());
            let mut subdirs = Vec::new();
            for entry in entries {
                let Ok(kind) = entry.file_type() else { continue };
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if kind.is_dir() {
                    if depth < MAX_DEPTH && !SKIP_DIRS.contains(&name.as_str()) {
                        subdirs.push((path, depth + 1));
                    }
                    continue;
                }
                if !kind.is_file() {
                    continue;
                }
                if seen >= MAX_FILES {
                    s.truncated = true;
                    break;
                }
                seen += 1;
                let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
                s.read(&path, &rel, &name);
                s.files.push(rel);
            }
            // Depth-first, in name order.
            stack.extend(subdirs.into_iter().rev());
        }
        s.settle();
        Ok(s)
    }

    fn read(&mut self, path: &Path, rel: &str, name: &str) {
        let lower = name.to_ascii_lowercase();
        let dir = rel.rsplit_once('/').map_or(".", |(d, _)| d).to_owned();
        let text = || std::fs::read_to_string(path).ok();
        if rel == "lease.toml" {
            self.lease = text();
        } else if lower == "dockerfile"
            || lower == "containerfile"
            || lower.starts_with("dockerfile.")
            || lower.ends_with(".dockerfile")
        {
            if let Some(t) = text() {
                self.dockerfiles.push(dockerfile(rel, &t));
            }
        } else if ["docker-compose.yml", "docker-compose.yaml", "compose.yml", "compose.yaml"].contains(&lower.as_str())
            && self.compose.is_none()
            && dir == "."
        {
            if let Some(c) = text().and_then(|t| compose(rel, &t)) {
                self.compose = Some(c);
            }
        } else if is_env_example(&lower) {
            if let Some(t) = text() {
                self.env_example(&t);
            }
        } else if lower == "procfile" {
            if let Some(t) = text() {
                self.hints
                    .extend(t.lines().filter(|l| !l.trim().is_empty()).map(|l| format!("Procfile: {}", l.trim())));
            }
        } else if rel.starts_with(".github/workflows/")
            && (lower.ends_with(".yml") || lower.ends_with(".yaml"))
            && let Some(t) = text()
        {
            self.ci_hints(rel, &t);
        }
        if let Some(stack) = manifest(path, &lower, &dir) {
            self.needs.extend(needs_from_manifest(path));
            self.stacks.push(stack);
        }
        if is_source(&lower)
            && std::fs::metadata(path).is_ok_and(|m| m.len() <= MAX_SOURCE_BYTES)
            && let Some(t) = text()
        {
            for c in ENV_REF.captures_iter(&t) {
                let Some(m) = c.iter().skip(1).flatten().next() else { continue };
                let var = self.env.entry(m.as_str().to_owned()).or_default();
                if var.read_in.len() < 3 && !var.read_in.iter().any(|f| f == rel) {
                    var.read_in.push(rel.to_owned());
                }
            }
        }
    }

    fn env_example(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim().trim_start_matches("export ").trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((name, value)) = line.split_once('=') else { continue };
            let name = name.trim();
            if !valid_var(name) {
                continue;
            }
            let var = self.env.entry(name.to_owned()).or_default();
            var.documented = true;
            let value = value.split(" #").next().unwrap_or("").trim().trim_matches(['"', '\'']);
            if !looks_secret(name) && !value.is_empty() && value.len() <= 120 {
                var.example = Some(value.to_owned());
            }
        }
    }

    fn ci_hints(&mut self, rel: &str, text: &str) {
        let Ok(doc) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text) else { return };
        let Some(jobs) = doc.get("jobs").and_then(|j| j.as_mapping()) else { return };
        for job in jobs.values() {
            let Some(steps) = job.get("steps").and_then(|s| s.as_sequence()) else { continue };
            for step in steps {
                if let Some(run) = step.get("run").and_then(|r| r.as_str()) {
                    for line in run.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
                        if self.hints.len() < MAX_HINTS {
                            self.hints.push(format!("{rel}: {line}"));
                        }
                    }
                }
            }
        }
    }

    /// Mark secrets, drop ambient names, and infer backing services from
    /// the env names the code reads.
    fn settle(&mut self) {
        self.env.retain(|name, _| !AMBIENT.contains(&name.as_str()) && !name.starts_with("HENRY_"));
        for (name, var) in &mut self.env {
            var.secret = looks_secret(name);
            if name == "DATABASE_URL" || name.starts_with("POSTGRES_") || name.starts_with("PG") {
                self.needs.insert(Need::Postgres);
            }
            if name.starts_with("REDIS_") {
                self.needs.insert(Need::Redis);
            }
        }
        if let Some(c) = &self.compose {
            for svc in &c.services {
                match svc.image.as_deref().map(image_kind) {
                    Some(Some(Need::Postgres)) => self.needs.insert(Need::Postgres),
                    Some(Some(Need::Redis)) => self.needs.insert(Need::Redis),
                    _ => false,
                };
            }
        }
        self.hints.truncate(MAX_HINTS);
    }

    /// A plain-text summary for Ghost: what is here, without file contents.
    pub fn brief(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "## Files ({}{})", self.files.len(), if self.truncated { ", truncated" } else { "" });
        for f in self.files.iter().take(300) {
            let _ = writeln!(out, "{f}");
        }
        if self.files.len() > 300 {
            let _ = writeln!(out, "… and {} more", self.files.len() - 300);
        }
        if self.lease.is_some() {
            let _ = writeln!(out, "\n## lease.toml\nThe repo has its own lease.toml.");
        }
        if !self.dockerfiles.is_empty() {
            let _ = writeln!(out, "\n## Dockerfiles");
            for d in &self.dockerfiles {
                let _ =
                    writeln!(out, "{}: FROM {}; EXPOSE {:?}; CMD {}", d.path, d.from.join(", "), d.expose, d.has_cmd);
            }
        }
        if let Some(c) = &self.compose {
            let _ = writeln!(out, "\n## {}", c.path);
            for s in &c.services {
                let _ = writeln!(
                    out,
                    "{}: image {:?}, build {:?}, ports {:?}, env {:?}, volumes {:?}, depends_on {:?}",
                    s.name,
                    s.image,
                    s.build,
                    s.ports,
                    s.env.keys().collect::<Vec<_>>(),
                    s.volumes,
                    s.depends_on
                );
            }
        }
        if !self.stacks.is_empty() {
            let _ = writeln!(out, "\n## Manifests");
            for s in &self.stacks {
                let _ = writeln!(
                    out,
                    "{} ({}) in {}: start {:?}, framework {:?}",
                    s.manifest,
                    s.kind.as_str(),
                    s.dir,
                    s.start,
                    s.framework
                );
            }
        }
        if !self.env.is_empty() {
            let _ = writeln!(out, "\n## Env vars (names only)");
            for (name, v) in &self.env {
                let mut what = Vec::new();
                if v.secret {
                    what.push("secret".to_owned());
                }
                if v.documented {
                    what.push("documented".to_owned());
                }
                if let Some(e) = &v.example {
                    what.push(format!("example {e:?}"));
                }
                if !v.read_in.is_empty() {
                    what.push(format!("read in {}", v.read_in.join(", ")));
                }
                let _ = writeln!(out, "{name}: {}", what.join("; "));
            }
        }
        if !self.needs.is_empty() {
            let _ = writeln!(out, "\n## Backing services it seems to need\n{:?}", self.needs);
        }
        if !self.hints.is_empty() {
            let _ = writeln!(out, "\n## Commands from CI and Procfiles");
            for h in &self.hints {
                let _ = writeln!(out, "{h}");
            }
        }
        out
    }
}

fn valid_var(name: &str) -> bool {
    !name.is_empty()
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_source(lower: &str) -> bool {
    const EXT: &[&str] = &[
        ".js", ".mjs", ".cjs", ".jsx", ".ts", ".mts", ".tsx", ".svelte", ".vue", ".py", ".go", ".rs", ".rb", ".php",
        ".ex", ".exs", ".java", ".kt", ".cs",
    ];
    EXT.iter().any(|e| lower.ends_with(e)) && !lower.ends_with(".min.js") && !lower.ends_with(".d.ts")
}

fn dockerfile(rel: &str, text: &str) -> Dockerfile {
    let mut d = Dockerfile { path: rel.to_owned(), expose: vec![], from: vec![], has_cmd: false };
    for line in text.lines() {
        let line = line.trim();
        let Some((word, rest)) = line.split_once(char::is_whitespace) else { continue };
        match word.to_ascii_uppercase().as_str() {
            "EXPOSE" => {
                d.expose.extend(rest.split_whitespace().filter_map(|p| p.split('/').next()?.parse::<u16>().ok()))
            }
            "FROM" => {
                let image = rest.split_whitespace().find(|w| !w.starts_with("--")).unwrap_or_default();
                d.from.push(image.to_owned());
            }
            "CMD" | "ENTRYPOINT" => d.has_cmd = true,
            _ => {}
        }
    }
    d
}

/// What a well-known image is, so a compose service can get the right probe.
fn image_kind(image: &str) -> Option<Need> {
    let base = image.rsplit('/').next().unwrap_or(image);
    let base = base.split([':', '@']).next().unwrap_or(base);
    match base {
        "postgres" | "postgis" | "pgvector" => Some(Need::Postgres),
        "redis" | "valkey" | "redis-stack-server" => Some(Need::Redis),
        _ => None,
    }
}

/// `${S}`, `${S:-d}`, `${S-d}` or `$S` as a whole value: `(S, d)`.
fn interpolation(v: &str) -> Option<(String, Option<String>)> {
    let name_ok = |n: &str| {
        let mut c = n.chars();
        c.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && c.all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    if let Some(inner) = v.strip_prefix("${").and_then(|r| r.strip_suffix('}')) {
        if inner.contains(['$', '{', '}']) {
            return None;
        }
        let (name, default) = match inner.split_once(":-").or_else(|| inner.split_once('-')) {
            Some((n, d)) => (n, Some(d.to_owned())),
            None => (inner, None),
        };
        return name_ok(name).then(|| (name.to_owned(), default));
    }
    v.strip_prefix('$').filter(|n| name_ok(n)).map(|n| (n.to_owned(), None))
}

/// `http://localhost:8000/health` as `(8000, "/health")`. Only the
/// container's own loopback counts: that is what the service answers itself.
fn local_http(word: &str) -> Option<(u16, String)> {
    let rest = word.trim_matches(['"', '\'']).strip_prefix("http://")?;
    let (host, path) = rest.split_once('/').map_or((rest, ""), |(h, p)| (h, p));
    let (name, port) = host.rsplit_once(':').map_or((host, "80"), |(n, p)| (n, p));
    if !matches!(name, "localhost" | "127.0.0.1" | "0.0.0.0") {
        return None;
    }
    Some((port.parse().ok()?, format!("/{path}")))
}

fn compose(rel: &str, text: &str) -> Option<Compose> {
    use serde_yaml_ng::Value;
    let doc: Value = serde_yaml_ng::from_str(text).ok()?;
    let services = doc.get("services")?.as_mapping()?;
    let base = rel.rsplit_once('/').map_or("", |(d, _)| d);
    let join = |p: &str| -> String {
        let p = p.trim_start_matches("./");
        let joined = if base.is_empty() { p.to_owned() } else { format!("{base}/{p}") };
        if joined.is_empty() { ".".to_owned() } else { joined.trim_end_matches('/').to_owned() }
    };
    let mut out = Vec::new();
    for (name, svc) in services {
        let Some(name) = name.as_str() else { continue };
        let mut s = ComposeService { name: name.to_owned(), ..Default::default() };
        s.image = svc.get("image").and_then(Value::as_str).map(str::to_owned);
        s.build = match svc.get("build") {
            Some(Value::String(ctx)) => Some((join(ctx), join(&format!("{}/Dockerfile", ctx.trim_end_matches('/'))))),
            Some(b @ Value::Mapping(_)) => {
                let ctx = b.get("context").and_then(Value::as_str).unwrap_or(".");
                let file = b.get("dockerfile").and_then(Value::as_str).unwrap_or("Dockerfile");
                Some((join(ctx), join(&format!("{}/{file}", ctx.trim_end_matches('/')))))
            }
            _ => None,
        };
        for p in svc.get("ports").and_then(Value::as_sequence).into_iter().flatten() {
            let port = match p {
                Value::Number(n) => n.as_u64().map(|n| n as u16),
                Value::String(t) => t.rsplit(':').next().and_then(|c| c.split(['/', '-']).next()?.parse().ok()),
                Value::Mapping(_) => p.get("target").and_then(Value::as_u64).map(|n| n as u16),
                _ => None,
            };
            s.ports.extend(port);
        }
        for p in svc.get("expose").and_then(Value::as_sequence).into_iter().flatten() {
            let port = match p {
                Value::Number(n) => n.as_u64().map(|n| n as u16),
                Value::String(t) => t.split('/').next().and_then(|t| t.parse().ok()),
                _ => None,
            };
            s.ports.extend(port);
        }
        match svc.get("environment") {
            Some(Value::Mapping(m)) => {
                for (k, v) in m {
                    let Some(k) = k.as_str() else { continue };
                    let v = match v {
                        Value::String(t) => Some(t.clone()),
                        Value::Number(n) => Some(n.to_string()),
                        Value::Bool(b) => Some(b.to_string()),
                        _ => None,
                    };
                    if let Some(i) = v.as_deref().and_then(interpolation) {
                        s.interpolated.insert(k.to_owned(), i);
                    }
                    s.env.insert(k.to_owned(), v.filter(|v| !v.contains('$')));
                }
            }
            Some(Value::Sequence(items)) => {
                for item in items.iter().filter_map(Value::as_str) {
                    let (k, v) = item.split_once('=').map_or((item, None), |(k, v)| (k, Some(v.to_owned())));
                    if let Some(i) = v.as_deref().and_then(interpolation) {
                        s.interpolated.insert(k.to_owned(), i);
                    }
                    s.env.insert(k.to_owned(), v.filter(|v| !v.contains('$')));
                }
            }
            _ => {}
        }
        s.env_file = svc.get("env_file").is_some();
        s.health = svc.get("healthcheck").and_then(|h| h.get("test")).and_then(|t| {
            let words: Vec<String> = match t {
                Value::Sequence(items) => items.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
                Value::String(t) => t.split_whitespace().map(str::to_owned).collect(),
                _ => vec![],
            };
            words.iter().find_map(|w| local_http(w))
        });
        for v in svc.get("volumes").and_then(Value::as_sequence).into_iter().flatten() {
            let (source, target) = match v {
                Value::String(t) => {
                    let mut parts = t.splitn(3, ':');
                    (parts.next().unwrap_or_default().to_owned(), parts.next().unwrap_or_default().to_owned())
                }
                Value::Mapping(_) => (
                    v.get("source").and_then(Value::as_str).unwrap_or_default().to_owned(),
                    v.get("target").and_then(Value::as_str).unwrap_or_default().to_owned(),
                ),
                _ => continue,
            };
            if source.starts_with(['.', '/', '~']) || target.is_empty() {
                s.bind_mounts = true;
            } else {
                s.volumes.push((source, target));
            }
        }
        s.depends_on = match svc.get("depends_on") {
            Some(Value::Sequence(items)) => items.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
            Some(Value::Mapping(m)) => m.keys().filter_map(Value::as_str).map(str::to_owned).collect(),
            _ => vec![],
        };
        s.command = match svc.get("command") {
            Some(Value::Sequence(items)) => Some(items.iter().filter_map(Value::as_str).map(str::to_owned).collect()),
            Some(Value::String(t)) if t.contains(['"', '\'', '&', '|', ';', '$']) => {
                Some(vec!["sh".into(), "-c".into(), t.clone()])
            }
            Some(Value::String(t)) => Some(t.split_whitespace().map(str::to_owned).collect()),
            _ => None,
        };
        out.push(s);
    }
    Some(Compose { path: rel.to_owned(), services: out })
}

fn manifest(path: &Path, lower: &str, dir: &str) -> Option<Stack> {
    let stack = |kind, start: Option<String>, framework: Option<String>| Stack {
        kind,
        dir: dir.to_owned(),
        manifest: if dir == "." { lower.to_owned() } else { format!("{dir}/{lower}") },
        start,
        framework,
    };
    match lower {
        "package.json" => {
            let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
            let start = v.pointer("/scripts/start").and_then(|s| s.as_str()).map(str::to_owned);
            let deps = deps_of(&v);
            let framework = ["next", "nuxt", "@sveltejs/kit", "astro", "@remix-run/node", "express", "fastify", "hono"]
                .into_iter()
                .find(|f| deps.contains(*f))
                .map(str::to_owned);
            Some(stack(StackKind::Node, start, framework))
        }
        "pyproject.toml" | "requirements.txt" | "pipfile" => {
            let text = std::fs::read_to_string(path).unwrap_or_default().to_ascii_lowercase();
            let framework = ["django", "fastapi", "flask", "starlette", "streamlit"]
                .into_iter()
                .find(|f| text.contains(f))
                .map(str::to_owned);
            Some(stack(StackKind::Python, None, framework))
        }
        "go.mod" => Some(stack(StackKind::Go, None, None)),
        "cargo.toml" => {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            // A workspace root without a package is not itself an app.
            let doc: toml::Table = text.parse().ok()?;
            doc.contains_key("package").then(|| {
                let framework = ["axum", "actix-web", "rocket", "warp", "poem"]
                    .into_iter()
                    .find(|f| text.contains(f))
                    .map(str::to_owned);
                stack(StackKind::Rust, None, framework)
            })
        }
        "gemfile" => {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            let framework = ["rails", "sinatra"].into_iter().find(|f| text.contains(f)).map(str::to_owned);
            Some(stack(StackKind::Ruby, None, framework))
        }
        "composer.json" => Some(stack(StackKind::Php, None, None)),
        "mix.exs" => Some(stack(StackKind::Elixir, None, None)),
        "pom.xml" | "build.gradle" | "build.gradle.kts" => Some(stack(StackKind::Java, None, None)),
        "deno.json" | "deno.jsonc" => Some(stack(StackKind::Deno, None, None)),
        _ => None,
    }
}

fn deps_of(package: &serde_json::Value) -> BTreeSet<String> {
    ["dependencies", "devDependencies"]
        .iter()
        .filter_map(|k| package.get(k)?.as_object())
        .flat_map(|m| m.keys().cloned())
        .collect()
}

/// Backing services a manifest's dependencies point at.
fn needs_from_manifest(path: &Path) -> Vec<Need> {
    const PG: &[&str] = &[
        "\"pg\"",
        "\"postgres\"",
        "psycopg",
        "asyncpg",
        "tokio-postgres",
        "github.com/lib/pq",
        "github.com/jackc/pgx",
        "\"pg\":",
        "gem \"pg\"",
        "gem 'pg'",
        "postgrex",
    ];
    const REDIS: &[&str] = &[
        "\"redis\"",
        "ioredis",
        "bullmq",
        "\"bull\"",
        "go-redis",
        "redis-py",
        "\nredis",
        "gem \"redis\"",
        "gem 'redis'",
        "redix",
    ];
    let Ok(text) = std::fs::read_to_string(path) else { return vec![] };
    let text = format!("\n{}", text.to_ascii_lowercase());
    let mut out = Vec::new();
    if PG.iter().any(|m| text.contains(m)) {
        out.push(Need::Postgres);
    }
    if REDIS.iter().any(|m| text.contains(m)) {
        out.push(Need::Redis);
    }
    out
}

#[cfg(test)]
mod tests;
