//! Task planner that generates execution steps using Claude API.

use crate::types::{Step, StepType, Task};
use henry_claude::ClaudeManager;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Error type for planning operations.
#[derive(Debug, thiserror::Error)]
pub enum PlannerError {
    #[error("Claude API error: {0}")]
    ClaudeApi(String),
    #[error("Failed to parse plan: {0}")]
    ParseError(String),
    #[error("Task is not in a plannable state")]
    InvalidState,
}

/// Generates execution plans for tasks.
pub struct TaskPlanner {
    claude: Arc<RwLock<ClaudeManager>>,
    max_steps: usize,
    allowed_step_types: Vec<String>,
}

impl TaskPlanner {
    /// Create a new task planner.
    pub fn new(claude: Arc<RwLock<ClaudeManager>>) -> Self {
        Self {
            claude,
            max_steps: 10,
            allowed_step_types: vec![
                "command".to_string(),
                "claude_code".to_string(),
                "claude_api".to_string(),
                "container_op".to_string(),
                "user_input".to_string(),
            ],
        }
    }

    /// Set the maximum number of steps allowed.
    pub fn with_max_steps(mut self, max: usize) -> Self {
        self.max_steps = max;
        self
    }

    /// Generate a plan for the given task.
    pub async fn plan(&self, task: &Task) -> Result<Vec<Step>, PlannerError> {
        info!("Planning task: {}", task.title);

        let system_prompt = self.build_system_prompt();
        let user_prompt = self.build_user_prompt(task);

        // Call Claude API to generate plan
        let claude = self.claude.read().await;
        let full_prompt = format!("{}\n\n{}", system_prompt, user_prompt);
        let response = claude
            .ask(&full_prompt)
            .await
            .map_err(|e| PlannerError::ClaudeApi(e.to_string()))?;

        // Parse the response into steps
        let steps = self.parse_plan_response(&response)?;

        if steps.len() > self.max_steps {
            warn!(
                "Plan has {} steps, truncating to {}",
                steps.len(),
                self.max_steps
            );
            return Ok(steps.into_iter().take(self.max_steps).collect());
        }

        info!("Generated {} steps for task {}", steps.len(), task.id);
        Ok(steps)
    }

    /// Build the system prompt for planning.
    fn build_system_prompt(&self) -> String {
        format!(
            r#"You are a task planning assistant for Henry, a home server management daemon.
Your job is to break down user requests into concrete, executable steps.

Available step types:
- command: Execute a shell command (safe commands only)
- claude_code: Use Claude Code to write/modify code in a workspace
- claude_api: Use Claude API for analysis/generation tasks
- container_op: Docker container operation (start, stop, restart)
- user_input: Request input from the user

Output your plan as a JSON array of steps. Each step should have:
- id: Unique string identifier (step_1, step_2, etc.)
- description: Human-readable description
- step_type: Object with "type" field and relevant parameters
- depends_on: Array of step IDs that must complete first (optional)

Example:
```json
[
  {{
    "id": "step_1",
    "description": "Check current disk usage",
    "step_type": {{ "type": "command", "command": "df", "args": ["-h"], "timeout_secs": 30 }},
    "depends_on": []
  }},
  {{
    "id": "step_2",
    "description": "Clean up old logs if disk is low",
    "step_type": {{ "type": "command", "command": "find", "args": ["/var/log", "-name", "*.log.gz", "-mtime", "+30", "-delete"], "timeout_secs": 60 }},
    "depends_on": ["step_1"]
  }}
]
```

Safety rules:
- Never use rm -rf on system directories
- Never modify system files without user_input confirmation
- Keep timeout_secs reasonable (max 300 for most commands)
- Prefer non-destructive operations
- Maximum {} steps per task

Only output the JSON array, no other text."#,
            self.max_steps
        )
    }

