//! ReAct executor for task steps.
//!
//! Implements the Thought → Action → Observation loop for executing
//! individual steps with retry and verification.

use crate::types::{Step, StepIteration, StepStatus, StepType, TaskContext};
use chrono::Utc;
use henry_claude::ClaudeManager;
use henry_server::ContainerManager;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

/// Error type for execution operations.
#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error("Step execution failed: {0}")]
    StepFailed(String),
    #[error("Command execution error: {0}")]
    CommandError(String),
    #[error("Claude API error: {0}")]
    ClaudeApi(String),
    #[error("Container operation error: {0}")]
    ContainerError(String),
    #[error("Timeout exceeded")]
    Timeout,
    #[error("User input required")]
    UserInputRequired,
    #[error("Token budget exceeded")]
    TokenBudgetExceeded,
    #[error("Command blocked by safety policy: {0}")]
    CommandBlocked(String),
}

/// Configuration for the executor.
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// Maximum iterations per step.
    pub max_iterations: u32,
    /// Default command timeout.
    pub default_timeout: Duration,
    /// Commands that are never allowed.
    pub command_blocklist: Vec<String>,
    /// Commands that are always allowed (bypass allowlist check).
    pub command_allowlist: Vec<String>,
    /// Require human approval for destructive actions.
    pub require_approval_for_destructive: bool,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            max_iterations: 5,
            default_timeout: Duration::from_secs(60),
            command_blocklist: vec![
                "rm -rf /".to_string(),
                "rm -rf /*".to_string(),
                "sudo rm -rf".to_string(),
                "chmod 777".to_string(),
                ":(){ :|:& };:".to_string(), // fork bomb
            ],
            command_allowlist: vec![
                "ls".to_string(),
                "cat".to_string(),
                "grep".to_string(),
                "find".to_string(),
                "git".to_string(),
                "npm".to_string(),
                "cargo".to_string(),
                "df".to_string(),
                "du".to_string(),
                "ps".to_string(),
                "docker".to_string(),
            ],
            require_approval_for_destructive: true,
        }
    }
}

/// Executes task steps using the ReAct pattern.
pub struct ReActExecutor {
    config: ExecutorConfig,
    claude: Arc<RwLock<ClaudeManager>>,
    containers: Arc<RwLock<ContainerManager>>,
}

impl ReActExecutor {
    /// Create a new executor.
    pub fn new(
        config: ExecutorConfig,
        claude: Arc<RwLock<ClaudeManager>>,
        containers: Arc<RwLock<ContainerManager>>,
    ) -> Self {
        Self {
            config,
            claude,
            containers,
        }
    }

    /// Execute a single step with the ReAct loop.
    pub async fn execute_step(
        &self,
        step: &mut Step,
        context: &mut TaskContext,
    ) -> Result<(), ExecutorError> {
        info!("Executing step: {} - {}", step.id, step.description);

        step.status = StepStatus::Running;
        let max_iterations = step.max_iterations.min(self.config.max_iterations);

        for iteration in 0..max_iterations {
            debug!("Step {} iteration {}/{}", step.id, iteration + 1, max_iterations);

            // THOUGHT: Generate reasoning about current state
            let thought = self.generate_thought(step, context, iteration).await?;

            // ACTION: Execute the step
            let (action, observation, success) = match &step.step_type {
                StepType::Command {
                    command,
                    args,
                    timeout_secs,
                } => {
                    self.execute_command(command, args, Duration::from_secs(*timeout_secs))
                        .await
                }
                StepType::ClaudeCode { workspace, prompt } => {
                    self.execute_claude_code(workspace, prompt, context).await
                }
                StepType::ClaudeApi { prompt, system } => {
                    self.execute_claude_api(prompt, system.as_deref()).await
                }
                StepType::ContainerOp {
                    operation,
                    container,
                } => self.execute_container_op(operation, container).await,
                StepType::UserInput { .. } => {
                    // Cannot execute without user input
                    return Err(ExecutorError::UserInputRequired);
                }
            };

            // Record the iteration
            step.iterations.push(StepIteration {
                iteration,
                thought,
                action,
                observation: observation.clone(),
                success,
                timestamp: Utc::now(),
            });

            if success {
                step.status = StepStatus::Completed;
                info!("Step {} completed successfully", step.id);
                return Ok(());
            }

            // Add error to context for next iteration
            context.errors.push(observation);
            warn!(
                "Step {} iteration {} failed, {} attempts remaining",
                step.id,
                iteration + 1,
                max_iterations - iteration - 1
            );
        }

        // All iterations failed
        step.status = StepStatus::Failed;
        Err(ExecutorError::StepFailed(format!(
            "Step {} failed after {} iterations",
            step.id, max_iterations
        )))
    }

