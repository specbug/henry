//! Reactive control loop for Henry daemon.
//!
//! Provides continuous monitoring, anomaly detection, auto-remediation, and escalation.
//!
//! The reactive loop follows this pattern:
//! 1. OBSERVE: Collect metrics from HealthManager, poll container states
//! 2. DETECT: Compare against thresholds, emit SystemEvents
//! 3. DECIDE: Match events to policy rules, check cooldowns/rate limits
//! 4. EXECUTE: Run auto-approved actions with timeout
//! 5. VERIFY: Confirm fix worked (re-check metrics after delay)
//! 6. ESCALATE: If failed or needs approval → notify user

pub mod action;
pub mod decision;
pub mod observer;
pub mod policy;
pub mod types;

use action::{ActionExecutor, NotifyCallback};
use decision::DecisionEngine;
use observer::{AnomalyObserver, ObserverThresholds};
use policy::{RemediationPolicy, RemediationRule};
use types::{Action, ActionStatus, Anomaly, ControlLoopState};

use chrono::{DateTime, Utc};
use henry_health::HealthManager;
use henry_maint::MaintenanceManager;
use henry_server::ContainerManager;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::{debug, error, info};

pub use types::{ActionType, NotifyUrgency, Severity, SystemEvent};

#[derive(Error, Debug)]
pub enum ReactiveError {
    #[error("engine not started")]
    NotStarted,

    #[error("engine already running")]
    AlreadyRunning,

    #[error("action not found: {0}")]
    ActionNotFound(String),

    #[error("anomaly not found: {0}")]
    AnomalyNotFound(String),

    #[error("state error: {0}")]
    State(#[from] henry_state::StateError),

    #[error("execution error: {0}")]
    Execution(#[from] action::ExecutorError),
}

/// Configuration for the reactive engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactiveConfig {
    /// Whether the reactive engine is enabled.
    pub enabled: bool,
    /// Check interval in seconds.
    pub check_interval_secs: u64,
    /// Maximum actions per hour.
    pub max_actions_per_hour: u32,
    /// Maximum concurrent actions.
    pub max_concurrent_actions: usize,
    /// Observer thresholds.
    pub thresholds: ThresholdsConfig,
    /// Remediation rules.
    pub rules: Vec<RemediationRule>,
    /// Notification configuration.
    pub notifications: NotificationsConfig,
    /// Quiet hours (start, end) in 24h format.
    pub quiet_hours: Option<(u8, u8)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThresholdsConfig {
    pub disk_warning_percent: f32,
    pub disk_critical_percent: f32,
    pub memory_warning_percent: f32,
    pub memory_critical_percent: f32,
    pub cpu_warning_percent: f32,
    pub cpu_alert_duration_secs: u64,
}

impl Default for ThresholdsConfig {
    fn default() -> Self {
        Self {
            disk_warning_percent: 80.0,
            disk_critical_percent: 95.0,
            memory_warning_percent: 85.0,
            memory_critical_percent: 95.0,
            cpu_warning_percent: 90.0,
            cpu_alert_duration_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationsConfig {
    /// Verbosity level: all, milestones, critical.
    pub verbosity: String,
    /// Include routine fixes in notifications.
    pub include_routine_fixes: bool,
    /// Batch interval for notifications (0 = immediate).
    pub batch_interval_secs: u64,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            verbosity: "all".to_string(),
            include_routine_fixes: true,
            batch_interval_secs: 0,
        }
    }
}

impl Default for ReactiveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_interval_secs: 10,
            max_actions_per_hour: 20,
            max_concurrent_actions: 3,
            thresholds: ThresholdsConfig::default(),
            rules: RemediationPolicy::default_rules(),
            notifications: NotificationsConfig::default(),
            quiet_hours: Some((2, 6)),
        }
    }
}

