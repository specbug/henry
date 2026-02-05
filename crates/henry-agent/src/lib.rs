//! Agentic task execution for Henry daemon.
//!
//! This crate provides the AgentEngine which accepts high-level tasks,
//! plans execution steps, and executes them using the ReAct pattern
//! (Thought → Action → Observation).

pub mod evaluator;
pub mod executor;
pub mod planner;
pub mod task;
pub mod types;

use evaluator::ResultEvaluator;
use executor::{ExecutorConfig, ReActExecutor};
use henry_claude::ClaudeManager;
use henry_config::AgentConfig;
use henry_server::ContainerManager;
use henry_state::StateManager;
use planner::TaskPlanner;
use std::sync::Arc;
use std::time::Instant;
use task::{TaskQueue, TaskQueueError};
use tokio::sync::{broadcast, RwLock};
use tokio::time::{interval, Duration};
use tracing::{error, info, warn};
use types::{AgentState, Task, TaskId, TaskPriority, TaskStatus};

pub use types::{TaskContext, TaskResult};

/// Error type for agent operations.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("Task queue error: {0}")]
    TaskQueue(#[from] TaskQueueError),
    #[error("Planning error: {0}")]
    Planning(#[from] planner::PlannerError),
    #[error("Execution error: {0}")]
    Execution(#[from] executor::ExecutorError),
    #[error("Agent is not running")]
    NotRunning,
    #[error("Task not found: {0}")]
    TaskNotFound(TaskId),
}

/// The main agent engine that coordinates task execution.
pub struct AgentEngine {
    config: AgentConfig,
    state: AgentState,
    queue: TaskQueue,
    planner: TaskPlanner,
    executor: ReActExecutor,
    evaluator: ResultEvaluator,
    shutdown_rx: broadcast::Receiver<()>,
    /// Callback for notifications (task progress, completion, etc.)
    notify_callback: Option<Box<dyn Fn(AgentNotification) + Send + Sync>>,
}

/// Notification types sent by the agent.
#[derive(Debug, Clone)]
pub enum AgentNotification {
    /// Task submitted to queue.
    TaskSubmitted { task_id: TaskId, title: String },
    /// Task planning started.
    TaskPlanning { task_id: TaskId },
    /// Task execution started.
    TaskStarted { task_id: TaskId, steps: usize },
    /// Step completed.
    StepCompleted {
        task_id: TaskId,
        step_index: usize,
        total_steps: usize,
        description: String,
    },
    /// Step failed (will retry).
    StepRetrying {
        task_id: TaskId,
        step_index: usize,
        attempt: u32,
        error: String,
    },
    /// Task requires user input.
    UserInputRequired {
        task_id: TaskId,
        prompt: String,
    },
    /// Task completed successfully.
    TaskCompleted {
        task_id: TaskId,
        summary: String,
    },
    /// Task failed.
    TaskFailed {
        task_id: TaskId,
        error: String,
    },
}

impl AgentEngine {
    /// Create a new agent engine.
    pub fn new(
        config: AgentConfig,
        state: Arc<RwLock<StateManager>>,
        claude: Arc<RwLock<ClaudeManager>>,
        containers: Arc<RwLock<ContainerManager>>,
        shutdown_rx: broadcast::Receiver<()>,
    ) -> Self {
        let queue = TaskQueue::new(state.clone());
        let planner = TaskPlanner::new(claude.clone())
            .with_max_steps(config.max_steps_per_task);

        let executor_config = ExecutorConfig {
            max_iterations: 5,
            command_allowlist: config.safety.command_allowlist.clone(),
            command_blocklist: config.safety.command_blocklist.clone(),
            ..Default::default()
        };
        let executor = ReActExecutor::new(executor_config, claude.clone(), containers);

        let evaluator = ResultEvaluator::new(claude);

        Self {
            config,
            state: AgentState::Idle,
            queue,
            planner,
            executor,
            evaluator,
            shutdown_rx,
            notify_callback: None,
        }
    }

    /// Set the notification callback.
    pub fn set_notify_callback<F>(&mut self, callback: F)
    where
        F: Fn(AgentNotification) + Send + Sync + 'static,
    {
        self.notify_callback = Some(Box::new(callback));
    }

    /// Send a notification.
    fn notify(&self, notification: AgentNotification) {
        if let Some(ref callback) = self.notify_callback {
            callback(notification);
        }
    }

    /// Start the agent engine's main loop.
    pub async fn start(&mut self) -> Result<(), AgentError> {
        info!("Starting agent engine");

        // Load pending tasks from database
        self.queue.load_from_db().await?;

        let mut check_interval = interval(Duration::from_secs(5));

        loop {
            tokio::select! {
                _ = check_interval.tick() => {
                    if self.state == AgentState::Paused {
                        continue;
                    }

                    // Check for and process tasks
                    if let Err(e) = self.process_next_task().await {
                        error!("Error processing task: {}", e);
                    }
                }
                _ = self.shutdown_rx.recv() => {
                    info!("Agent engine shutting down");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Process the next available task.
    async fn process_next_task(&mut self) -> Result<(), AgentError> {
        // Get next queued task - clone data we need
        let (task_id, task_title, task_for_planning) = {
            let task = match self.queue.next() {
                Some(t) => t,
                None => return Ok(()), // No tasks to process
            };
            (task.id.clone(), task.title.clone(), task.clone())
        };

        info!("Processing task: {} - {}", task_id, task_title);
        self.state = AgentState::Processing;

        let start_time = Instant::now();

        // Phase 1: Planning
        self.notify(AgentNotification::TaskPlanning {
            task_id: task_id.clone(),
        });

        // Update status to Planning
        if let Some(task) = self.queue.get_mut(&task_id) {
            task.status = TaskStatus::Planning;
        }

        match self.planner.plan(&task_for_planning).await {
            Ok(steps) => {
                let num_steps = steps.len();
                if let Some(task) = self.queue.get_mut(&task_id) {
                    task.steps = steps;
                    task.status = TaskStatus::Executing;
                }

                self.notify(AgentNotification::TaskStarted {
                    task_id: task_id.clone(),
                    steps: num_steps,
                });
            }
            Err(e) => {
                if let Some(task) = self.queue.get_mut(&task_id) {
                    task.status = TaskStatus::Failed;
                }
                self.notify(AgentNotification::TaskFailed {
                    task_id: task_id.clone(),
                    error: e.to_string(),
                });
                self.state = AgentState::Idle;
                return Err(e.into());
            }
        }

        // Phase 2: Execution
        let total_steps = self.queue.get(&task_id).map(|t| t.steps.len()).unwrap_or(0);

        for step_index in 0..total_steps {
            // Update current step index and check budget
            let over_budget = {
                if let Some(task) = self.queue.get_mut(&task_id) {
                    task.current_step_index = step_index;
                    task.over_budget()
                } else {
                    false
                }
            };

            if over_budget {
                warn!("Task {} exceeded token budget", task_id);
                if let Some(task) = self.queue.get_mut(&task_id) {
                    task.status = TaskStatus::Failed;
                }
                self.notify(AgentNotification::TaskFailed {
                    task_id: task_id.clone(),
                    error: "Token budget exceeded".to_string(),
                });
                self.state = AgentState::Idle;
                return Ok(());
            }

            // Clone step and context for execution
            let (mut step, mut context, is_user_input) = {
                let task = self.queue.get(&task_id).unwrap();
                let step = task.steps[step_index].clone();
                let is_user_input = matches!(step.step_type, types::StepType::UserInput { .. });
                (step, task.context.clone(), is_user_input)
            };

            // Check for user input requirement
            if is_user_input {
                if let Some(task) = self.queue.get_mut(&task_id) {
                    task.status = TaskStatus::AwaitingInput;
                }
                if let types::StepType::UserInput { ref prompt } = step.step_type {
                    self.notify(AgentNotification::UserInputRequired {
                        task_id: task_id.clone(),
                        prompt: prompt.clone(),
                    });
                }
                self.state = AgentState::Idle;
                return Ok(());
            }

            // Execute the step
            match self.executor.execute_step(&mut step, &mut context).await {
                Ok(_) => {
                    if let Some(task) = self.queue.get_mut(&task_id) {
                        task.steps[step_index] = step.clone();
                        task.context = context;
                        task.tokens_used += step.tokens_used;
                    }

                    self.notify(AgentNotification::StepCompleted {
                        task_id: task_id.clone(),
                        step_index,
                        total_steps,
                        description: step.description.clone(),
                    });
                }
                Err(executor::ExecutorError::UserInputRequired) => {
                    if let Some(task) = self.queue.get_mut(&task_id) {
                        task.status = TaskStatus::AwaitingInput;
                    }
                    if let types::StepType::UserInput { ref prompt } = step.step_type {
                        self.notify(AgentNotification::UserInputRequired {
                            task_id: task_id.clone(),
                            prompt: prompt.clone(),
                        });
                    }
                    self.state = AgentState::Idle;
                    return Ok(());
                }
                Err(e) => {
                    if let Some(task) = self.queue.get_mut(&task_id) {
                        task.steps[step_index] = step;
                        task.context = context;
                        task.status = TaskStatus::Failed;
                    }

                    self.notify(AgentNotification::TaskFailed {
                        task_id: task_id.clone(),
                        error: e.to_string(),
                    });
                    self.state = AgentState::Idle;
                    return Ok(());
                }
            }
        }

        // Phase 3: Evaluation
        let duration = start_time.elapsed().as_secs();
        let task_for_eval = self.queue.get(&task_id).unwrap().clone();
        let result = self.evaluator.evaluate(&task_for_eval, duration).await;

        let summary = result.summary.clone();
        let success = result.success;

        if let Some(task) = self.queue.get_mut(&task_id) {
            task.result = Some(result);
            task.status = if success {
                TaskStatus::Completed
            } else {
                TaskStatus::Failed
            };
        }

        // Get final status for persistence
        let final_status = self.queue.get(&task_id).map(|t| t.status).unwrap_or(TaskStatus::Failed);

        // Persist final state
        self.queue.update_status(&task_id, final_status).await?;

        if success {
            self.notify(AgentNotification::TaskCompleted {
                task_id: task_id.clone(),
                summary,
            });
        } else {
            self.notify(AgentNotification::TaskFailed {
                task_id: task_id.clone(),
                error: summary,
            });
        }

        self.state = AgentState::Idle;
        info!("Task {} completed", task_id);
        Ok(())
    }

    /// Submit a new task.
    pub async fn submit_task(&mut self, title: String, description: String, created_by: String) -> Result<TaskId, AgentError> {
        let mut task = Task::new(title.clone(), description, created_by);
        task.token_budget = self.config.max_tokens_per_task;

        let task_id = self.queue.submit(task).await?;

        self.notify(AgentNotification::TaskSubmitted {
            task_id: task_id.clone(),
            title,
        });

        Ok(task_id)
    }

    /// Submit a task with priority.
    pub async fn submit_task_with_priority(
        &mut self,
        title: String,
        description: String,
        created_by: String,
        priority: TaskPriority,
    ) -> Result<TaskId, AgentError> {
        let mut task = Task::new(title.clone(), description, created_by);
        task.priority = priority;
        task.token_budget = self.config.max_tokens_per_task;

        let task_id = self.queue.submit(task).await?;

        self.notify(AgentNotification::TaskSubmitted {
            task_id: task_id.clone(),
            title,
        });

        Ok(task_id)
    }

    /// Get task status.
    pub fn get_task(&self, id: &TaskId) -> Option<&Task> {
        self.queue.get(id)
    }

    /// List all tasks.
    pub fn list_tasks(&self, status: Option<TaskStatus>) -> Vec<&Task> {
        self.queue.list(status)
    }

    /// Provide user input for a waiting task.
    pub async fn provide_input(&mut self, task_id: &TaskId, key: String, value: String) -> Result<(), AgentError> {
        self.queue.provide_input(task_id, key, value).await?;
        Ok(())
    }

    /// Cancel a task.
    pub async fn cancel_task(&mut self, id: &TaskId) -> Result<(), AgentError> {
        self.queue.cancel(id).await?;
        Ok(())
    }

    /// Pause the agent.
    pub fn pause(&mut self) {
        self.state = AgentState::Paused;
        info!("Agent paused");
    }

    /// Resume the agent.
    pub fn resume(&mut self) {
        self.state = AgentState::Idle;
        info!("Agent resumed");
    }

    /// Get current agent state.
    pub fn state(&self) -> AgentState {
        self.state
    }

    /// Get pending task count.
    pub fn pending_count(&self) -> usize {
        self.queue.pending_count()
    }

    /// Check if agent is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }
}
