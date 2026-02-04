//! Claude/Anthropic AI integration for Henry daemon.
//!
//! Provides both direct Anthropic API access and Claude Code session management
//! with workspace isolation.

use henry_config::ClaudeConfig;
use thiserror::Error;

pub mod api;
pub mod session;
mod types;

pub use api::{AnthropicClient, ApiError};
pub use session::{SessionError, SessionManager};
pub use types::{
    ClaudeStatus, Message, MessageResponse, MessageRole, SessionInfo, SessionStatus,
};

/// Claude module errors.
#[derive(Error, Debug)]
pub enum ClaudeError {
    #[error("API error: {0}")]
    Api(#[from] ApiError),

    #[error("session error: {0}")]
    Session(#[from] SessionError),

    #[error("module not configured")]
    NotConfigured,
}

/// Claude module manager.
///
/// Provides access to both the Anthropic API and Claude Code session management.
pub struct ClaudeManager {
    config: ClaudeConfig,
    api_client: Option<AnthropicClient>,
    session_manager: SessionManager,
}

impl ClaudeManager {
    /// Create a new Claude manager.
    pub fn new(config: ClaudeConfig, api_key: Option<String>) -> Self {
        let api_client = api_key.map(AnthropicClient::new);

        let session_manager = SessionManager::new(
            config.workspace_dir.clone(),
            config.max_sessions,
        );

        Self {
            config,
            api_client,
            session_manager,
        }
    }

    /// Check if the API client is configured.
    pub fn has_api_client(&self) -> bool {
        self.api_client.is_some()
    }

    /// Send a message to Claude via the Anthropic API.
    pub async fn send_message(
        &self,
        messages: &[Message],
        model: Option<&str>,
        max_tokens: Option<u32>,
        system: Option<&str>,
    ) -> Result<MessageResponse, ClaudeError> {
        let client = self.api_client.as_ref().ok_or(ClaudeError::NotConfigured)?;

        let model = model.unwrap_or("claude-sonnet-4-20250514");
        let max_tokens = max_tokens.unwrap_or(4096);

        Ok(client.send_message(messages, model, max_tokens, system).await?)
    }

    /// Send a single user message and get a response.
    pub async fn ask(&self, question: &str) -> Result<String, ClaudeError> {
        let messages = vec![Message::user(question)];
        let response = self.send_message(&messages, None, None, None).await?;
        Ok(response.content)
    }

    /// Check if the Anthropic API is reachable.
    pub async fn ping_api(&self) -> bool {
        match &self.api_client {
            Some(client) => client.ping().await,
            None => false,
        }
    }

    /// Start a new Claude Code session.
    pub async fn new_session(&self, workspace: &str) -> Result<SessionInfo, ClaudeError> {
        Ok(self.session_manager.new_session(workspace).await?)
    }

    /// Stop a Claude Code session.
    pub async fn stop_session(&self, session_id: &str) -> Result<(), ClaudeError> {
        Ok(self.session_manager.stop_session(session_id).await?)
    }

    /// Get information about a session.
    pub async fn get_session(&self, session_id: &str) -> Option<SessionInfo> {
        self.session_manager.get_session(session_id).await
    }

    /// List all sessions.
    pub async fn list_sessions(&self) -> Vec<SessionInfo> {
        self.session_manager.list_sessions().await
    }

    /// List active sessions only.
    pub async fn list_active_sessions(&self) -> Vec<SessionInfo> {
        self.session_manager.list_active_sessions().await
    }

    /// Get the number of active sessions.
    pub async fn active_session_count(&self) -> usize {
        self.session_manager.active_count().await
    }

    /// Refresh session statuses by checking process states.
    pub async fn refresh_sessions(&self) {
        self.session_manager.refresh_sessions().await
    }

    /// Clean up old stopped sessions.
    pub async fn cleanup_sessions(&self, max_age: std::time::Duration) {
        self.session_manager.cleanup_old_sessions(max_age).await
    }

    /// Get the full status of the Claude module.
    pub async fn status(&self) -> ClaudeStatus {
        self.refresh_sessions().await;

        let sessions = self.list_sessions().await;
        let active_sessions = sessions.iter().filter(|s| s.status.is_active()).count();

        ClaudeStatus {
            enabled: self.config.enabled,
            api_key_configured: self.api_client.is_some(),
            active_sessions,
            max_sessions: self.config.max_sessions,
            workspace_dir: self.config.workspace_dir.display().to_string(),
            sessions,
        }
    }

    /// Get the configuration.
    pub fn config(&self) -> &ClaudeConfig {
        &self.config
    }

    /// Get the session manager.
    pub fn session_manager(&self) -> &SessionManager {
        &self.session_manager
    }
}

/// Format a session for display.
pub fn format_session(session: &SessionInfo) -> String {
    let status_indicator = match session.status {
        SessionStatus::Running => "RUN",
        SessionStatus::Starting => "START",
        SessionStatus::Stopping => "STOP",
        SessionStatus::Stopped => "DONE",
        SessionStatus::Error => "ERR",
    };

    let pid_info = session
        .pid
        .map(|p| format!(" (PID {})", p))
        .unwrap_or_default();

    format!(
        "{} {} {}{}",
        status_indicator, session.id, session.workspace, pid_info
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_manager_no_api_key() {
        let config = ClaudeConfig::default();
        let manager = ClaudeManager::new(config, None);
        assert!(!manager.has_api_client());
    }

    #[test]
    fn test_claude_manager_with_api_key() {
        let config = ClaudeConfig::default();
        let manager = ClaudeManager::new(config, Some("test-key".to_string()));
        assert!(manager.has_api_client());
    }

    #[tokio::test]
    async fn test_status_without_api() {
        let config = ClaudeConfig::default();
        let manager = ClaudeManager::new(config.clone(), None);
        let status = manager.status().await;
        assert!(!status.api_key_configured);
        assert_eq!(status.max_sessions, config.max_sessions);
        assert_eq!(status.active_sessions, 0);
    }

    #[test]
    fn test_format_session_running() {
        let mut session = SessionInfo::new("abc123".to_string(), "/tmp/test".to_string());
        session.set_running(12345);
        let formatted = format_session(&session);
        assert!(formatted.contains("RUN"));
        assert!(formatted.contains("abc123"));
        assert!(formatted.contains("PID 12345"));
    }

    #[test]
    fn test_format_session_stopped() {
        let mut session = SessionInfo::new("abc123".to_string(), "/tmp/test".to_string());
        session.set_stopped();
        let formatted = format_session(&session);
        assert!(formatted.contains("DONE"));
        assert!(!formatted.contains("PID"));
    }

    #[test]
    fn test_format_session_error() {
        let mut session = SessionInfo::new("abc123".to_string(), "/tmp/test".to_string());
        session.set_error("Test error".to_string());
        let formatted = format_session(&session);
        assert!(formatted.contains("ERR"));
    }
}
