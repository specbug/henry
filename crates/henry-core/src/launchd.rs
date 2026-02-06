//! macOS launchd integration for running Henry as a LaunchAgent.

use std::path::PathBuf;
use thiserror::Error;
use tracing::info;

const PLIST_LABEL: &str = "com.henry.daemon";

#[derive(Error, Debug)]
pub enum LaunchdError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("launchctl error: {0}")]
    Launchctl(String),

    #[error("not supported on this platform")]
    NotSupported,
}

/// Status of the LaunchAgent.
#[derive(Debug, Clone)]
pub enum LaunchdStatus {
    Installed { loaded: bool, pid: Option<u32> },
    NotInstalled,
}

impl std::fmt::Display for LaunchdStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LaunchdStatus::Installed { loaded: true, pid: Some(pid) } => {
                write!(f, "installed and running (PID {})", pid)
            }
            LaunchdStatus::Installed { loaded: true, pid: None } => {
                write!(f, "installed and loaded")
            }
            LaunchdStatus::Installed { loaded: false, .. } => {
                write!(f, "installed but not loaded")
            }
            LaunchdStatus::NotInstalled => write!(f, "not installed"),
        }
    }
}

/// Get the plist file path.
pub fn plist_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{}.plist", PLIST_LABEL))
}

/// Generate the plist XML content.
pub fn generate_plist(
    binary: &std::path::Path,
    config: Option<&std::path::Path>,
    log_dir: &std::path::Path,
) -> String {
    let binary_str = binary.display();
    let stdout_log = log_dir.join("henry.stdout.log");
    let stderr_log = log_dir.join("henry.stderr.log");

    let config_args = if let Some(cfg) = config {
        format!(
            "        <string>--config</string>\n        <string>{}</string>\n",
            cfg.display()
        )
    } else {
        String::new()
    };

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
{config_args}        <string>run</string>
        <string>--headless</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{stdout}</string>
    <key>StandardErrorPath</key>
    <string>{stderr}</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>/usr/local/bin:/usr/bin:/bin:/opt/homebrew/bin</string>
    </dict>
</dict>
</plist>"#,
        label = PLIST_LABEL,
        binary = binary_str,
        config_args = config_args,
        stdout = stdout_log.display(),
        stderr = stderr_log.display(),
    )
}

/// Install Henry as a macOS LaunchAgent.
pub async fn install(
    config: Option<&std::path::Path>,
    log_dir: &std::path::Path,
    force: bool,
) -> Result<(), LaunchdError> {
    let plist = plist_path();

    if plist.exists() && !force {
        return Err(LaunchdError::Launchctl(
            "LaunchAgent already installed. Use --force to reinstall.".to_string(),
        ));
    }

    // Find the henry binary
    let binary = std::env::current_exe().map_err(|e| {
        LaunchdError::Launchctl(format!("Could not determine binary path: {}", e))
    })?;

    // Ensure log directory exists
    std::fs::create_dir_all(log_dir)?;

    // Ensure LaunchAgents directory exists
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Unload existing if present
    if plist.exists() {
        let _ = tokio::process::Command::new("launchctl")
            .args(["unload", "-w", &plist.display().to_string()])
            .output()
            .await;
    }

    // Write plist
    let content = generate_plist(&binary, config, log_dir);
    std::fs::write(&plist, &content)?;

    info!("Wrote LaunchAgent plist to {}", plist.display());

    // Load
    let output = tokio::process::Command::new("launchctl")
        .args(["load", "-w", &plist.display().to_string()])
        .output()
        .await
        .map_err(|e| LaunchdError::Launchctl(format!("Failed to run launchctl: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(LaunchdError::Launchctl(format!(
            "launchctl load failed: {}",
            stderr
        )));
    }

    info!("LaunchAgent loaded successfully");
    Ok(())
}

/// Uninstall the Henry LaunchAgent.
pub async fn uninstall() -> Result<(), LaunchdError> {
    let plist = plist_path();

    if !plist.exists() {
        return Err(LaunchdError::Launchctl(
            "LaunchAgent is not installed".to_string(),
        ));
    }

    // Unload
    let output = tokio::process::Command::new("launchctl")
        .args(["unload", "-w", &plist.display().to_string()])
        .output()
        .await
        .map_err(|e| LaunchdError::Launchctl(format!("Failed to run launchctl: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        info!("Warning: launchctl unload: {}", stderr);
    }

    // Remove plist file
    std::fs::remove_file(&plist)?;

    info!("LaunchAgent uninstalled");
    Ok(())
}

/// Check the status of the LaunchAgent.
pub fn status() -> LaunchdStatus {
    let plist = plist_path();

    if !plist.exists() {
        return LaunchdStatus::NotInstalled;
    }

    // Check if loaded via launchctl list
    let output = std::process::Command::new("launchctl")
        .args(["list", PLIST_LABEL])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            // Parse PID from launchctl list output
            let pid = stdout
                .lines()
                .find(|l| l.contains("PID"))
                .and_then(|l| l.split('=').nth(1))
                .and_then(|s| s.trim().trim_end_matches(';').parse::<u32>().ok());

            LaunchdStatus::Installed {
                loaded: true,
                pid,
            }
        }
        _ => LaunchdStatus::Installed {
            loaded: false,
            pid: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plist_path() {
        let path = plist_path();
        assert!(path.to_string_lossy().contains("LaunchAgents"));
        assert!(path.to_string_lossy().contains(PLIST_LABEL));
    }

    #[test]
    fn test_generate_plist() {
        let binary = std::path::PathBuf::from("/usr/local/bin/henry");
        let log_dir = std::path::PathBuf::from("/tmp/henry-logs");

        let plist = generate_plist(&binary, None, &log_dir);
        assert!(plist.contains(PLIST_LABEL));
        assert!(plist.contains("/usr/local/bin/henry"));
        assert!(plist.contains("--headless"));
        assert!(plist.contains("RunAtLoad"));
        assert!(plist.contains("KeepAlive"));
    }

    #[test]
    fn test_generate_plist_with_config() {
        let binary = std::path::PathBuf::from("/usr/local/bin/henry");
        let config = std::path::PathBuf::from("/home/user/.henry/config.toml");
        let log_dir = std::path::PathBuf::from("/tmp/henry-logs");

        let plist = generate_plist(&binary, Some(&config), &log_dir);
        assert!(plist.contains("--config"));
        assert!(plist.contains("/home/user/.henry/config.toml"));
    }

    #[test]
    fn test_status_display() {
        let s = LaunchdStatus::NotInstalled;
        assert_eq!(format!("{}", s), "not installed");

        let s = LaunchdStatus::Installed { loaded: true, pid: Some(1234) };
        assert!(format!("{}", s).contains("1234"));
    }
}
