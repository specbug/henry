//! Where Henry keeps its things, and the config that lives there.
//!
//! On the Mac that is `~/Library/Application Support/Henry`, deliberately
//! outside `~/Documents`: macOS's privacy layer (TCC) denies background
//! agents there, which is what made hostd read manifests through `git show`.
//! `HENRY_HOME` overrides it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Clone)]
pub struct Home {
    pub root: PathBuf,
}

impl Home {
    pub fn locate() -> Result<Home> {
        if let Some(root) = std::env::var_os("HENRY_HOME") {
            return Ok(Home { root: root.into() });
        }
        let home = std::env::var_os("HOME").context("HOME is not set; set HENRY_HOME instead")?;
        let home = PathBuf::from(home);
        let root = if cfg!(target_os = "macos") {
            home.join("Library/Application Support/Henry")
        } else {
            match std::env::var_os("XDG_DATA_HOME") {
                Some(data) => PathBuf::from(data).join("henry"),
                None => home.join(".local/share/henry"),
            }
        };
        Ok(Home { root })
    }

    /// Create the directory tree, private to you.
    pub fn ensure(&self) -> Result<()> {
        for dir in [self.root.clone(), self.checkouts(), self.logs(), self.caddy_dir()] {
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(())
    }

    pub fn canon(&self) -> PathBuf {
        self.root.join("canon.db")
    }

    pub fn config(&self) -> PathBuf {
        self.root.join("henry.toml")
    }

    pub fn lock(&self) -> PathBuf {
        self.root.join("henry.lock")
    }

    /// The master key, where no Keychain is available.
    #[cfg(not(target_os = "macos"))]
    pub fn key_file(&self) -> PathBuf {
        self.root.join("master.key")
    }

    /// Henry's own clones. Your working copies are never touched.
    pub fn checkouts(&self) -> PathBuf {
        self.root.join("checkouts")
    }

    /// Stand-ins for commands other tools call by name: `docker`, for
    /// Railpack, is really Podman on the Mac.
    pub fn shims(&self) -> PathBuf {
        self.root.join("shims")
    }

    /// Binaries Henry owns and upgrades: `tools/<name>/<version>/`.
    pub fn tools(&self) -> PathBuf {
        self.root.join("tools")
    }

    /// The binary to run for a tool: Henry's own copy when it manages one,
    /// otherwise what the config names.
    pub fn tool(&self, name: &str, configured: &std::path::Path) -> PathBuf {
        mend::tools::Tools::current(&self.tools(), name).unwrap_or_else(|| configured.to_path_buf())
    }

    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// Caddy's own state: its autosaved config, so `--resume` brings routes back.
    pub fn caddy_dir(&self) -> PathBuf {
        self.root.join("caddy")
    }

    /// The config Caddy boots with before its first resume: just the admin
    /// endpoint, so Hinge can reach it.
    pub fn caddy_boot(&self) -> PathBuf {
        self.caddy_dir().join("boot.json")
    }

    /// The tunnel token, for cloudflared's `--token-file`. Mode 0600.
    pub fn tunnel_token(&self) -> PathBuf {
        self.root.join("tunnel.token")
    }

    /// Glass's login token, for browsers on the Mac. Mode 0600.
    pub fn glass_token(&self) -> PathBuf {
        self.root.join("glass.token")
    }

    /// Glass's login token, made the first time it is asked for. Delete the
    /// file and restart Henry to sign every browser out.
    pub fn ensure_glass_token(&self) -> Result<String> {
        let path = self.glass_token();
        if let Ok(t) = std::fs::read_to_string(&path)
            && t.trim().len() >= 32
        {
            return Ok(t.trim().to_owned());
        }
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness for Glass's token: {e}"))?;
        let token = hex::encode(bytes);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        use std::io::Write as _;
        opts.open(&path)
            .and_then(|mut f| f.write_all(token.as_bytes()))
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(token)
    }
}

/// `henry.toml`. Every field has a default, so the file is optional.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub limits: LimitsConfig,
    pub ai: AiConfig,
    pub engine: EngineConfig,
    pub press: PressConfig,
    pub http: HttpConfig,
    pub hinge: HingeConfig,
    pub proof: ProofConfig,
    /// Unset: no backups.
    pub ark: Option<ArkConfig>,
    pub mend: MendConfig,
    /// Unset: no tunnel, and Caddy serves on loopback only.
    pub cloudflare: Option<CloudflareConfig>,
    pub glass: GlassConfig,
}

