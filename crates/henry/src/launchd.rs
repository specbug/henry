//! Henry and its edge as LaunchAgents: started at login, restarted if they die.
//!
//! Agents, not daemons, on purpose. Podman's VM and the login Keychain both
//! belong to your session, so Henry runs in it. That is why the host needs
//! auto-login (see the concept doc, "Restarts and crashes").
//!
//! Caddy and cloudflared get agents of their own rather than being Henry's
//! children: requests never go through Henry, so Henry can restart or
//! replace itself without the edge noticing.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::home::{Config, Home};

pub const LABEL: &str = "dev.henry";
pub const CADDY: &str = "dev.henry.caddy";
pub const CLOUDFLARED: &str = "dev.henry.cloudflared";

/// Homebrew first: podman, cloudflared, caddy and restic live there.
const PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

pub struct Agent {
    pub label: &'static str,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub log: PathBuf,
}

impl Agent {
    pub fn plist(&self) -> String {
        let args: String = self.args.iter().map(|a| format!("        <string>{}</string>\n", xml(a))).collect();
        let env: String = [("PATH".to_owned(), PATH.to_owned())]
            .iter()
            .chain(&self.env)
            .map(|(k, v)| format!("        <key>{}</key>\n        <string>{}</string>\n", xml(k), xml(v)))
            .collect();
        let log = xml(&self.log.display().to_string());
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{args}    </array>
    <key>EnvironmentVariables</key>
    <dict>
{env}    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>ProcessType</key>
    <string>Standard</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
            label = self.label,
        )
    }
}

/// Henry itself.
pub fn henry(binary: &Path, home: &Home) -> Agent {
    Agent {
        label: LABEL,
        args: vec![binary.display().to_string(), "run".into()],
        env: vec![("HENRY_HOME".into(), home.root.display().to_string())],
        log: home.logs().join("henry.log"),
    }
}

/// Caddy, resuming its last config so routes survive any restart, Henry's
/// included. Its state lives under Henry's home.
pub fn caddy(home: &Home, config: &Config) -> Agent {
    Agent {
        label: CADDY,
        args: vec![
            home.tool("caddy", &config.hinge.caddy).display().to_string(),
            "run".into(),
            "--resume".into(),
            "--config".into(),
            home.caddy_boot().display().to_string(),
        ],
        env: vec![
            ("XDG_CONFIG_HOME".into(), home.caddy_dir().display().to_string()),
            ("XDG_DATA_HOME".into(), home.caddy_dir().display().to_string()),
        ],
        log: home.logs().join("caddy.log"),
    }
}

/// cloudflared, reading the tunnel token from a private file so it is on
/// neither the command line nor in this plist.
pub fn cloudflared(home: &Home, binary: &Path) -> Agent {
    Agent {
        label: CLOUDFLARED,
        args: vec![
            binary.display().to_string(),
            "tunnel".into(),
            "--no-autoupdate".into(),
            "run".into(),
            "--token-file".into(),
            home.tunnel_token().display().to_string(),
        ],
        env: vec![],
        log: home.logs().join("cloudflared.log"),
    }
}

fn agent_path(label: &str) -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join("Library/LaunchAgents").join(format!("{label}.plist")))
}

/// Write the agent and (re)load it into your GUI session.
pub fn install(agent: &Agent) -> Result<PathBuf> {
    if !cfg!(target_os = "macos") {
        bail!("launchd exists only on macOS; use `henry install --print` to see the agents");
    }
    let path = agent_path(agent.label)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("plist.tmp");
    std::fs::write(&tmp, agent.plist())?;
    std::fs::rename(&tmp, &path)?;

    let domain = gui_domain()?;
    // Not loaded yet is fine; anything else surfaces at bootstrap.
    let _ = Command::new("launchctl").args(["bootout", &format!("{domain}/{}", agent.label)]).status();
    let status = Command::new("launchctl").args(["bootstrap", &domain]).arg(&path).status()?;
    if !status.success() {
        bail!("launchctl bootstrap {domain} {} failed ({status})", path.display());
    }
    Ok(path)
}

pub fn uninstall(label: &str) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("launchd exists only on macOS");
    }
    let domain = gui_domain()?;
    let _ = Command::new("launchctl").args(["bootout", &format!("{domain}/{label}")]).status();
    match std::fs::remove_file(agent_path(label)?) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

fn gui_domain() -> Result<String> {
    let out = Command::new("id").arg("-u").output().context("running `id -u`")?;
    let uid = String::from_utf8(out.stdout)?.trim().to_owned();
    if uid.is_empty() || !uid.chars().all(|c| c.is_ascii_digit()) {
        bail!("`id -u` printed something unexpected");
    }
    Ok(format!("gui/{uid}"))
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> Home {
        Home { root: "/Users/me/Library/Application Support/Henry".into() }
    }

    #[test]
    fn the_henry_agent_runs_henry_and_keeps_it_alive() {
        let p = henry(Path::new("/opt/homebrew/bin/henry"), &home()).plist();
        assert!(p.contains("<string>dev.henry</string>"));
        assert!(p.contains("<string>/opt/homebrew/bin/henry</string>\n        <string>run</string>"));
        assert!(p.contains("<key>KeepAlive</key>\n    <true/>"));
        assert!(p.contains("<string>/Users/me/Library/Application Support/Henry</string>"));
        assert!(p.contains("Henry/logs/henry.log"));
    }

    #[test]
    fn caddy_resumes_and_keeps_its_state_with_henry() {
        let p = caddy(&home(), &Config::default()).plist();
        assert!(p.contains("<string>dev.henry.caddy</string>"));
        assert!(p.contains("<string>--resume</string>"));
        assert!(p.contains("Henry/caddy</string>"), "{p}");
    }

    #[test]
    fn the_tunnel_token_never_appears_in_the_plist() {
        let p = cloudflared(&home(), Path::new("/opt/homebrew/bin/cloudflared")).plist();
        assert!(p.contains("<string>--token-file</string>"));
        assert!(p.contains("tunnel.token</string>"));
        assert!(!p.contains("<string>--token</string>"));
    }

    #[test]
    fn paths_are_escaped() {
        let p = henry(Path::new("/bin/henry"), &Home { root: "/tmp/a&b<c>".into() }).plist();
        assert!(p.contains("/tmp/a&amp;b&lt;c&gt;"));
        assert!(!p.contains("a&b"));
    }
}
