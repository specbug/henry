//! Types for Claude integration.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Status of a Claude Code session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    /// Session is starting up.
    Starting,
    /// Session is running and ready.
    Running,
    /// Session is stopping.
    Stopping,
    /// Session has stopped.
    Stopped,
    /// Session encountered an error.
    Error,
}

impl SessionStatus {
    /// Check if the session is active (starting or running).
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }
}

impl std::fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Starting => write!(f, "starting"),
            Self::Running => write!(f, "running"),
            Self::Stopping => write!(f, "stopping"),
            Self::Stopped => write!(f, "stopped"),
            Self::Error => write!(f, "error"),
        }
    }
}

/// Information about a Claude Code session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Unique session identifier.
    pub id: String,
    /// Workspace directory for this session.
    pub workspace: String,
    /// Current status of the session.
    pub status: SessionStatus,
    /// Process ID if running.
    pub pid: Option<u32>,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
    /// When the session was last active.
    pub last_active: DateTime<Utc>,
    /// Error message if status is Error.
    pub error: Option<String>,
}

impl SessionInfo {
    /// Create a new session info.
    pub fn new(id: String, workspace: String) -> Self {
        let now = Utc::now();
        Self {
            id,
            workspace,
            status: SessionStatus::Starting,
            pid: None,
            created_at: now,
            last_active: now,
            error: None,
        }
    }

    /// Update the session to running state with a PID.
    pub fn set_running(&mut self, pid: u32) {
        self.status = SessionStatus::Running;
        self.pid = Some(pid);
        self.last_active = Utc::now();
    }

    /// Update the session to stopped state.
    pub fn set_stopped(&mut self) {
        self.status = SessionStatus::Stopped;
        self.pid = None;
        self.last_active = Utc::now();
    }

    /// Update the session to error state.
    pub fn set_error(&mut self, error: String) {
        self.status = SessionStatus::Error;
        self.pid = None;
        self.error = Some(error);
        self.last_active = Utc::now();
    }
}

/// Overall Claude module status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeStatus {
    /// Whether the module is enabled.
    pub enabled: bool,
    /// Whether the Anthropic API key is configured.
    pub api_key_configured: bool,
    /// Number of active sessions.
    pub active_sessions: usize,
    /// Maximum allowed sessions.
    pub max_sessions: usize,
    /// Workspace directory.
    pub workspace_dir: String,
    /// List of all sessions.
    pub sessions: Vec<SessionInfo>,
}

/// Response from sending a message to the Anthropic API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageResponse {
    /// Response ID.
    pub id: String,
    /// Model used.
    pub model: String,
    /// Response content.
    pub content: String,
    /// Input tokens used.
    pub input_tokens: u32,
    /// Output tokens used.
    pub output_tokens: u32,
    /// Stop reason.
    pub stop_reason: Option<String>,
}

/// Message role for API requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
}

/// A message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
}

impl Message {
    /// Create a user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
        }
    }

    /// Create an assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_status_is_active() {
        assert!(SessionStatus::Starting.is_active());
        assert!(SessionStatus::Running.is_active());
        assert!(!SessionStatus::Stopping.is_active());
        assert!(!SessionStatus::Stopped.is_active());
        assert!(!SessionStatus::Error.is_active());
    }

    #[test]
    fn test_session_info_lifecycle() {
        let mut session = SessionInfo::new("test-id".to_string(), "/tmp/test".to_string());
        assert_eq!(session.status, SessionStatus::Starting);
        assert!(session.pid.is_none());

        session.set_running(12345);
        assert_eq!(session.status, SessionStatus::Running);
        assert_eq!(session.pid, Some(12345));

        session.set_stopped();
        assert_eq!(session.status, SessionStatus::Stopped);
        assert!(session.pid.is_none());
    }

    #[test]
    fn test_session_info_error() {
        let mut session = SessionInfo::new("test-id".to_string(), "/tmp/test".to_string());
        session.set_error("Test error".to_string());
        assert_eq!(session.status, SessionStatus::Error);
        assert_eq!(session.error, Some("Test error".to_string()));
    }

    #[test]
    fn test_message_creation() {
        let user_msg = Message::user("Hello");
        assert_eq!(user_msg.role, MessageRole::User);
        assert_eq!(user_msg.content, "Hello");

        let assistant_msg = Message::assistant("Hi there");
        assert_eq!(assistant_msg.role, MessageRole::Assistant);
    }
}