/// Glass, the web UI. It always answers on Henry's loopback port; with all
/// three of these set it is also public, behind Cloudflare Access.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GlassConfig {
    /// The public hostname, e.g. `henry.example.com`.
    pub hostname: Option<String>,
    /// The Zero Trust team name, e.g. `acme` for `acme.cloudflareaccess.com`.
    pub access_team: Option<String>,
    /// The Access application's AUD tag.
    pub access_aud: Option<String>,
}

impl GlassConfig {
    /// The Access check, when Glass is public. A hostname without Access is
    /// refused: Glass is never public on its own.
    pub fn access(&self) -> Result<Option<(&str, &str, &str)>> {
        match (&self.hostname, &self.access_team, &self.access_aud) {
            (None, None, None) => Ok(None),
            (Some(h), Some(t), Some(a)) if !h.is_empty() && !t.is_empty() && !a.is_empty() => Ok(Some((h, t, a))),
            _ => anyhow::bail!(
                "[glass] needs hostname, access_team and access_aud together: Glass is public only behind Cloudflare Access"
            ),
        }
    }
}

/// Caddy, the proxy every request passes through on the Mac.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HingeConfig {
    pub caddy: std::path::PathBuf,
    /// Caddy's admin endpoint.
    pub admin: String,
    /// Where Caddy listens for the tunnel.
    pub listen: String,
    /// A public hostname for Henry's own endpoints. GitHub's webhook goes to
    /// `https://<hooks_host>/hooks/github`.
    pub hooks_host: Option<String>,
}

impl Default for HingeConfig {
    fn default() -> Self {
        HingeConfig {
            caddy: "/opt/homebrew/bin/caddy".into(),
            admin: "http://127.0.0.1:2019".into(),
            listen: "127.0.0.1:8080".into(),
            hooks_host: None,
        }
    }
}

/// The tunnel. Its API token is `henry/CLOUDFLARE_API_TOKEN` and the token
/// cloudflared runs with is `henry/CLOUDFLARE_TUNNEL_TOKEN`, both in Hush.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudflareConfig {
    pub account: String,
    pub tunnel: String,
    /// Zones Henry may create DNS records in, e.g. `["example.com"]`.
    pub zones: Vec<String>,
    #[serde(default = "default_cloudflared")]
    pub cloudflared: std::path::PathBuf,
    #[serde(default = "default_api")]
    pub api: String,
}

fn default_cloudflared() -> std::path::PathBuf {
    "/opt/homebrew/bin/cloudflared".into()
}

fn default_api() -> String {
    "https://api.cloudflare.com/client/v4".into()
}

/// Builds and deploys on push.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PressConfig {
    /// The CLI that builds images: `podman` or `docker`. Unset: Podman on
    /// macOS, otherwise whichever of the two is on PATH.
    pub builder: Option<std::path::PathBuf>,
    /// How often each auto-deploying app's branch is checked.
    pub poll_seconds: u64,
    /// A build running longer than this is stopped and the release fails.
    pub build_timeout_minutes: u64,
    /// The `railpack` binary, for repos without a Dockerfile. Unset: found on
    /// PATH or in Homebrew's bin.
    pub railpack: Option<std::path::PathBuf>,
    /// The BuildKit image Railpack builds with, run as `henry-buildkit`.
    pub buildkit_image: String,
}

impl Default for PressConfig {
    fn default() -> Self {
        PressConfig {
            builder: None,
            poll_seconds: 60,
            build_timeout_minutes: 30,
            railpack: None,
            // The BuildKit Railpack is built against (its go.mod): an older
            // one can refuse what Railpack asks of it.
            buildkit_image: "moby/buildkit:v0.34.0".into(),
        }
    }
}

