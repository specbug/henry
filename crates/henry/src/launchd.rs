//! Henry as a LaunchAgent: started at login, restarted if it dies.
//!
//! An agent, not a daemon, on purpose. Podman's VM and the login Keychain
//! both belong to your session, so Henry runs in it. That is why the host
//! needs auto-login (see the concept doc, "Restarts and crashes").

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::home::Home;

pub const LABEL: &str = "dev.henry";

/// Homebrew first: podman, cloudflared, caddy and restic live there.
const PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

pub fn plist(binary: &Path, home: &Home) -> String {
    let log = home.logs().join("henry.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>run</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{PATH}</string>
        <key>HENRY_HOME</key>
        <string>{root}</string>
    </dict>
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
        binary = xml(&binary.display().to_string()),
        root = xml(&home.root.display().to_string()),
        log = xml(&log.display().to_string()),
    )
}

pub fn agent_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

/// Write the agent and (re)load it into your GUI session.
pub fn install(binary: &Path, home: &Home) -> Result<PathBuf> {
    if !cfg!(target_os = "macos") {
        bail!("launchd exists only on macOS; use `henry install --print` to see the agent");
    }
    let path = agent_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("plist.tmp");
    std::fs::write(&tmp, plist(binary, home))?;
    std::fs::rename(&tmp, &path)?;

    let domain = gui_domain()?;
    // Not loaded yet is fine; anything else surfaces at bootstrap.
    let _ = Command::new("launchctl").args(["bootout", &format!("{domain}/{LABEL}")]).status();
    let status = Command::new("launchctl").args(["bootstrap", &domain]).arg(&path).status()?;
    if !status.success() {
        bail!("launchctl bootstrap {domain} {} failed ({status})", path.display());
    }
    Ok(path)
}

pub fn uninstall() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("launchd exists only on macOS");
    }
    let domain = gui_domain()?;
    let _ = Command::new("launchctl").args(["bootout", &format!("{domain}/{LABEL}")]).status();
    let path = agent_path()?;
    match std::fs::remove_file(&path) {
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

    #[test]
    fn the_agent_runs_henry_and_keeps_it_alive() {
        let home = Home { root: "/Users/me/Library/Application Support/Henry".into() };
        let p = plist(Path::new("/opt/homebrew/bin/henry"), &home);
        assert!(p.contains("<string>dev.henry</string>"));
        assert!(p.contains("<string>/opt/homebrew/bin/henry</string>\n        <string>run</string>"));
        assert!(p.contains("<key>KeepAlive</key>\n    <true/>"));
        assert!(p.contains("<string>/Users/me/Library/Application Support/Henry</string>"));
        assert!(p.contains("Henry/logs/henry.log"));
    }

    #[test]
    fn paths_are_escaped() {
        let home = Home { root: "/tmp/a&b<c>".into() };
        let p = plist(Path::new("/bin/henry"), &home);
        assert!(p.contains("/tmp/a&amp;b&lt;c&gt;"));
        assert!(!p.contains("a&b"));
    }
}