/// Status of the reactive engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactiveStatus {
    /// Whether the engine is running.
    pub running: bool,
    /// Whether the engine is paused.
    pub paused: bool,
    /// When the engine was started.
    pub started_at: Option<DateTime<Utc>>,
    /// Total checks performed.
    pub total_checks: u64,
    /// Total anomalies detected.
    pub anomalies_detected: u64,
    /// Total actions executed.
    pub actions_executed: u64,
    /// Actions in the last hour.
    pub actions_this_hour: u32,
    /// Active (unresolved) anomalies.
    pub active_anomalies: usize,
    /// Actions awaiting approval.
    pub pending_approvals: usize,
    /// Last check time.
    pub last_check: Option<DateTime<Utc>>,
    /// Current quiet hours status.
    pub in_quiet_hours: bool,
}

/// Commands that can be sent to the reactive engine.
#[derive(Debug)]
pub enum ReactiveCommand {
    /// Pause the control loop.
    Pause,
    /// Resume the control loop.
    Resume,
    /// Shutdown the engine.
    Shutdown,
    /// Approve a pending action.
    ApproveAction { action_id: String, approved_by: String },
    /// Cancel an action.
    CancelAction { action_id: String },
    /// Inject a custom anomaly (for testing).
    InjectAnomaly(Anomaly),
}

/// The main reactive engine.
pub struct ReactiveEngine {
    /// Configuration.
    config: ReactiveConfig,
    /// Control loop state.
    state: Arc<RwLock<ControlLoopState>>,
    /// Observer for detecting anomalies.
    observer: Arc<RwLock<AnomalyObserver>>,
    /// Decision engine for matching policies.
    decision: Arc<RwLock<DecisionEngine>>,
    /// Action executor.
    executor: Arc<RwLock<ActionExecutor>>,
    /// Active anomalies (id -> anomaly).
    anomalies: Arc<RwLock<HashMap<String, Anomaly>>>,
    /// Pending and executing actions (id -> action).
    actions: Arc<RwLock<HashMap<String, Action>>>,
    /// Command channel sender.
    command_tx: Option<mpsc::Sender<ReactiveCommand>>,
    /// Shutdown broadcast sender.
    shutdown_tx: Option<broadcast::Sender<()>>,
}

impl ReactiveEngine {
    /// Create a new reactive engine.
    pub fn new(
        config: ReactiveConfig,
        health: Arc<RwLock<HealthManager>>,
        containers: Option<Arc<RwLock<ContainerManager>>>,
        maint: Option<Arc<RwLock<MaintenanceManager>>>,
    ) -> Self {
        // Build observer thresholds
        let thresholds = ObserverThresholds {
            disk_warning_percent: config.thresholds.disk_warning_percent,
            disk_critical_percent: config.thresholds.disk_critical_percent,
            memory_warning_percent: config.thresholds.memory_warning_percent,
            memory_critical_percent: config.thresholds.memory_critical_percent,
            cpu_warning_percent: config.thresholds.cpu_warning_percent,
            cpu_alert_duration_secs: config.thresholds.cpu_alert_duration_secs,
        };

        // Build policy
        let mut policy = RemediationPolicy::new(config.rules.clone());
        if let Some((start, end)) = config.quiet_hours {
            policy.set_quiet_hours(start, end);
        }

        // Create components
        let observer = AnomalyObserver::new(health, containers.clone(), thresholds);
        let decision = DecisionEngine::new(policy);
        let executor = ActionExecutor::new(containers, maint);

        Self {
            config,
            state: Arc::new(RwLock::new(ControlLoopState::default())),
            observer: Arc::new(RwLock::new(observer)),
            decision: Arc::new(RwLock::new(decision)),
            executor: Arc::new(RwLock::new(executor)),
            anomalies: Arc::new(RwLock::new(HashMap::new())),
            actions: Arc::new(RwLock::new(HashMap::new())),
            command_tx: None,
            shutdown_tx: None,
        }
    }

    /// Set the notification callback.
    pub async fn set_notify_callback(&self, callback: NotifyCallback) {
        self.executor.write().await.set_notify_callback(callback);
    }