/// The Live gate: how long a release must hold before it goes live, and how
/// patient Henry is with a live app that stops answering.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProofConfig {
    /// Minutes a release must run cleanly before its restart test.
    pub soak_minutes: u64,
    /// Passing health checks in a row, per service, the soak must end on.
    pub streak: u32,
    /// A soak that has not held after this long fails the release.
    pub soak_limit_minutes: u64,
    /// Failing checks in a row before a live app is Degraded.
    pub degraded_after: u32,
    /// Failing checks in a row before its container is restarted.
    pub restart_after: u32,
    /// More log lines that mean a crash, on top of the built-in ones.
    pub signatures: Vec<String>,
}

impl Default for ProofConfig {
    fn default() -> Self {
        let d = proof::Settings::default();
        ProofConfig {
            soak_minutes: d.soak.as_secs() / 60,
            streak: d.streak,
            soak_limit_minutes: d.soak_limit.as_secs() / 60,
            degraded_after: d.degraded_after,
            restart_after: d.restart_after,
            signatures: vec![],
        }
    }
}

impl ProofConfig {
    /// Proof's settings. `proxy` is Caddy's listen address, so public routes
    /// are checked end to end.
    pub fn settings(&self, proxy: &str) -> proof::Settings {
        let mut s = proof::Settings {
            soak: std::time::Duration::from_secs(self.soak_minutes * 60),
            streak: self.streak.max(1),
            // The streak needs time to build after the soak.
            soak_limit: std::time::Duration::from_secs(self.soak_limit_minutes.max(self.soak_minutes + 10) * 60),
            degraded_after: self.degraded_after.max(1),
            restart_after: self.restart_after.max(self.degraded_after.max(1)),
            proxy: Some(proxy.to_owned()),
            ..proof::Settings::default()
        };
        s.signatures.extend(self.signatures.iter().cloned());
        s
    }
}

/// Backups. Secrets: `henry/RESTIC_PASSWORD` (keep a copy; without it the
/// backups cannot be read), and for R2 `henry/R2_ACCESS_KEY_ID` and
/// `henry/R2_SECRET_ACCESS_KEY`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArkConfig {
    /// A restic repository, e.g.
    /// `s3:https://<account>.r2.cloudflarestorage.com/<bucket>/henry`.
    pub repository: String,
    /// More repositories that get a copy every day, e.g. `rclone:gdrive:henry`.
    #[serde(default)]
    pub mirrors: Vec<String>,
    /// The `restic` binary. Unset: found on PATH or in Homebrew's bin.
    #[serde(default)]
    pub restic: Option<std::path::PathBuf>,
    #[serde(default = "ark_every_hours")]
    pub every_hours: u64,
    #[serde(default = "ark_drill_days")]
    pub drill_every_days: u64,
    #[serde(default = "ark_daily")]
    pub keep_daily: u32,
    #[serde(default = "ark_weekly")]
    pub keep_weekly: u32,
    #[serde(default = "ark_monthly")]
    pub keep_monthly: u32,
}

fn ark_every_hours() -> u64 {
    24
}
fn ark_drill_days() -> u64 {
    30
}
fn ark_daily() -> u32 {
    7
}
fn ark_weekly() -> u32 {
    4
}
fn ark_monthly() -> u32 {
    6
}

/// Upkeep. Every task runs unless listed in `off`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MendConfig {
    /// Tasks not to run: any of disk, refresh, host, tools, bench.
    pub off: Vec<String>,
    /// Releases per app whose images are kept for a rollback.
    pub keep_releases: usize,
    pub log_days: u64,
    /// Model tokens one Bench run of one model may spend.
    pub bench_tokens: u64,
    /// Where tool releases are looked up.
    pub github_api: String,
}

impl Default for MendConfig {
    fn default() -> Self {
        MendConfig {
            off: vec![],
            keep_releases: 3,
            log_days: 30,
            bench_tokens: 1_500_000,
            github_api: "https://api.github.com".into(),
        }
    }
}

