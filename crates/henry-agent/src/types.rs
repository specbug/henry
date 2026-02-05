//! Core types for the agentic task execution system.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Unique identifier for a task.
pub type TaskId = String;

/// Unique identifier for a step.
pub type StepId = String;

/// Current status of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Task is waiting to be processed.
    Queued,
    /// Task is being planned (generating steps).
    Planning,
    /// Task is actively being executed.
    Executing,
    /// Task is waiting for user input.
    AwaitingInput,
    /// Task completed successfully.
    Completed,
    /// Task failed.
    Failed,
    /// Task was cancelled.
    Cancelled,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskStatus::Queued => write!(f, "queued"),
            TaskStatus::Planning => write!(f, "planning"),
            TaskStatus::Executing => write!(f, "executing"),
            TaskStatus::AwaitingInput => write!(f, "awaiting_input"),
            TaskStatus::Completed => write!(f, "completed"),
            TaskStatus::Failed => write!(f, "failed"),
            TaskStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// Priority level for tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPriority {
    Low,
    Normal,
    High,
    Critical,
}

impl Default for TaskPriority {
    fn default() -> Self {
        TaskPriority::Normal
    }
}

impl std::fmt::Display for TaskPriority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskPriority::Low => write!(f, "low"),
            TaskPriority::Normal => write!(f, "normal"),
            TaskPriority::High => write!(f, "high"),
            TaskPriority::Critical => write!(f, "critical"),
        }
    }
}

/// Status of an individual step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// Step is waiting to be executed.
    Pending,
    /// Step is currently running.
    Running,
    /// Step completed successfully.
    Completed,
    /// Step failed.
    Failed,
    /// Step was skipped.
    Skipped,
}

impl std::fmt::Display for StepStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StepStatus::Pending => write!(f, "pending"),
            StepStatus::Running => write!(f, "running"),
            StepStatus::Completed => write!(f, "completed"),
            StepStatus::Failed => write!(f, "failed"),
            StepStatus::Skipped => write!(f, "skipped"),
        }
    }
}

/// Type of step to execute.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StepType {
    /// Execute a Claude Code session in a workspace.
    ClaudeCode {
        /// Working directory for the session.
        workspace: String,
        /// Prompt/instructions for Claude Code.
        prompt: String,
    },
    /// Call the Claude API directly.
    ClaudeApi {
        /// User prompt.
        prompt: String,
        /// Optional system prompt override.
        system: Option<String>,
    },
    /// Execute a shell command.
    Command {
        /// Command to run.
        command: String,
        /// Arguments.
        args: Vec<String>,
        /// Timeout in seconds.
        timeout_secs: u64,
    },
    /// Container operation.
    ContainerOp {
        /// Operation: start, stop, restart, remove.
        operation: String,
        /// Container name or ID.
        container: String,
    },
    /// Request user input.
    UserInput {
        /// Prompt to show the user.
        prompt: String,
    },
}

impl StepType {
    /// Get a description of this step type.
    pub fn description(&self) -> String {
        match self {
            StepType::ClaudeCode { workspace, prompt } => {
                format!("Claude Code in {}: {}", workspace, truncate_str(prompt, 50))
            }
            StepType::ClaudeApi { prompt, .. } => {
                format!("Claude API: {}", truncate_str(prompt, 50))
            }
            StepType::Command { command, args, .. } => {
                format!("{} {}", command, args.join(" "))
            }
            StepType::ContainerOp {
                operation,
                container,
            } => {
                format!("{} container {}", operation, container)
            }
            StepType::UserInput { prompt } => {
                format!("Waiting for input: {}", truncate_str(prompt, 50))
            }
        }
    }
}

/// A single iteration of a step (ReAct pattern).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepIteration {
    /// Iteration number (0-indexed).
    pub iteration: u32,
    /// The agent's reasoning (Thought).
    pub thought: String,
    /// The action taken.
    pub action: String,
    /// The result observed.
    pub observation: String,
    /// Whether this iteration succeeded.
    pub success: bool,
    /// Timestamp of this iteration.
    pub timestamp: DateTime<Utc>,
}

