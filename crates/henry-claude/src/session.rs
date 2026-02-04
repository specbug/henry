//! Claude Code session management.
//!
//! Manages Claude Code CLI process spawning with workspace isolation.

use crate::types::{SessionInfo, SessionStatus};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use thiserror::Error;
use tokio::process::{Child, Command};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Session manager errors.
#[derive(Error, Debug)]
pub enum SessionError {
    #[error("workspace does not exist: {0}")]
    WorkspaceNotFound(String),

    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("max sessions reached: {0}")]
    MaxSessionsReached(usize),

    #[error("session already exists for workspace: {0}")]
    SessionExists(String),

    #[error("failed to spawn process: {0}")]
    SpawnFailed(#[source] std::io::Error),

    #[error("process error: {0}")]
    ProcessError(String),

    #[error("workspace directory error: {0}")]
    WorkspaceError(#[source] std::io::Error),
}

/// Manages Claude Code sessions.
pub struct SessionManager {
    /// Base workspace directory.
    workspace_base: PathBuf,
    /// Maximum concurrent sessions.
    max_sessions: usize,
    /// Active sessions by ID.
    sessions: RwLock<HashMap<String, SessionEntry>>,
}

/// Internal session entry with process handle.
struct SessionEntry {
    info: SessionInfo,
    process: Option<Child>,
}

impl SessionManager {
    /// Create a new session manager.
    pub fn new(workspace_base: PathBuf, max_sessions: usize) -> Self {
        Self {
            workspace_base,
            max_sessions,
            sessions: RwLock::new(HashMap::new()),
        }
    }

    /// Start a new Claude Code session in the specified workspace.
    ///
    /// If `workspace` is a relative path, it's resolved relative to the base workspace directory.
    /// If it's an absolute path, it's used directly.
    pub async fn new_session(&self, workspace: &str) -> Result<SessionInfo, SessionError> {
        // Resolve workspace path
        let workspace_path = self.resolve_workspace(workspace)?;
        let workspace_str = workspace_path.display().to_string();

        // Check max sessions
        let sessions = self.sessions.read().await;
        let active_count = sessions.values().filter(|e| e.info.status.is_active()).count();
        if active_count >= self.max_sessions {
            return Err(SessionError::MaxSessionsReached(self.max_sessions));
        }

        // Check if session already exists for this workspace
        for entry in sessions.values() {
            if entry.info.workspace == workspace_str && entry.info.status.is_active() {
                return Err(SessionError::SessionExists(workspace_str));
            }
        }
        drop(sessions);

        // Generate session ID
        let session_id = Uuid::new_v4().to_string()[..8].to_string();
        let mut session_info = SessionInfo::new(session_id.clone(), workspace_str.clone());

        // Spawn Claude Code process
        info!("Starting Claude Code session {} in {}", session_id, workspace_str);

        let child = match Self::spawn_claude_code(&workspace_path).await {
            Ok(child) => child,
            Err(e) => {
                error!("Failed to spawn Claude Code: {}", e);
                session_info.set_error(e.to_string());
                // Store the failed session for tracking
                let mut sessions = self.sessions.write().await;
                sessions.insert(
                    session_id.clone(),
                    SessionEntry {
                        info: session_info.clone(),
                        process: None,
                    },
                );
                return Err(SessionError::SpawnFailed(e));
            }
        };

        // Get PID and update session info
        let pid = child.id().unwrap_or(0);
        session_info.set_running(pid);

        // Store session
        let mut sessions = self.sessions.write().await;
        sessions.insert(
            session_id.clone(),
            SessionEntry {
                info: session_info.clone(),
                process: Some(child),
            },
        );

        info!("Claude Code session {} started with PID {}", session_id, pid);
        Ok(session_info)
    }

    /// Stop a session by ID.
    pub async fn stop_session(&self, session_id: &str) -> Result<(), SessionError> {
        let mut sessions = self.sessions.write().await;

        let entry = sessions
            .get_mut(session_id)
            .ok_or_else(|| SessionError::SessionNotFound(session_id.to_string()))?;

        if !entry.info.status.is_active() {
            debug!("Session {} is already stopped", session_id);
            return Ok(());
        }

        entry.info.status = SessionStatus::Stopping;

        if let Some(ref mut child) = entry.process {
            info!("Stopping Claude Code session {}", session_id);

            // Try graceful shutdown first (SIGTERM)
            if let Err(e) = child.kill().await {
                warn!("Failed to kill session {} process: {}", session_id, e);
            }

            // Wait for process to exit
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                child.wait(),
            )
            .await
            {
                Ok(Ok(status)) => {
                    debug!("Session {} exited with status: {:?}", session_id, status);
                }
                Ok(Err(e)) => {
                    warn!("Error waiting for session {} to exit: {}", session_id, e);
                }
                Err(_) => {
                    warn!("Timeout waiting for session {} to exit", session_id);
                }
            }
        }

        entry.info.set_stopped();
        entry.process = None;

        info!("Claude Code session {} stopped", session_id);
        Ok(())
    }

    /// Get information about a session.
    pub async fn get_session(&self, session_id: &str) -> Option<SessionInfo> {
        let sessions = self.sessions.read().await;
        sessions.get(session_id).map(|e| e.info.clone())
    }

    /// List all sessions.
    pub async fn list_sessions(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.read().await;
        sessions.values().map(|e| e.info.clone()).collect()
    }

    /// List only active sessions.
    pub async fn list_active_sessions(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.read().await;
        sessions
            .values()
            .filter(|e| e.info.status.is_active())
            .map(|e| e.info.clone())
            .collect()
    }

    /// Get the count of active sessions.
    pub async fn active_count(&self) -> usize {
        let sessions = self.sessions.read().await;
        sessions.values().filter(|e| e.info.status.is_active()).count()
    }

    /// Clean up stopped sessions older than the specified duration.
    pub async fn cleanup_old_sessions(&self, max_age: std::time::Duration) {
        let mut sessions = self.sessions.write().await;
        let now = chrono::Utc::now();
        let cutoff = now - chrono::Duration::from_std(max_age).unwrap_or_default();

        sessions.retain(|id, entry| {
            if !entry.info.status.is_active() && entry.info.last_active < cutoff {
                debug!("Cleaning up old session: {}", id);
                false
            } else {
                true
            }
        });
    }

    /// Check and update the status of all sessions.
    pub async fn refresh_sessions(&self) {
        let mut sessions = self.sessions.write().await;

        for (id, entry) in sessions.iter_mut() {
            if let Some(ref mut child) = entry.process {
                // Check if process is still running
                match child.try_wait() {
                    Ok(Some(status)) => {
                        // Process has exited
                        if status.success() {
                            entry.info.set_stopped();
                        } else {
                            entry.info.set_error(format!("Process exited with status: {:?}", status));
                        }
                        debug!("Session {} process exited: {:?}", id, status);
                    }
                    Ok(None) => {
                        // Process still running
                    }
                    Err(e) => {
                        warn!("Error checking session {} status: {}", id, e);
                    }
                }
            }
        }
    }

    /// Resolve a workspace path.
    fn resolve_workspace(&self, workspace: &str) -> Result<PathBuf, SessionError> {
        let path = Path::new(workspace);
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace_base.join(workspace)
        };

        // Create directory if it doesn't exist
        if !resolved.exists() {
            std::fs::create_dir_all(&resolved).map_err(SessionError::WorkspaceError)?;
            info!("Created workspace directory: {}", resolved.display());
        }

        if !resolved.is_dir() {
            return Err(SessionError::WorkspaceNotFound(resolved.display().to_string()));
        }

        Ok(resolved)
    }

    /// Spawn a Claude Code process.
    async fn spawn_claude_code(workspace: &Path) -> Result<Child, std::io::Error> {
        // Look for claude binary in common locations
        let claude_bin = Self::find_claude_binary()?;

        Command::new(&claude_bin)
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
    }

    /// Find the claude binary.
    fn find_claude_binary() -> Result<PathBuf, std::io::Error> {
        // Check common locations
        let candidates = [
            // User-installed via npm
            dirs::home_dir()
                .map(|h| h.join(".local/bin/claude"))
                .unwrap_or_default(),
            dirs::home_dir()
                .map(|h| h.join("Library/pnpm/claude"))
                .unwrap_or_default(),
            // Homebrew
            PathBuf::from("/opt/homebrew/bin/claude"),
            PathBuf::from("/usr/local/bin/claude"),
            // npm global
            PathBuf::from("/usr/bin/claude"),
        ];

        for candidate in &candidates {
            if candidate.exists() {
                return Ok(candidate.clone());
            }
        }

        // Fall back to PATH lookup
        which::which("claude").map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("claude binary not found: {}", e),
            )
        })
    }

    /// Get the workspace base directory.
    pub fn workspace_base(&self) -> &Path {
        &self.workspace_base
    }

    /// Get the max sessions limit.
    pub fn max_sessions(&self) -> usize {
        self.max_sessions
    }
}