    /// Generate a thought/reasoning about the current state.
    async fn generate_thought(
        &self,
        step: &Step,
        context: &TaskContext,
        iteration: u32,
    ) -> Result<String, ExecutorError> {
        if iteration == 0 {
            // First iteration: just describe what we're about to do
            return Ok(format!("Attempting: {}", step.description));
        }

        // Subsequent iterations: analyze previous failures
        let last_error = context.errors.last().map(|e| e.as_str()).unwrap_or("unknown");
        let claude = self.claude.read().await;

        let prompt = format!(
            "The previous attempt at '{}' failed with: {}\n\
             This is attempt {} of {}. What should we try differently?",
            step.description,
            last_error,
            iteration + 1,
            step.max_iterations
        );

        match claude.ask(&prompt).await {
            Ok(response) => Ok(response),
            Err(e) => {
                warn!("Failed to generate thought: {}", e);
                Ok(format!("Retrying after error: {}", last_error))
            }
        }
    }

    /// Execute a shell command.
    async fn execute_command(
        &self,
        command: &str,
        args: &[String],
        timeout_duration: Duration,
    ) -> (String, String, bool) {
        let full_cmd = format!("{} {}", command, args.join(" "));

        // Safety check: blocklist
        for blocked in &self.config.command_blocklist {
            if full_cmd.contains(blocked) {
                return (
                    full_cmd.clone(),
                    format!("Command blocked by safety policy: contains '{}'", blocked),
                    false,
                );
            }
        }

        // Execute command
        let result = timeout(timeout_duration, async {
            let mut child = Command::new(command)
                .args(args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;

            let output = child.wait_with_output().await?;
            Ok::<_, std::io::Error>((output.status, output.stdout, output.stderr))
        })
        .await;

        match result {
            Ok(Ok((status, stdout, stderr))) => {
                let stdout_str = String::from_utf8_lossy(&stdout);
                let stderr_str = String::from_utf8_lossy(&stderr);

                let output = if stderr_str.is_empty() {
                    stdout_str.to_string()
                } else {
                    format!("stdout: {}\nstderr: {}", stdout_str, stderr_str)
                };

                (full_cmd, output, status.success())
            }
            Ok(Err(e)) => (full_cmd.clone(), format!("Failed to execute: {}", e), false),
            Err(_) => (full_cmd.clone(), "Command timed out".to_string(), false),
        }
    }

    /// Execute a Claude Code session.
    async fn execute_claude_code(
        &self,
        workspace: &str,
        prompt: &str,
        context: &TaskContext,
    ) -> (String, String, bool) {
        // For now, we'll simulate this with a Claude API call
        // In a real implementation, this would spawn a Claude Code subprocess
        let action = format!("Claude Code in {}: {}", workspace, truncate_str(prompt, 50));

        let claude = self.claude.read().await;
        let system = format!(
            "You are working in directory: {}\n\
             Available context: {:?}\n\
             Provide a clear, actionable response.",
            workspace, context.variables
        );

        match claude.ask(&format!("{}\n\n{}", system, prompt)).await {
            Ok(response) => (action, response, true),
            Err(e) => (action, format!("Claude Code error: {}", e), false),
        }
    }

    /// Execute a Claude API call.
    async fn execute_claude_api(
        &self,
        prompt: &str,
        system: Option<&str>,
    ) -> (String, String, bool) {
        let action = format!("Claude API: {}", truncate_str(prompt, 50));
        let system_prompt = system.unwrap_or("You are a helpful assistant.");

        let claude = self.claude.read().await;
        match claude.ask(&format!("{}\n\n{}", system_prompt, prompt)).await {
            Ok(response) => (action, response, true),
            Err(e) => (action, format!("Claude API error: {}", e), false),
        }
    }

    /// Execute a container operation.
    async fn execute_container_op(
        &self,
        operation: &str,
        container: &str,
    ) -> (String, String, bool) {
        let action = format!("{} container {}", operation, container);

        let containers = self.containers.read().await;

        let result = match operation {
            "start" => containers.start(container).await,
            "stop" => containers.stop(container).await,
            "restart" => containers.restart(container).await,
            _ => {
                return (
                    action,
                    format!("Unknown operation: {}", operation),
                    false,
                );
            }
        };

        match result {
            Ok(_) => (action, format!("Container {} {}ed successfully", container, operation), true),
            Err(e) => (action, format!("Container operation failed: {}", e), false),
        }
    }
}

/// Helper to truncate strings for display.
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}