    /// Build the user prompt from the task.
    fn build_user_prompt(&self, task: &Task) -> String {
        let context_info = if !task.context.variables.is_empty() {
            format!(
                "\n\nContext variables:\n{}",
                task.context
                    .variables
                    .iter()
                    .map(|(k, v)| format!("- {}: {}", k, v))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        } else {
            String::new()
        };

        format!(
            "Task: {}\n\nDescription: {}{}",
            task.title, task.description, context_info
        )
    }

    /// Parse the Claude response into steps.
    fn parse_plan_response(&self, response: &str) -> Result<Vec<Step>, PlannerError> {
        // Extract JSON from response (may be wrapped in markdown code blocks)
        let json_str = extract_json_array(response)
            .ok_or_else(|| PlannerError::ParseError("No JSON array found in response".to_string()))?;

        // Parse the JSON
        let raw_steps: Vec<RawStep> = serde_json::from_str(&json_str)
            .map_err(|e| PlannerError::ParseError(format!("JSON parse error: {}", e)))?;

        // Convert to our Step type
        let steps: Vec<Step> = raw_steps
            .into_iter()
            .filter_map(|raw| self.convert_raw_step(raw))
            .collect();

        if steps.is_empty() {
            return Err(PlannerError::ParseError("No valid steps in plan".to_string()));
        }

        Ok(steps)
    }

    /// Convert a raw parsed step into our Step type.
    fn convert_raw_step(&self, raw: RawStep) -> Option<Step> {
        let step_type = match raw.step_type.r#type.as_str() {
            "command" => StepType::Command {
                command: raw.step_type.command.unwrap_or_default(),
                args: raw.step_type.args.unwrap_or_default(),
                timeout_secs: raw.step_type.timeout_secs.unwrap_or(60),
            },
            "claude_code" => StepType::ClaudeCode {
                workspace: raw.step_type.workspace.unwrap_or_else(|| ".".to_string()),
                prompt: raw.step_type.prompt.unwrap_or_default(),
            },
            "claude_api" => StepType::ClaudeApi {
                prompt: raw.step_type.prompt.unwrap_or_default(),
                system: raw.step_type.system,
            },
            "container_op" => StepType::ContainerOp {
                operation: raw.step_type.operation.unwrap_or_default(),
                container: raw.step_type.container.unwrap_or_default(),
            },
            "user_input" => StepType::UserInput {
                prompt: raw.step_type.prompt.unwrap_or_default(),
            },
            unknown => {
                warn!("Unknown step type: {}", unknown);
                return None;
            }
        };

        let mut step = Step::new(raw.id, raw.description, step_type);
        step.depends_on = raw.depends_on;
        Some(step)
    }
}

/// Raw step structure from JSON parsing.
#[derive(Debug, serde::Deserialize)]
struct RawStep {
    id: String,
    description: String,
    step_type: RawStepType,
    #[serde(default)]
    depends_on: Vec<String>,
}

/// Raw step type from JSON parsing.
#[derive(Debug, serde::Deserialize)]
struct RawStepType {
    r#type: String,
    // Command fields
    command: Option<String>,
    args: Option<Vec<String>>,
    timeout_secs: Option<u64>,
    // Claude fields
    workspace: Option<String>,
    prompt: Option<String>,
    system: Option<String>,
    // Container fields
    operation: Option<String>,
    container: Option<String>,
}

/// Extract a JSON array from text (handles markdown code blocks).
fn extract_json_array(text: &str) -> Option<String> {
    // Try to find JSON in code blocks first
    if let Some(start) = text.find("```json") {
        let after_marker = &text[start + 7..];
        if let Some(end) = after_marker.find("```") {
            return Some(after_marker[..end].trim().to_string());
        }
    }

    // Try plain code blocks
    if let Some(start) = text.find("```") {
        let after_marker = &text[start + 3..];
        if let Some(end) = after_marker.find("```") {
            let content = after_marker[..end].trim();
            if content.starts_with('[') {
                return Some(content.to_string());
            }
        }
    }

    // Try to find raw JSON array
    if let Some(start) = text.find('[') {
        if let Some(end) = text.rfind(']') {
            return Some(text[start..=end].to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_json_array() {
        let input = r#"Here's the plan:
```json
[{"id": "step_1", "description": "test"}]
```
Done!"#;
        let result = extract_json_array(input);
        assert!(result.is_some());
        assert!(result.unwrap().contains("step_1"));
    }

    #[test]
    fn test_extract_raw_json() {
        let input = r#"[{"id": "step_1"}]"#;
        let result = extract_json_array(input);
        assert!(result.is_some());
    }
}
