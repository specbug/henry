//! Single-instance locking for Henry daemon.
//!
//! Uses a combination of port binding and PID file with stale detection.
//! Inspired by OpenClaw's gateway-lock.ts patterns.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, info, warn};

#[derive(Error, Debug)]
pub enum LockError {
    #[error("another instance is already running (port {port} in use)")]
    PortInUse { port: u16 },

    #[error("another instance is already running (PID {pid})")]
    AlreadyRunning { pid: u32 },

    #[error("failed to create PID file: {0}")]
    PidFileCreate(#[source] std::io::Error),

    #[error("failed to read PID file: {0}")]
    PidFileRead(#[source] std::io::Error),

    #[error("failed to bind to port {port}: {source}")]
    BindFailed {
        port: u16,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to create lock directory: {0}")]
    CreateDir(#[source] std::io::Error),
}

/// Instance lock combining port binding and PID file.
pub struct InstanceLock {
    _listener: TcpListener,
    pid_path: PathBuf,
    port: u16,
}

impl InstanceLock {
    /// Acquire an instance lock.
    ///
    /// First checks if the port is available, then creates a PID file.
    /// If a stale PID file exists (process not running), it will be cleaned up.
    pub fn acquire(port: u16, pid_path: impl AsRef<Path>) -> Result<Self, LockError> {
        let pid_path = pid_path.as_ref().to_path_buf();

        // Check for existing instance via port
        if Self::is_port_in_use(port) {
            return Err(LockError::PortInUse { port });
        }

        // Check and clean up stale PID file
        if pid_path.exists() {
            match Self::check_pid_file(&pid_path) {
                Ok(Some(pid)) => {
                    if Self::is_process_running(pid) {
                        return Err(LockError::AlreadyRunning { pid });
                    }
                    warn!("Cleaning up stale PID file (PID {} not running)", pid);
                    let _ = fs::remove_file(&pid_path);
                }
                Ok(None) => {
                    warn!("Removing invalid PID file");
                    let _ = fs::remove_file(&pid_path);
                }
                Err(e) => {
                    warn!("Error reading PID file: {}, removing it", e);
                    let _ = fs::remove_file(&pid_path);
                }
            }
        }

        // Bind to the lock port
        let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| LockError::BindFailed {
            port,
            source: e,
        })?;

        // Create PID file directory if needed
        if let Some(parent) = pid_path.parent() {
            fs::create_dir_all(parent).map_err(LockError::CreateDir)?;
        }

        // Write PID file with restrictive permissions
        Self::write_pid_file(&pid_path)?;

        info!("Instance lock acquired (port {}, PID {})", port, std::process::id());

        Ok(Self {
            _listener: listener,
            pid_path,
            port,
        })
    }

    /// Check if another instance is running without acquiring the lock.
    pub fn check_running(port: u16, pid_path: impl AsRef<Path>) -> Option<u32> {
        let pid_path = pid_path.as_ref();

        // First check port
        if Self::is_port_in_use(port) {
            // Try to get PID from file
            if let Ok(Some(pid)) = Self::check_pid_file(pid_path) {
                return Some(pid);
            }
            // Port in use but no valid PID file - assume another instance
            return Some(0);
        }

        // Port not in use, check PID file
        if let Ok(Some(pid)) = Self::check_pid_file(pid_path) {
            if Self::is_process_running(pid) {
                return Some(pid);
            }
        }

        None
    }

    fn is_port_in_use(port: u16) -> bool {
        TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_millis(100),
        )
        .is_ok()
    }

    fn check_pid_file(path: &Path) -> Result<Option<u32>, LockError> {
        let mut file = File::open(path).map_err(LockError::PidFileRead)?;
        let mut content = String::new();
        file.read_to_string(&mut content)
            .map_err(LockError::PidFileRead)?;

        Ok(content.trim().parse::<u32>().ok())
    }

    fn write_pid_file(path: &Path) -> Result<(), LockError> {
        let pid = std::process::id();

        // Create file with restricted permissions (0o600)
        let mut file = File::create(path).map_err(LockError::PidFileCreate)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
        }

        write!(file, "{}", pid).map_err(LockError::PidFileCreate)?;
        debug!("Wrote PID {} to {}", pid, path.display());

        Ok(())
    }

    #[cfg(unix)]
    fn is_process_running(pid: u32) -> bool {
        // Use kill with signal 0 to check if process exists
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    #[cfg(not(unix))]
    fn is_process_running(_pid: u32) -> bool {
        // On non-Unix, assume running if we can't check
        true
    }

    /// Get the port used for locking.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Get the PID file path.
    pub fn pid_path(&self) -> &Path {
        &self.pid_path
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Clean up PID file on drop
        if let Err(e) = fs::remove_file(&self.pid_path) {
            warn!("Failed to remove PID file: {}", e);
        } else {
            debug!("Removed PID file {}", self.pid_path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn test_lock_acquire_and_release() {
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_path = temp_dir.path().join("test.pid");

        // Find an available port
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let lock = InstanceLock::acquire(port, &pid_path).unwrap();
        assert!(pid_path.exists());

        // Should fail to acquire again
        assert!(matches!(
            InstanceLock::acquire(port, &pid_path),
            Err(LockError::PortInUse { .. })
        ));

        drop(lock);
        assert!(!pid_path.exists());
    }

    #[test]
    fn test_stale_pid_cleanup() {
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_path = temp_dir.path().join("test.pid");

        // Write a fake PID that doesn't exist
        fs::write(&pid_path, "99999999").unwrap();

        // Find an available port
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        // Should succeed after cleaning up stale PID
        let lock = InstanceLock::acquire(port, &pid_path);
        assert!(lock.is_ok());
    }
}
