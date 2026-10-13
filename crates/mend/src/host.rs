//! The Mac's own settings that an unattended host depends on. Mend reads
//! them and reports drift; changing them needs you (and usually sudo).

use std::process::Stdio;

use tokio::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub name: &'static str,
    /// What Henry needs, in words.
    pub want: &'static str,
    /// What the Mac says.
    pub got: String,
    /// `None` when the answer could not be read.
    pub ok: Option<bool>,
}

/// `fdesetup status`: FileVault must be off, or a reboot waits for a password.
pub fn filevault(out: &str) -> Option<bool> {
    if out.contains("FileVault is Off") {
        Some(true)
    } else if out.contains("FileVault is On") {
        Some(false)
    } else {
        None
    }
}

/// `defaults read …loginwindow autoLoginUser`: someone must log in by themselves.
pub fn autologin(out: Option<&str>) -> Option<bool> {
    Some(out.is_some_and(|u| !u.trim().is_empty()))
}

/// A value from `pmset -g`, e.g. `sleep` or `autorestart`.
pub fn pmset(out: &str, key: &str) -> Option<i64> {
    out.lines().find_map(|l| {
        let mut words = l.split_whitespace();
        (words.next()? == key).then(|| words.next()?.parse().ok())?
    })
}

/// `defaults read …SoftwareUpdate AutomaticallyInstallMacOSUpdates`: macOS
/// must not restart itself into an upgrade.
pub fn auto_install(out: Option<&str>) -> Option<bool> {
    match out.map(str::trim) {
        Some("0") => Some(true),
        Some("1") => Some(false),
        _ => None,
    }
}

async fn read(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).stdin(Stdio::null()).output().await.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Read every check. Off the Mac there is nothing to read.
pub async fn checks() -> Vec<Check> {
    if !cfg!(target_os = "macos") {
        return vec![];
    }
    let fv = read("fdesetup", &["status"]).await.unwrap_or_default();
    let login = read("defaults", &["read", "/Library/Preferences/com.apple.loginwindow", "autoLoginUser"]).await;
    let pm = read("pmset", &["-g"]).await.unwrap_or_default();
    let updates = read(
        "defaults",
        &["read", "/Library/Preferences/com.apple.SoftwareUpdate", "AutomaticallyInstallMacOSUpdates"],
    )
    .await;
    let podman = read("/opt/homebrew/bin/brew", &["outdated", "--quiet", "podman"]).await;
    let sleep = pmset(&pm, "sleep");
    let restart = pmset(&pm, "autorestart");
    vec![
        Check { name: "filevault", want: "off", got: fv.trim().to_owned(), ok: filevault(&fv) },
        Check {
            name: "auto-login",
            want: "a user logs in at boot",
            got: login.clone().map_or("none".into(), |u| u.trim().to_owned()),
            ok: autologin(login.as_deref()),
        },
        Check { name: "sleep", want: "never (sleep 0)", got: format!("{sleep:?}"), ok: sleep.map(|s| s == 0) },
        Check {
            name: "restart after power loss",
            want: "on (autorestart 1)",
            got: format!("{restart:?}"),
            ok: restart.map(|r| r == 1),
        },
        Check {
            name: "macOS auto-install",
            want: "off",
            got: updates.clone().map_or("unset".into(), |u| u.trim().to_owned()),
            ok: auto_install(updates.as_deref()),
        },
        Check {
            name: "podman",
            want: "current (Henry reports; upgrading Podman is yours)",
            got: if podman.as_deref().is_some_and(|p| p.contains("podman")) { "upgrade available" } else { "current" }
                .into(),
            ok: podman.map(|p| !p.contains("podman")),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_answers_are_read_right() {
        assert_eq!(filevault("FileVault is Off.\n"), Some(true));
        assert_eq!(filevault("FileVault is On.\n"), Some(false));
        assert_eq!(filevault(""), None);
        let pm = "System-wide power settings:\nCurrently in use:\n standby              0\n sleep                0 (sleep prevented by coreaudiod)\n autorestart          1\n disksleep            10\n";
        assert_eq!(pmset(pm, "sleep"), Some(0));
        assert_eq!(pmset(pm, "autorestart"), Some(1));
        assert_eq!(pmset(pm, "womp"), None);
        assert_eq!(autologin(Some("rishit\n")), Some(true));
        assert_eq!(autologin(None), Some(false));
        assert_eq!(auto_install(Some("0\n")), Some(true));
        assert_eq!(auto_install(Some("1")), Some(false));
        assert_eq!(auto_install(None), None);
    }
}