    /// Start the reactive control loop.
    pub async fn start(&mut self, mut shutdown_rx: broadcast::Receiver<()>) -> Result<(), ReactiveError> {
        // Check if already running
        {
            let state = self.state.read().await;
            if state.running {
                return Err(ReactiveError::AlreadyRunning);
            }
        }

        // Create command channel
        let (command_tx, mut command_rx) = mpsc::channel::<ReactiveCommand>(32);
        let (shutdown_tx, _) = broadcast::channel(1);
        self.command_tx = Some(command_tx);
        self.shutdown_tx = Some(shutdown_tx.clone());

        // Mark as started
        {
            let mut state = self.state.write().await;
            state.running = true;
            state.started_at = Some(Utc::now());
        }

        info!("Reactive engine started");

        // Clone Arcs for the loop
        let state = self.state.clone();
        let observer = self.observer.clone();
        let decision = self.decision.clone();
        let executor = self.executor.clone();
        let anomalies = self.anomalies.clone();
        let actions = self.actions.clone();
        let check_interval = Duration::from_secs(self.config.check_interval_secs);
        let max_actions_per_hour = self.config.max_actions_per_hour;
        let max_concurrent = self.config.max_concurrent_actions;

        // Spawn the main loop
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(check_interval);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // Check if paused
                        let is_paused = {
                            let s = state.read().await;
                            s.paused
                        };

                        if !is_paused {
                            Self::run_cycle(
                                &state,
                                &observer,
                                &decision,
                                &executor,
                                &anomalies,
                                &actions,
                                max_actions_per_hour,
                                max_concurrent,
                            ).await;
                        }
                    }
                    Some(cmd) = command_rx.recv() => {
                        match cmd {
                            ReactiveCommand::Pause => {
                                let mut s = state.write().await;
                                s.paused = true;
                                info!("Reactive engine paused");
                            }
                            ReactiveCommand::Resume => {
                                let mut s = state.write().await;
                                s.paused = false;
                                info!("Reactive engine resumed");
                            }
                            ReactiveCommand::Shutdown => {
                                info!("Reactive engine shutting down");
                                let _ = shutdown_tx.send(());
                                break;
                            }
                            ReactiveCommand::ApproveAction { action_id, approved_by } => {
                                let mut actions_guard = actions.write().await;
                                if let Some(action) = actions_guard.get_mut(&action_id) {
                                    action.approve(&approved_by);
                                    info!("Action {} approved by {}", action_id, approved_by);
                                }
                            }
                            ReactiveCommand::CancelAction { action_id } => {
                                let mut actions_guard = actions.write().await;
                                if let Some(action) = actions_guard.get_mut(&action_id) {
                                    action.status = ActionStatus::Cancelled;
                                    info!("Action {} cancelled", action_id);
                                }
                            }
                            ReactiveCommand::InjectAnomaly(anomaly) => {
                                let mut anomalies_guard = anomalies.write().await;
                                info!("Injected anomaly: {:?}", anomaly.event.event_type());
                                anomalies_guard.insert(anomaly.id.clone(), anomaly);
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        info!("Reactive engine received shutdown signal");
                        break;
                    }
                }
            }

