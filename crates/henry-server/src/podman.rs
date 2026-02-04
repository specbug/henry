//! Podman-specific socket detection for macOS.

use std::path::PathBuf;
use tracing::debug;

/// Detect the Podman socket path on macOS.
/// Tries multiple known locations in order of preference.
pub fn detect_podman_socket() -> Option<String> {
    let candidates = get_socket_candidates();

    for path in candidates {
        if path.exists() {
            debug!("Found Podman socket at: {}", path.display());
            return Some(format!("unix://{}", path.display()));
        }
    }

    debug!("No Podman socket found");
    None
}

/// Detect the Docker socket path (also works for Podman with Docker compat).
pub fn detect_docker_socket() -> Option<String> {
    let candidates = [
        PathBuf::from("/var/run/docker.sock"),
        PathBuf::from("/run/docker.sock"),
    ];

    for path in candidates {
        if path.exists() {
            debug!("Found Docker socket at: {}", path.display());
            return Some(format!("unix://{}", path.display()));
        }
    }

    debug!("No Docker socket found");
    None
}

/// Get all candidate socket paths for Podman on macOS.
fn get_socket_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    // XDG_RUNTIME_DIR based path (Linux standard)
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        candidates.push(PathBuf::from(&xdg).join("podman/podman.sock"));
    }

    // User ID based path (Linux)
    if let Some(uid) = get_uid() {
        candidates.push(PathBuf::from(format!("/run/user/{}/podman/podman.sock", uid)));
    }

    // macOS Podman machine socket
    if let Some(home) = home_dir() {
        // Podman machine socket (macOS)
        candidates.push(home.join(".local/share/containers/podman/machine/podman.sock"));
        candidates.push(home.join(".local/share/containers/podman/machine/qemu/podman.sock"));

        // Podman Desktop socket location
        candidates.push(home.join(".local/share/containers/podman/machine/podman-machine-default/podman.sock"));
    }

    // Docker socket (Podman with docker compat or podman-mac-helper)
    candidates.push(PathBuf::from("/var/run/docker.sock"));

    // Colima socket (Docker-compatible on macOS)
    if let Some(home) = home_dir() {
        candidates.push(home.join(".colima/default/docker.sock"));
        candidates.push(home.join(".colima/docker.sock"));
    }

    candidates
}

/// Get current user ID.
#[cfg(unix)]
fn get_uid() -> Option<u32> {
    Some(unsafe { libc::getuid() })
}

#[cfg(not(unix))]
fn get_uid() -> Option<u32> {
    None
}

/// Get home directory.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_socket_candidates() {
        let candidates = get_socket_candidates();
        // Should at least have the Docker socket path
        assert!(candidates.iter().any(|p| p.to_string_lossy().contains("docker.sock")));
    }

    #[cfg(unix)]
    #[test]
    fn test_get_uid() {
        // Should return a valid UID on Unix
        assert!(get_uid().is_some());
    }
}
