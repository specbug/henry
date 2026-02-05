//! Task queue with persistence.

use crate::types::{Task, TaskId, TaskPriority, TaskStatus};
use henry_state::{StateManager, TaskRecord};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info};

/// Error type for task queue operations.
#[derive(Debug, thiserror::Error)]
pub enum TaskQueueError {
    #[error("Task not found: {0}")]
    TaskNotFound(TaskId),
    #[error("Task is in invalid state for this operation: {0}")]
    InvalidState(String),
    #[error("Database error: {0}")]
    Database(String),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Task queue with in-memory queue and database persistence.
pub struct TaskQueue {
    /// In-memory queue of task IDs (ordered by priority).
    queue: VecDeque<TaskId>,
    /// All known tasks (including completed).
    tasks: std::collections::HashMap<TaskId, Task>,
    /// Database for persistence.
    state: Arc<RwLock<StateManager>>,
}

impl TaskQueue {
    /// Create a new task queue.
    pub fn new(state: Arc<RwLock<StateManager>>) -> Self {
        Self {
            queue: VecDeque::new(),
            tasks: std::collections::HashMap::new(),
            state,
        }
    }

    /// Load pending tasks from the database.
    pub async fn load_from_db(&mut self) -> Result<(), TaskQueueError> {
        // Load from database with scoped lock
        let records = {
            let state = self.state.read().await;
            state
                .get_tasks(100)
                .await
                .map_err(|e| TaskQueueError::Database(e.to_string()))?
        };

        for record in records {
            if let Some(ref context) = record.context {
                match serde_json::from_str::<Task>(context) {
                    Ok(task) => {
                        if !task.is_terminal() {
                            self.queue.push_back(task.id.clone());
                        }
                        self.tasks.insert(task.id.clone(), task);
                    }
                    Err(e) => {
                        error!("Failed to deserialize task {}: {}", record.id, e);
                    }
                }
            }
        }

        // Sort queue by priority
        self.sort_queue();

        info!("Loaded {} tasks from database", self.tasks.len());
        Ok(())
    }

    /// Submit a new task to the queue.
    pub async fn submit(&mut self, task: Task) -> Result<TaskId, TaskQueueError> {
        let id = task.id.clone();

        // Persist to database
        self.persist_task(&task).await?;

        // Add to in-memory structures
        self.queue.push_back(id.clone());
        self.tasks.insert(id.clone(), task);

        // Re-sort by priority
        self.sort_queue();

        info!("Task {} submitted to queue", id);
        Ok(id)
    }

    /// Get the next task to process.
    pub fn next(&mut self) -> Option<&mut Task> {
        // Find first queued task
        let task_id = self.queue.iter().find(|id| {
            self.tasks
                .get(*id)
                .map(|t| t.status == TaskStatus::Queued)
                .unwrap_or(false)
        })?;

        let task_id = task_id.clone();
        self.tasks.get_mut(&task_id)
    }

    /// Get a task by ID.
    pub fn get(&self, id: &TaskId) -> Option<&Task> {
        self.tasks.get(id)
    }

    /// Get a task by ID mutably.
    pub fn get_mut(&mut self, id: &TaskId) -> Option<&mut Task> {
        self.tasks.get_mut(id)
    }

    /// Update a task's status and persist.
    pub async fn update_status(
        &mut self,
        id: &TaskId,
        status: TaskStatus,
    ) -> Result<(), TaskQueueError> {
        // Get task, update it, and clone for persistence
        let task_clone = {
            let task = self
                .tasks
                .get_mut(id)
                .ok_or_else(|| TaskQueueError::TaskNotFound(id.clone()))?;
            task.status = status;
            task.clone()
        };

        self.persist_task(&task_clone).await?;

        debug!("Task {} status updated to {}", id, status);

        // Remove from queue if terminal
        if task_clone.is_terminal() {
            self.queue.retain(|qid| qid != id);
        }

        Ok(())
    }