// Home directory helper
mod dirs {
    use std::path::PathBuf;

    pub fn home_dir() -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;

    #[test]
    fn test_session_manager_creation() {
        let base = temp_dir().join("claude-test");
        let manager = SessionManager::new(base.clone(), 4);
        assert_eq!(manager.max_sessions(), 4);
        assert_eq!(manager.workspace_base(), &base);
    }

    #[tokio::test]
    async fn test_list_empty_sessions() {
        let base = temp_dir().join("claude-test-empty");
        let manager = SessionManager::new(base, 4);
        let sessions = manager.list_sessions().await;
        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn test_active_count_empty() {
        let base = temp_dir().join("claude-test-count");
        let manager = SessionManager::new(base, 4);
        assert_eq!(manager.active_count().await, 0);
    }

    #[test]
    fn test_resolve_workspace_relative() {
        let base = temp_dir().join("claude-workspaces");
        let manager = SessionManager::new(base.clone(), 4);
        let resolved = manager.resolve_workspace("test-project").unwrap();
        assert_eq!(resolved, base.join("test-project"));
    }

    #[test]
    fn test_resolve_workspace_absolute() {
        let base = temp_dir().join("claude-workspaces");
        let manager = SessionManager::new(base, 4);
        let absolute = temp_dir().join("other-project");
        let resolved = manager.resolve_workspace(&absolute.display().to_string()).unwrap();
        assert_eq!(resolved, absolute);
    }

    #[tokio::test]
    async fn test_get_nonexistent_session() {
        let base = temp_dir().join("claude-test-get");
        let manager = SessionManager::new(base, 4);
        let session = manager.get_session("nonexistent").await;
        assert!(session.is_none());
    }

    #[tokio::test]
    async fn test_stop_nonexistent_session() {
        let base = temp_dir().join("claude-test-stop");
        let manager = SessionManager::new(base, 4);
        let result = manager.stop_session("nonexistent").await;
        assert!(matches!(result, Err(SessionError::SessionNotFound(_))));
    }
}
