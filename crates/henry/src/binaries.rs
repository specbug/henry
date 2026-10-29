//! The binaries Henry runs, found as its agents find them: Henry's own copy
//! when it manages one, else what henry.toml names, else the first on the
//! agents' PATH (launchd's, not your shell's). `henry tools` shows the same.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::home::{ArkConfig, Config, Home};

/// The tools Henry runs, in the order `henry tools` lists them.
pub const NAMES: [&str; 4] = ["caddy", "cloudflared", "restic", "railpack"];

/// The first `name` on the PATH Henry's agents run with.
pub fn which(name: &str) -> Option<PathBuf> {
    crate::launchd::PATH.split(':').map(|d| Path::new(d).join(name)).find(|p| p.is_file())
}

pub fn caddy(home: &Home, config: &Config) -> PathBuf {
    home.tool("caddy", &config.hinge.caddy)
}

pub fn cloudflared(home: &Home, config: &Config) -> PathBuf {
    let configured =
        config.cloudflare.as_ref().map_or_else(crate::home::default_cloudflared, |c| c.cloudflared.clone());
    home.tool("cloudflared", &configured)
}

pub fn restic(home: &Home, ark: Option<&ArkConfig>) -> PathBuf {
    match ark.and_then(|a| a.restic.clone()) {
        Some(p) => p,
        None => home.tool("restic", &which("restic").unwrap_or_else(|| "/opt/homebrew/bin/restic".into())),
    }
}

/// Railpack, if Henry can find one.
pub fn railpack(home: &Home, config: &Config) -> Option<PathBuf> {
    config
        .press
        .railpack
        .clone()
        .or_else(|| mend::tools::Tools::current(&home.tools(), "railpack"))
        .or_else(|| which("railpack"))
}

/// One tool as `henry tools` reports it.
pub struct Found {
    pub name: &'static str,
    /// Unset: Henry finds none.
    pub path: Option<PathBuf>,
    /// Where it came from, in words.
    pub source: String,
}

/// Every tool Henry runs, where it is, and how it got there.
pub fn found(home: &Home, config: &Config) -> Vec<Found> {
    NAMES
        .into_iter()
        .map(|name| {
            let path = match name {
                "caddy" => Some(caddy(home, config)),
                "cloudflared" => Some(cloudflared(home, config)),
                "restic" => Some(restic(home, config.ark.as_ref())),
                _ => railpack(home, config),
            }
            .filter(|p| p.is_file());
            let managed = mend::tools::Tools::current(&home.tools(), name);
            let source = match &path {
                None => "not found where Henry looks".to_owned(),
                Some(p) if managed.as_ref() == Some(p) => "Henry's own copy, upgraded weekly".to_owned(),
                Some(p) if p.starts_with("/opt/homebrew") || p.starts_with("/usr/local/Cellar") => {
                    "Homebrew (brew upgrade keeps it current)".to_owned()
                }
                Some(_) => "your own install".to_owned(),
            };
            Found { name, path, source }
        })
        .collect()
}

/// What `path --version` says, first line, or nothing within a few seconds.
pub async fn version(name: &str, path: &Path) -> Option<String> {
    let arg = if name == "restic" { "version" } else { "--version" };
    let run = tokio::process::Command::new(path).arg(arg).kill_on_drop(true).output();
    let out = tokio::time::timeout(Duration::from_secs(5), run).await.ok()?.ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.lines().next().map(|l| l.trim().to_owned()).filter(|l| !l.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn henry_looks_on_its_agents_path_not_your_shells() {
        // launchd's PATH, whatever the shell running this says.
        assert!(which("sh").is_some_and(|p| ["/bin/sh", "/usr/bin/sh"].contains(&p.to_str().unwrap())));
        assert_eq!(which("henry-no-such-tool"), None);
    }

    #[test]
    fn a_tool_henry_cannot_find_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home { root: dir.path().join("h") };
        let mut config = Config::default();
        config.hinge.caddy = dir.path().join("no-caddy");
        config.press.railpack = Some(dir.path().join("no-railpack"));
        let seen = found(&home, &config);
        let caddy = seen.iter().find(|f| f.name == "caddy").unwrap();
        assert!(caddy.path.is_none() && caddy.source.contains("not found"));
        let railpack = seen.iter().find(|f| f.name == "railpack").unwrap();
        assert!(railpack.path.is_none());

        std::fs::write(dir.path().join("own-railpack"), "").unwrap();
        config.press.railpack = Some(dir.path().join("own-railpack"));
        let seen = found(&home, &config);
        let railpack = seen.iter().find(|f| f.name == "railpack").unwrap();
        assert_eq!(railpack.source, "your own install");
    }
}
