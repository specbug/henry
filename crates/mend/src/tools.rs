//! The binaries Henry owns: Caddy, cloudflared, restic and Railpack.
//!
//! Each version lives in its own directory, `tools/<name>/<version>/`, and
//! `tools/<name>/current` is a link to one of them, swapped atomically.
//! Henry and its LaunchAgents run `current`. An upgrade downloads the new
//! release from GitHub, checks its published checksum, switches the link,
//! and keeps it only if a gate passes; otherwise the link goes back. Canon
//! records the switch before it happens, so a crash mid-switch is undone
//! at the next start ([`Tools::recover`]).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use canon::Canon;
use serde_json::Value;
use sha2::Digest;
use tracing::{info, warn};

use crate::{Error, Result};

/// How a release publishes its checksums.
#[derive(Debug, Clone, Copy)]
pub enum Sums {
    /// A `<hash>  <file>` list in this asset, SHA-256.
    Sha256(fn(&str) -> String),
    /// The same, SHA-512.
    Sha512(fn(&str) -> String),
    /// `<file>: <sha256>` lines in the release notes.
    InNotes,
}

/// One tool Henry can own.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub name: &'static str,
    /// `owner/repo` on GitHub.
    pub repo: &'static str,
    /// The asset for a version on this platform (`os`, `arch` as Rust names them).
    pub asset: fn(&str, &str, &str) -> Option<String>,
    pub sums: Sums,
}

fn caddy(v: &str, os: &str, arch: &str) -> Option<String> {
    let os = match os {
        "macos" => "mac",
        "linux" => "linux",
        _ => return None,
    };
    let arch = match arch {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        _ => return None,
    };
    Some(format!("caddy_{v}_{os}_{arch}.tar.gz"))
}

fn cloudflared(_v: &str, os: &str, arch: &str) -> Option<String> {
    let arch = match arch {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        _ => return None,
    };
    match os {
        "macos" => Some(format!("cloudflared-darwin-{arch}.tgz")),
        "linux" => Some(format!("cloudflared-linux-{arch}")),
        _ => None,
    }
}

fn restic(v: &str, os: &str, arch: &str) -> Option<String> {
    let os = match os {
        "macos" => "darwin",
        "linux" => "linux",
        _ => return None,
    };
    let arch = match arch {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        _ => return None,
    };
    Some(format!("restic_{v}_{os}_{arch}.bz2"))
}

fn railpack(v: &str, os: &str, arch: &str) -> Option<String> {
    let arch = match arch {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        _ => return None,
    };
    match os {
        "macos" => Some(format!("railpack-v{v}-{arch}-apple-darwin.tar.gz")),
        "linux" => Some(format!("railpack-v{v}-{arch}-unknown-linux-musl.tar.gz")),
        _ => None,
    }
}

pub const SPECS: &[Spec] = &[
    Spec {
        name: "caddy",
        repo: "caddyserver/caddy",
        asset: caddy,
        sums: Sums::Sha512(|v| format!("caddy_{v}_checksums.txt")),
    },
    Spec { name: "cloudflared", repo: "cloudflare/cloudflared", asset: cloudflared, sums: Sums::InNotes },
    Spec { name: "restic", repo: "restic/restic", asset: restic, sums: Sums::Sha256(|_| "SHA256SUMS".into()) },
    Spec {
        name: "railpack",
        repo: "railwayapp/railpack",
        asset: railpack,
        sums: Sums::Sha256(|v| format!("railpack_{v}_checksums.txt")),
    },
];

pub fn spec(name: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.name == name)
}

/// A release, resolved to the one file this Mac needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: String,
    pub asset: String,
    pub url: String,
    /// Hex digest, and whether it is SHA-512 (else SHA-256).
    pub digest: String,
    pub sha512: bool,
}

/// Proves a newly switched tool works. Caddy's gate, say, restarts Caddy
/// and checks every public route still answers.
pub trait Gate: Send + Sync {
    fn check(&self, tool: &str) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

#[derive(Debug, Clone, PartialEq)]
pub enum Upgrade {
    Current(String),
    Upgraded { from: String, to: String },
    RolledBack { from: String, to: String, why: String },
}

impl std::fmt::Display for Upgrade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Upgrade::Current(v) => write!(f, "{v} is current"),
            Upgrade::Upgraded { from, to } => write!(f, "{from} → {to}"),
            Upgrade::RolledBack { from, to, why } => write!(f, "{to} failed its check and {from} is back: {why}"),
        }
    }
}

pub struct Tools {
    /// `<home>/tools`.
    pub root: PathBuf,
    /// `https://api.github.com`, or a test server.
    pub api: String,
    pub token: Option<String>,
    pub http: reqwest::Client,
}

