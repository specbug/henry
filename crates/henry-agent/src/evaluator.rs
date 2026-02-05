//! Result evaluator for determining task success/failure.

use crate::types::{Step, StepStatus, Task, TaskResult};
use henry_claude::ClaudeManager;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Evaluates task results and generates summaries.
pub struct ResultEvaluator {
    claude: Arc<RwLock<ClaudeManager>>,
}

impl ResultEvaluator {
    /// Create a new evaluator.
    pub fn new(claude: Arc<RwLock<ClaudeManager>>) -> Self {
        Self { claude }
    }

    /// Evaluate a completed task and generate a result.
    pub async fn evaluate(&self, task: &Task, duration_secs: u64) -> TaskResult {
        let success = self.calculate_success(task);
        let summary = self.generate_summary(task, success).await;
        let output = self.collect_output(task);

        TaskResult {
            success,
            summary,
            output,
            tokens_used: task.tokens_used,
            duration_secs,
        }
    }

    /// Determine if the task succeeded overall.
    fn calculate_success(&self, task: &Task) -> bool {
        // Task succeeded if all steps completed or were skipped
        task.steps.iter().all(|s| {
            matches!(s.status, StepStatus::Completed | StepStatus::Skipped)
        })
    }

    /// Generate a human-readable summary of the task.
    async fn generate_summary(&self, task: &Task, success: bool) -> String {
        let completed_steps = task
            .steps
            .iter()
            .filter(|s| s.status == StepStatus::Completed)
            .count();
        let total_steps = task.steps.len();

        // Try to generate a natural language summary using Claude
        let claude = self.claude.read().await;
        let prompt = format!(
            "Summarize this task result in 1-2 sentences:\n\
             Task: {}\n\
             Description: {}\n\
             Success: {}\n\
             Steps completed: {} of {}\n\
             Steps:\n{}",
            task.title,
            task.description,
            success,
            completed_steps,
            total_steps,
            task.steps
                .iter()
                .map(|s| format!("- {} ({})", s.description, s.status))
                .collect::<Vec<_>>()
                .join("\n")
        );

        match claude.ask(&prompt).await {
            Ok(summary) => summary,
            Err(_) => {
                // Fallback to simple summary
                if success {
                    format!(
                        "Task '{}' completed successfully ({}/{} steps)",
                        task.title, completed_steps, total_steps
                    )
                } else {
                    let failed_steps: Vec<_> = task
                        .steps
                        .iter()
                        .filter(|s| s.status == StepStatus::Failed)
                        .map(|s| s.description.as_str())
                        .collect();
                    format!(
                        "Task '{}' failed. Failed steps: {}",
                        task.title,
                        failed_steps.join(", ")
                    )
                }
            }
        }
    }

    /// Collect detailed output from all steps.
    fn collect_output(&self, task: &Task) -> String {
        let mut output = String::new();

        for step in &task.steps {
            output.push_str(&format!("## Step: {}\n", step.description));
            output.push_str(&format!("Status: {}\n", step.status));

            for iter in &step.iterations {
                output.push_str(&format!(
                    "\n### Iteration {}\n",
                    iter.iteration + 1
                ));
                output.push_str(&format!("Thought: {}\n", iter.thought));
                output.push_str(&format!("Action: {}\n", iter.action));
                output.push_str(&format!(
                    "Observation: {}\n",
                    truncate_str(&iter.observation, 500)
                ));
                output.push_str(&format!("Success: {}\n", iter.success));
            }

            output.push_str("\n---\n\n");
        }

        output
    }

    /// Check if a step should be considered successful based on its output.
    pub fn evaluate_step_output(&self, _step: &Step, output: &str) -> bool {
        // Check for common error patterns
        let error_patterns = [
            "error:",
            "Error:",
            "ERROR:",
            "failed",
            "Failed",
            "FAILED",
            "exception",
            "Exception",
            "panic",
            "Panic",
            "not found",
            "permission denied",
        ];

        // If output contains error patterns and is short, likely a failure
        if output.len() < 500 {
            for pattern in &error_patterns {
                if output.contains(pattern) {
                    return false;
                }
            }
        }

        // Default to success if no obvious errors
        true
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