impl MendConfig {
    pub fn tasks(&self) -> Vec<mend::Task> {
        mend::Task::ALL.into_iter().filter(|t| !self.off.iter().any(|o| o == t.as_str())).collect()
    }
}

/// Henry's own HTTP endpoint. Loopback only: Hinge exposes the webhook path.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HttpConfig {
    pub listen: String,
}

impl Default for HttpConfig {
    fn default() -> Self {
        HttpConfig { listen: "127.0.0.1:7777".into() }
    }
}

/// Where the containers run.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EngineConfig {
    /// A Docker-compatible socket to use as is. Unset: on macOS, the Podman
    /// machine's socket; elsewhere, the first Podman or Docker socket found.
    pub socket: Option<std::path::PathBuf>,
    /// The `podman` binary, for managing the machine on macOS.
    pub podman: Option<std::path::PathBuf>,
    /// A Podman machine other than the default one.
    pub machine: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LimitsConfig {
    /// The most memory one service may ask for, e.g. `8GiB`.
    pub max_service_memory: String,
    pub max_service_cpus: f64,
    pub max_services_per_app: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        let d = lease::Limits::default();
        LimitsConfig {
            max_service_memory: format!("{}GiB", d.max_memory_bytes >> 30),
            max_service_cpus: d.max_cpus,
            max_services_per_app: d.max_services,
        }
    }
}

/// Ghost: which model plans onboardings, and how much it may spend.
/// Provider keys are secrets: `henry secret set henry/META_API_KEY` (or
/// GEMINI_API_KEY, ANTHROPIC_API_KEY, OPENAI_API_KEY).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AiConfig {
    /// `provider/model`, e.g. `meta/Llama-4-Maverick-17B-128E-Instruct-FP8`.
    /// Unset: the first provider with a key, Meta first, at its default model.
    pub default_model: Option<String>,
    /// Plans one onboarding may try before it is stuck.
    pub max_rounds: i64,
    /// Model tokens, in and out, one onboarding may spend.
    pub max_tokens: u64,
    /// Endpoint overrides by provider, e.g. `meta = "https://…/v1"`.
    pub endpoints: std::collections::BTreeMap<String, String>,
    /// Other `provider/model`s Bench scores against the default each week.
    pub candidates: Vec<String>,
}

impl Default for AiConfig {
    fn default() -> Self {
        AiConfig {
            default_model: None,
            max_rounds: 5,
            max_tokens: 2_000_000,
            endpoints: Default::default(),
            candidates: vec![],
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("reading {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn limits(&self) -> Result<lease::Limits> {
        let memory = lease::parse_bytes(&self.limits.max_service_memory).with_context(|| {
            format!("limits.max_service_memory: `{}` is not a size", self.limits.max_service_memory)
        })?;
        Ok(lease::Limits {
            max_services: self.limits.max_services_per_app,
            max_memory_bytes: memory,
            max_cpus: self.limits.max_service_cpus,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_config_is_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let c = Config::load(&dir.path().join("henry.toml")).unwrap();
        let l = c.limits().unwrap();
        let d = lease::Limits::default();
        assert_eq!((l.max_memory_bytes, l.max_cpus, l.max_services), (d.max_memory_bytes, d.max_cpus, d.max_services));
    }

    #[test]
    fn config_overrides_and_rejects_typos() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("henry.toml");
        std::fs::write(&path, "[limits]\nmax_service_memory = \"2GiB\"\n[ai]\ndefault_model = \"meta/muse-spark\"\n")
            .unwrap();
        let c = Config::load(&path).unwrap();
        assert_eq!(c.limits().unwrap().max_memory_bytes, 2 << 30);
        assert_eq!(c.ai.default_model.as_deref(), Some("meta/muse-spark"));

        std::fs::write(&path, "[limit]\nmax_service_memory = \"2GiB\"\n").unwrap();
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn home_dirs_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home { root: dir.path().join("henry") };
        home.ensure().unwrap();
        assert!(home.checkouts().is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&home.root).unwrap().permissions().mode() & 0o777, 0o700);
        }
    }
}