/// A step in a task's execution plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    /// Unique identifier.
    pub id: StepId,
    /// Human-readable description.
    pub description: String,
    /// Type of step and its parameters.
    pub step_type: StepType,
    /// Current status.
    pub status: StepStatus,
    /// IDs of steps that must complete before this one.
    pub depends_on: Vec<StepId>,
    /// Iterations of this step (for ReAct loop).
    pub iterations: Vec<StepIteration>,
    /// Maximum number of iterations allowed.
    pub max_iterations: u32,
    /// Tokens consumed by this step.
    pub tokens_used: u32,
}

impl Step {
    /// Create a new step with the given type.
    pub fn new(id: StepId, description: String, step_type: StepType) -> Self {
        Self {
            id,
            description,
            step_type,
            status: StepStatus::Pending,
            depends_on: Vec::new(),
            iterations: Vec::new(),
            max_iterations: 5,
            tokens_used: 0,
        }
    }

    /// Check if this step has succeeded.
    pub fn succeeded(&self) -> bool {
        self.status == StepStatus::Completed
    }

    /// Check if this step has failed.
    pub fn failed(&self) -> bool {
        self.status == StepStatus::Failed
    }
}

/// Context passed to the agent during task execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskContext {
    /// Additional variables available to the task.
    pub variables: HashMap<String, String>,
    /// User input collected during execution.
    pub user_inputs: HashMap<String, String>,
    /// Files created or modified during execution.
    pub affected_files: Vec<String>,
    /// Errors encountered (for context in retries).
    pub errors: Vec<String>,
}

/// Result of a completed task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    /// Whether the task succeeded overall.
    pub success: bool,
    /// Summary of what was accomplished.
    pub summary: String,
    /// Detailed output/logs.
    pub output: String,
    /// Total tokens consumed.
    pub tokens_used: u32,
    /// Time taken in seconds.
    pub duration_secs: u64,
}

/// A task submitted to the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Unique identifier.
    pub id: TaskId,
    /// Short title.
    pub title: String,
    /// Full description of what to do.
    pub description: String,
    /// When the task was created.
    pub created_at: DateTime<Utc>,
    /// Who/what created the task.
    pub created_by: String,
    /// Priority level.
    pub priority: TaskPriority,
    /// Current status.
    pub status: TaskStatus,
    /// Planned steps (populated during planning phase).
    pub steps: Vec<Step>,
    /// Current step index being executed.
    pub current_step_index: usize,
    /// Execution context.
    pub context: TaskContext,
    /// Final result (when completed).
    pub result: Option<TaskResult>,
    /// Token budget for this task.
    pub token_budget: u32,
    /// Tokens consumed so far.
    pub tokens_used: u32,
}

impl Task {
    /// Create a new task.
    pub fn new(title: String, description: String, created_by: String) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            title,
            description,
            created_at: Utc::now(),
            created_by,
            priority: TaskPriority::default(),
            status: TaskStatus::Queued,
            steps: Vec::new(),
            current_step_index: 0,
            context: TaskContext::default(),
            result: None,
            token_budget: 50_000,
            tokens_used: 0,
        }
    }

    /// Check if the task is in a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        )
    }

    /// Get the current step being executed.
    pub fn current_step(&self) -> Option<&Step> {
        self.steps.get(self.current_step_index)
    }

    /// Get the current step mutably.
    pub fn current_step_mut(&mut self) -> Option<&mut Step> {
        self.steps.get_mut(self.current_step_index)
    }

    /// Check if the token budget has been exceeded.
    pub fn over_budget(&self) -> bool {
        self.tokens_used >= self.token_budget
    }
}

/// State of the agent engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// Agent is idle, waiting for tasks.
    Idle,
    /// Agent is processing a task.
    Processing,
    /// Agent is paused.
    Paused,
}

/// Helper to truncate strings for display.
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}