            // Mark as stopped
            let mut s = state.write().await;
            s.running = false;
            info!("Reactive engine stopped");
        });

        Ok(())
    }

    /// Run a single cycle of the control loop.
    async fn run_cycle(
        state: &Arc<RwLock<ControlLoopState>>,
        observer: &Arc<RwLock<AnomalyObserver>>,
        decision: &Arc<RwLock<DecisionEngine>>,
        executor: &Arc<RwLock<ActionExecutor>>,
        anomalies: &Arc<RwLock<HashMap<String, Anomaly>>>,
        actions: &Arc<RwLock<HashMap<String, Action>>>,
        max_actions_per_hour: u32,
        max_concurrent: usize,
    ) {
        debug!("Running reactive cycle");

        // Update state
        {
            let mut s = state.write().await;
            s.total_checks += 1;
            s.last_check = Some(Utc::now());
        }

        // 1. OBSERVE - Detect anomalies
        let new_anomalies = {
            let mut obs = observer.write().await;
            obs.observe().await
        };

        if !new_anomalies.is_empty() {
            debug!("Detected {} anomalies", new_anomalies.len());
        }

        // 2. Record anomalies and create actions
        for anomaly in new_anomalies {
            {
                let mut s = state.write().await;
                s.anomalies_detected += 1;
            }

            // 3. DECIDE - Get actions for this anomaly
            let loop_state = state.read().await.clone();
            let executing_count = {
                let acts = actions.read().await;
                acts.values().filter(|a| a.status == ActionStatus::Executing).count()
            };

            let decision_result = {
                let mut dec = decision.write().await;
                dec.set_executing_count(executing_count);
                dec.set_max_concurrent_actions(max_concurrent);
                dec.evaluate(anomaly.clone(), &loop_state, max_actions_per_hour)
            };

            // Store anomaly
            {
                let mut anoms = anomalies.write().await;
                anoms.insert(anomaly.id.clone(), decision_result.anomaly);
            }

            // Store and queue actions
            for action in decision_result.actions {
                let mut acts = actions.write().await;
                acts.insert(action.id.clone(), action);
            }
        }

        // 4. EXECUTE - Process pending actions
        let pending_action_ids: Vec<String> = {
            let acts = actions.read().await;
            acts.iter()
                .filter(|(_, a)| a.status == ActionStatus::Pending)
                .map(|(id, _)| id.clone())
                .collect()
        };

        for action_id in pending_action_ids {
            // Check concurrent limit
            let executing_count = {
                let acts = actions.read().await;
                acts.values().filter(|a| a.status == ActionStatus::Executing).count()
            };
            if executing_count >= max_concurrent {
                break;
            }

            // Execute the action
            let mut action = {
                let mut acts = actions.write().await;
                acts.remove(&action_id)
            };

            if let Some(ref mut action) = action {
                let exec = executor.read().await;
                let result = exec.execute(action).await;

                match result {
                    Ok(exec_result) => {
                        if exec_result.success {
                            // 5. VERIFY - Mark anomaly as resolved if action succeeded
                            let mut anoms = anomalies.write().await;
                            if let Some(anomaly) = anoms.get_mut(&action.anomaly_id) {
                                anomaly.resolve(&action.id);
                            }

                            let mut s = state.write().await;
                            s.record_action();
                        } else if action.can_retry() {
                            // Retry the action
                            action.retry();
                        }
                    }
                    Err(e) => {
                        error!("Action execution error: {}", e);
                        if action.can_retry() {
                            action.retry();
                        }
                    }
                }

                // Put action back
                let mut acts = actions.write().await;
                acts.insert(action_id, action.clone());
            }
        }

        // Update state counts
        {
            let mut s = state.write().await;
            let anoms = anomalies.read().await;
            let acts = actions.read().await;
            s.active_anomalies = anoms.values().filter(|a| !a.is_resolved()).count();
            s.pending_approvals = acts.values().filter(|a| a.status == ActionStatus::AwaitingApproval).count();
        }
    }

    /// Get the current status.
    pub async fn status(&self) -> ReactiveStatus {
        let state = self.state.read().await;
        let decision = self.decision.read().await;

        ReactiveStatus {
            running: state.running,
            paused: state.paused,
            started_at: state.started_at,
            total_checks: state.total_checks,
            anomalies_detected: state.anomalies_detected,
            actions_executed: state.actions_executed,
            actions_this_hour: state.actions_this_hour,
            active_anomalies: state.active_anomalies,
            pending_approvals: state.pending_approvals,
            last_check: state.last_check,
            in_quiet_hours: decision.policy().is_quiet_hours(),
        }
    }

    /// Pause the control loop.
    pub async fn pause(&self) -> Result<(), ReactiveError> {
        if let Some(ref tx) = self.command_tx {
            let _ = tx.send(ReactiveCommand::Pause).await;
            Ok(())
        } else {
            Err(ReactiveError::NotStarted)
        }
    }

    /// Resume the control loop.
    pub async fn resume(&self) -> Result<(), ReactiveError> {
        if let Some(ref tx) = self.command_tx {
            let _ = tx.send(ReactiveCommand::Resume).await;
            Ok(())
        } else {
            Err(ReactiveError::NotStarted)
        }
    }

    /// Shutdown the engine.
    pub async fn shutdown(&self) -> Result<(), ReactiveError> {
        if let Some(ref tx) = self.command_tx {
            let _ = tx.send(ReactiveCommand::Shutdown).await;
            Ok(())
        } else {
            Err(ReactiveError::NotStarted)
        }
    }

    /// Approve a pending action.
    pub async fn approve_action(&self, action_id: &str, approved_by: &str) -> Result<(), ReactiveError> {
        if let Some(ref tx) = self.command_tx {
            let _ = tx
                .send(ReactiveCommand::ApproveAction {
                    action_id: action_id.to_string(),
                    approved_by: approved_by.to_string(),
                })
                .await;
            Ok(())
        } else {
            Err(ReactiveError::NotStarted)
        }
    }

    /// Cancel an action.
    pub async fn cancel_action(&self, action_id: &str) -> Result<(), ReactiveError> {
        if let Some(ref tx) = self.command_tx {
            let _ = tx
                .send(ReactiveCommand::CancelAction {
                    action_id: action_id.to_string(),
                })
                .await;
            Ok(())
        } else {
            Err(ReactiveError::NotStarted)
        }
    }

    /// Get all anomalies.
    pub async fn get_anomalies(&self) -> Vec<Anomaly> {
        let anomalies = self.anomalies.read().await;
        anomalies.values().cloned().collect()
    }

    /// Get active (unresolved) anomalies.
    pub async fn get_active_anomalies(&self) -> Vec<Anomaly> {
        let anomalies = self.anomalies.read().await;
        anomalies.values().filter(|a| !a.is_resolved()).cloned().collect()
    }

    /// Get all actions.
    pub async fn get_actions(&self) -> Vec<Action> {
        let actions = self.actions.read().await;
        actions.values().cloned().collect()
    }

    /// Get pending actions (awaiting approval).
    pub async fn get_pending_actions(&self) -> Vec<Action> {
        let actions = self.actions.read().await;
        actions
            .values()
            .filter(|a| a.status == ActionStatus::AwaitingApproval)
            .cloned()
            .collect()
    }

    /// Get action history (last N actions).
    pub async fn get_action_history(&self, limit: usize) -> Vec<Action> {
        let actions = self.actions.read().await;
        let mut history: Vec<_> = actions.values().cloned().collect();
        history.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        history.truncate(limit);
        history
    }

    /// Inject an anomaly for testing.
    pub async fn inject_anomaly(&self, anomaly: Anomaly) -> Result<(), ReactiveError> {
        if let Some(ref tx) = self.command_tx {
            let _ = tx.send(ReactiveCommand::InjectAnomaly(anomaly)).await;
            Ok(())
        } else {
            Err(ReactiveError::NotStarted)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_engine_creation() {
        let health = Arc::new(RwLock::new(HealthManager::new("0.1.0")));
        let config = ReactiveConfig::default();
        let engine = ReactiveEngine::new(config, health, None, None);

        let status = engine.status().await;
        assert!(!status.running);
        assert!(!status.paused);
    }

    #[tokio::test]
    async fn test_engine_start_stop() {
        let health = Arc::new(RwLock::new(HealthManager::new("0.1.0")));
        let config = ReactiveConfig::default();
        let mut engine = ReactiveEngine::new(config, health, None, None);

        let (shutdown_tx, shutdown_rx) = broadcast::channel(1);

        // Start the engine
        engine.start(shutdown_rx).await.unwrap();

        let status = engine.status().await;
        assert!(status.running);

        // Shutdown
        let _ = shutdown_tx.send(());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