impl Tools {
    pub fn new(root: PathBuf, api: &str, token: Option<String>) -> Result<Tools> {
        let http = reqwest::Client::builder()
            .user_agent("henry")
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|e| Error::Failed(e.to_string()))?;
        Ok(Tools { root, api: api.trim_end_matches('/').to_owned(), token, http })
    }

    /// The path Henry runs a managed tool from, if it is managed here.
    pub fn current(root: &Path, name: &str) -> Option<PathBuf> {
        let path = root.join(name).join("current").join(name);
        path.exists().then_some(path)
    }

    async fn get(&self, url: &str) -> Result<reqwest::Response> {
        let mut req = self.http.get(url);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.map_err(|e| Error::Transient(format!("{url}: {e}")))?;
        match resp.status() {
            s if s.is_success() => Ok(resp),
            s if s.as_u16() == 403 || s.as_u16() == 429 || s.is_server_error() => {
                Err(Error::Transient(format!("{url} answered {s}")))
            }
            s => Err(Error::Failed(format!("{url} answered {s}"))),
        }
    }

    /// The newest release of a tool, for this platform, with its checksum.
    pub async fn latest(&self, spec: &Spec) -> Result<Release> {
        let url = format!("{}/repos/{}/releases/latest", self.api, spec.repo);
        let v: Value = self.get(&url).await?.json().await.map_err(|e| Error::Transient(e.to_string()))?;
        let tag = v["tag_name"].as_str().ok_or_else(|| Error::Failed(format!("{url}: no tag_name")))?;
        let version = tag.trim_start_matches('v').to_owned();
        let asset = (spec.asset)(&version, std::env::consts::OS, std::env::consts::ARCH)
            .ok_or_else(|| Error::Failed(format!("{} has no build for this Mac", spec.name)))?;
        let assets = v["assets"].as_array().cloned().unwrap_or_default();
        let find = |name: &str| {
            assets
                .iter()
                .find(|a| a["name"] == name)
                .and_then(|a| a["browser_download_url"].as_str())
                .map(str::to_owned)
        };
        let url = find(&asset).ok_or_else(|| Error::Failed(format!("{} {version} has no {asset}", spec.name)))?;
        let (sums, sha512) = match spec.sums {
            Sums::Sha256(file) | Sums::Sha512(file) => {
                // GoReleaser's default name, or the plain one a project may
                // switch to (Railpack did): the digest is checked either way.
                let list = find(&file(&version))
                    .or_else(|| matches!(spec.sums, Sums::Sha256(_)).then(|| find("checksums.txt")).flatten())
                    .ok_or_else(|| Error::Failed(format!("{} {version} publishes no checksums", spec.name)))?;
                (
                    self.get(&list).await?.text().await.map_err(|e| Error::Transient(e.to_string()))?,
                    matches!(spec.sums, Sums::Sha512(_)),
                )
            }
            Sums::InNotes => (v["body"].as_str().unwrap_or_default().to_owned(), false),
        };
        let digest = digest_for(&sums, &asset, if sha512 { 128 } else { 64 })
            .ok_or_else(|| Error::Failed(format!("no checksum for {asset}; refusing to install it")))?;
        Ok(Release { version, asset, url, digest, sha512 })
    }

    /// Download, verify and unpack a release into its own directory.
    /// Installing a version that is already there does nothing.
    pub async fn install(&self, spec: &Spec, r: &Release) -> Result<PathBuf> {
        let dir = self.root.join(spec.name).join(&r.version);
        let bin = dir.join(spec.name);
        if bin.exists() {
            return Ok(bin);
        }
        let bytes = self.get(&r.url).await?.bytes().await.map_err(|e| Error::Transient(e.to_string()))?;
        let got = if r.sha512 {
            hex::encode(sha2::Sha512::digest(&bytes))
        } else {
            hex::encode(sha2::Sha256::digest(&bytes))
        };
        if !got.eq_ignore_ascii_case(&r.digest) {
            return Err(Error::Failed(format!("{} does not match its published checksum", r.asset)));
        }
        let binary = unpack(&r.asset, &bytes, spec.name)?;
        // Write beside, then rename: a half-written binary is never "installed".
        let tmp = self.root.join(spec.name).join(format!(".{}.partial", r.version));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        std::fs::write(tmp.join(spec.name), binary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(tmp.join(spec.name), std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&tmp, &dir)?;
        Ok(bin)
    }

    /// Point `current` at a version, atomically.
    pub fn switch(&self, name: &str, version: &str) -> Result<()> {
        let base = self.root.join(name);
        let link = base.join("current");
        let tmp = base.join(".current.next");
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(version, &tmp)?;
        std::fs::rename(&tmp, &link)?;
        Ok(())
    }

    /// First install: the newest release, made current, no gate (there is
    /// nothing to fall back to).
    pub async fn adopt(&self, canon: &Canon, name: &str) -> Result<String> {
        let spec = spec(name).ok_or_else(|| Error::Failed(format!("Henry does not manage `{name}`")))?;
        let r = self.latest(spec).await?;
        self.install(spec, &r).await?;
        self.switch(name, &r.version)?;
        canon.set_tool(name, &r.version, None, None)?;
        Ok(r.version)
    }

    /// Upgrade a managed tool if a newer release exists, keeping it only if
    /// `gate` passes.
    pub async fn upgrade<G: Gate>(&self, canon: &Canon, name: &str, gate: &G) -> Result<Upgrade> {
        let spec = spec(name).ok_or_else(|| Error::Failed(format!("Henry does not manage `{name}`")))?;
        let Some(tool) = canon.tool(name)? else {
            return Err(Error::Failed(format!("`{name}` is not installed by Henry; `henry tools adopt {name}`")));
        };
        let r = self.latest(spec).await?;
        if r.version == tool.current {
            return Ok(Upgrade::Current(r.version));
        }
        self.install(spec, &r).await?;
        // Written before the switch: a crash from here on is rolled back.
        canon.set_tool(name, &tool.current, tool.previous.as_deref(), Some(&r.version))?;
        self.switch(name, &r.version)?;
        info!(tool = name, from = %tool.current, to = %r.version, "switched; checking");
        match gate.check(name).await {
            Ok(()) => {
                canon.set_tool(name, &r.version, Some(&tool.current), None)?;
                self.prune(name, &[&r.version, &tool.current]);
                Ok(Upgrade::Upgraded { from: tool.current, to: r.version })
            }
            Err(why) => {
                warn!(tool = name, version = %r.version, "failed its check, rolling back: {why}");
                self.switch(name, &tool.current)?;
                canon.set_tool(name, &tool.current, tool.previous.as_deref(), None)?;
                let keep: Vec<&str> =
                    [Some(tool.current.as_str()), tool.previous.as_deref()].into_iter().flatten().collect();
                self.prune(name, &keep);
                canon.record(
                    None,
                    "mend.rollback",
                    &format!("{name} {} failed its check and {} is back: {why}", r.version, tool.current),
                    &Value::Null,
                )?;
                // The old version's processes come back the same way.
                let _ = gate.check(name).await;
                Ok(Upgrade::RolledBack { from: tool.current, to: r.version, why })
            }
        }
    }

    /// Undo any switch a crash left unproven.
    pub fn recover(&self, canon: &Canon) -> Result<Vec<String>> {
        let mut undone = Vec::new();
        for t in canon.tools()? {
            if let Some(pending) = &t.pending {
                self.switch(&t.name, &t.current)?;
                canon.set_tool(&t.name, &t.current, t.previous.as_deref(), None)?;
                warn!(tool = %t.name, %pending, "an unproven switch was undone");
                undone.push(t.name);
            }
        }
        Ok(undone)
    }

    /// Remove every installed version but `keep`.
    fn prune(&self, name: &str, keep: &[&str]) {
        let Ok(entries) = std::fs::read_dir(self.root.join(name)) else { return };
        for e in entries.flatten() {
            let v = e.file_name().to_string_lossy().into_owned();
            // `current` is a link to a version, not a version.
            let real_dir = e.file_type().is_ok_and(|t| t.is_dir());
            if real_dir && !v.starts_with('.') && !keep.contains(&v.as_str()) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}

/// The hex digest for `asset` in a checksum list or release notes.
pub fn digest_for(text: &str, asset: &str, len: usize) -> Option<String> {
    text.lines()
        .find(|l| l.contains(asset))
        .and_then(|l| l.split(|c: char| !c.is_ascii_hexdigit()).find(|w| w.len() == len).map(str::to_lowercase))
}

/// The tool's binary out of whatever the release ships.
fn unpack(asset: &str, bytes: &[u8], name: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    if asset.ends_with(".tar.gz") || asset.ends_with(".tgz") {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            if path.file_name().is_some_and(|f| f == name) && entry.header().entry_type().is_file() {
                entry.read_to_end(&mut out)?;
                return Ok(out);
            }
        }
        Err(Error::Failed(format!("{asset} holds no `{name}`")))
    } else if asset.ends_with(".bz2") {
        bzip2::read::BzDecoder::new(bytes).read_to_end(&mut out)?;
        Ok(out)
    } else {
        Ok(bytes.to_vec())
    }
}

#[cfg(test)]
mod tests;