    /// Provide user input for a task awaiting it.
    pub async fn provide_input(
        &mut self,
        id: &TaskId,
        key: String,
        value: String,
    ) -> Result<(), TaskQueueError> {
        // Get task, update it, and clone for persistence
        let task_clone = {
            let task = self
                .tasks
                .get_mut(id)
                .ok_or_else(|| TaskQueueError::TaskNotFound(id.clone()))?;

            if task.status != TaskStatus::AwaitingInput {
                return Err(TaskQueueError::InvalidState(format!(
                    "Task is not awaiting input (status: {})",
                    task.status
                )));
            }

            task.context.user_inputs.insert(key, value);
            task.status = TaskStatus::Executing;
            task.clone()
        };

        self.persist_task(&task_clone).await?;

        info!("User input provided for task {}", id);
        Ok(())
    }

    /// Cancel a task.
    pub async fn cancel(&mut self, id: &TaskId) -> Result<(), TaskQueueError> {
        // Get task, update it, and clone for persistence
        let task_clone = {
            let task = self
                .tasks
                .get_mut(id)
                .ok_or_else(|| TaskQueueError::TaskNotFound(id.clone()))?;

            if task.is_terminal() {
                return Err(TaskQueueError::InvalidState(
                    "Cannot cancel a completed/failed task".to_string(),
                ));
            }

            task.status = TaskStatus::Cancelled;
            task.clone()
        };

        self.persist_task(&task_clone).await?;
        self.queue.retain(|qid| qid != id);

        info!("Task {} cancelled", id);
        Ok(())
    }

    /// List all tasks (optionally filtered by status).
    pub fn list(&self, status_filter: Option<TaskStatus>) -> Vec<&Task> {
        self.tasks
            .values()
            .filter(|t| status_filter.map(|s| t.status == s).unwrap_or(true))
            .collect()
    }

    /// Get queue length (pending tasks only).
    pub fn pending_count(&self) -> usize {
        self.queue
            .iter()
            .filter(|id| {
                self.tasks
                    .get(*id)
                    .map(|t| t.status == TaskStatus::Queued)
                    .unwrap_or(false)
            })
            .count()
    }

    /// Persist a task to the database.
    async fn persist_task(&self, task: &Task) -> Result<(), TaskQueueError> {
        let state = self.state.read().await;

        // Serialize task context (contains all task data)
        let context = serde_json::to_string(task)?;

        // Serialize steps separately
        let steps = serde_json::to_string(&task.steps)?;

        // Serialize result if present
        let result = task.result.as_ref().map(|r| {
            serde_json::to_string(r).unwrap_or_else(|_| "{}".to_string())
        });

        // Create a TaskRecord
        let record = TaskRecord {
            id: task.id.clone(),
            title: task.title.clone(),
            description: Some(task.description.clone()),
            created_at: task.created_at.to_rfc3339(),
            created_by: Some(task.created_by.clone()),
            priority: task.priority.to_string(),
            status: task.status.to_string(),
            steps: Some(steps),
            current_step_index: task.current_step_index as i32,
            context: Some(context),
            result,
        };

        // Use save_task which handles both insert and update
        state
            .save_task(&record)
            .await
            .map_err(|e| TaskQueueError::Database(e.to_string()))?;

        Ok(())
    }

    /// Sort the queue by priority (Critical > High > Normal > Low).
    fn sort_queue(&mut self) {
        let tasks = &self.tasks;
        let mut queue_vec: Vec<_> = self.queue.drain(..).collect();

        queue_vec.sort_by(|a, b| {
            let priority_a = tasks.get(a).map(|t| &t.priority);
            let priority_b = tasks.get(b).map(|t| &t.priority);

            // Higher priority first
            match (priority_a, priority_b) {
                (Some(TaskPriority::Critical), Some(TaskPriority::Critical)) => {
                    std::cmp::Ordering::Equal
                }
                (Some(TaskPriority::Critical), _) => std::cmp::Ordering::Less,
                (_, Some(TaskPriority::Critical)) => std::cmp::Ordering::Greater,
                (Some(TaskPriority::High), Some(TaskPriority::High)) => std::cmp::Ordering::Equal,
                (Some(TaskPriority::High), _) => std::cmp::Ordering::Less,
                (_, Some(TaskPriority::High)) => std::cmp::Ordering::Greater,
                (Some(TaskPriority::Normal), Some(TaskPriority::Normal)) => {
                    std::cmp::Ordering::Equal
                }
                (Some(TaskPriority::Normal), _) => std::cmp::Ordering::Less,
                (_, Some(TaskPriority::Normal)) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            }
        });

        self.queue = queue_vec.into();
    }
}
