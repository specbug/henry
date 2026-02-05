//! Decision engine that matches anomalies to policies and creates actions.

use crate::policy::RemediationPolicy;
use crate::types::{Action, ActionType, Anomaly, ControlLoopState, NotifyUrgency};
use tracing::{debug, info, warn};

/// Result of decision engine evaluation.
#[derive(Debug)]
pub struct Decision {
    /// The anomaly that was evaluated.
    pub anomaly: Anomaly,
    /// Actions to take (may be empty if no policy matches).
    pub actions: Vec<Action>,
    /// Whether the anomaly was handled by a policy.
    pub handled: bool,
    /// Reason for the decision.
    pub reason: String,
}

/// The decision engine.
pub struct DecisionEngine {
    /// Remediation policy.
    policy: RemediationPolicy,
    /// Maximum concurrent actions.
    max_concurrent_actions: usize,
    /// Currently executing action count.
    executing_count: usize,
}

impl DecisionEngine {
    /// Create a new decision engine with the given policy.
    pub fn new(policy: RemediationPolicy) -> Self {
        Self {
            policy,
            max_concurrent_actions: 3,
            executing_count: 0,
        }
    }

    /// Set maximum concurrent actions.
    pub fn set_max_concurrent_actions(&mut self, max: usize) {
        self.max_concurrent_actions = max;
    }

    /// Update executing count.
    pub fn set_executing_count(&mut self, count: usize) {
        self.executing_count = count;
    }

    /// Evaluate an anomaly and decide what actions to take.
    pub fn evaluate(
        &mut self,
        anomaly: Anomaly,
        loop_state: &ControlLoopState,
        max_actions_per_hour: u32,
    ) -> Decision {
        // Check if we're at the concurrent action limit
        if self.executing_count >= self.max_concurrent_actions {
            return Decision {
                anomaly,
                actions: vec![],
                handled: false,
                reason: "Max concurrent actions reached".to_string(),
            };
        }

        // Check hourly rate limit
        if !loop_state.can_execute_action(max_actions_per_hour) {
            warn!(
                "Hourly action limit reached ({}/{}), deferring action",
                loop_state.actions_this_hour, max_actions_per_hour
            );
            return Decision {
                anomaly: anomaly.clone(),
                actions: vec![Action::new(
                    &anomaly.id,
                    ActionType::NotifyUser {
                        message: format!(
                            "Rate limit reached: {} (deferred)",
                            anomaly.event.description()
                        ),
                        urgency: NotifyUrgency::Medium,
                    },
                    false,
                )],
                handled: true,
                reason: "Hourly rate limit reached".to_string(),
            };
        }

        // Check quiet hours
        let in_quiet_hours = self.policy.is_quiet_hours();
        if in_quiet_hours {
            // During quiet hours, only allow critical and emergency actions
            if anomaly.severity < crate::types::Severity::Critical {
                debug!(
                    "Quiet hours: deferring non-critical anomaly {:?}",
                    anomaly.event.event_type()
                );
                return Decision {
                    anomaly,
                    actions: vec![],
                    handled: false,
                    reason: "Quiet hours (non-critical)".to_string(),
                };
            }
        }

        // Find matching policy rule
        let matched_rule = self.policy.find_matching_rule(&anomaly.event, anomaly.severity).cloned();
        if let Some(rule) = matched_rule {
            info!(
                "Rule '{}' matched for anomaly {} ({:?})",
                rule.name,
                anomaly.id,
                anomaly.event.event_type()
            );

            // Convert rule action to ActionType
            if let Some(action_type) = self.policy.action_from_rule(&rule, &anomaly.event) {
                let mut actions = vec![];

                // Add pre-action if configured
                if let Some(ref pre_action_name) = rule.pre_action {
                    if let Some(pre_action_type) = self.parse_pre_action(pre_action_name) {
                        actions.push(Action::new(&anomaly.id, pre_action_type, false));
                    }
                }

                // Add main action
                let requires_approval = rule.requires_approval || in_quiet_hours;
                actions.push(Action::new(&anomaly.id, action_type, requires_approval));

                // Record the trigger
                self.policy.record_trigger(&rule.name);

                return Decision {
                    anomaly,
                    actions,
                    handled: true,
                    reason: format!("Matched rule: {}", rule.name),
                };
            }
        }

        // No policy matched - use default behavior based on severity
        let default_action = self.default_action_for_anomaly(&anomaly);
        if let Some(action_type) = default_action {
            return Decision {
                anomaly: anomaly.clone(),
                actions: vec![Action::new(&anomaly.id, action_type, true)], // Always require approval for default actions
                handled: true,
                reason: "Default action (no policy match)".to_string(),
            };
        }

        Decision {
            anomaly,
            actions: vec![],
            handled: false,
            reason: "No matching policy".to_string(),
        }
    }

    /// Parse a pre-action name to an ActionType.
    fn parse_pre_action(&self, name: &str) -> Option<ActionType> {
        match name {
            "backup_binary" => Some(ActionType::RunCommand {
                command: "cp".to_string(),
                args: vec![
                    "-p".to_string(),
                    std::env::current_exe()
                        .ok()?
                        .to_string_lossy()
                        .to_string(),
                    format!(
                        "{}.backup.{}",
                        std::env::current_exe().ok()?.to_string_lossy(),
                        chrono::Utc::now().format("%Y%m%d%H%M%S")
                    ),
                ],
                timeout_secs: 30,
            }),
            "rotate_logs" => Some(ActionType::RotateLogs),
            "backup" => Some(ActionType::TriggerBackup),
            _ => None,
        }
    }

    /// Get default action for an anomaly when no policy matches.
    fn default_action_for_anomaly(&self, anomaly: &Anomaly) -> Option<ActionType> {
        // For unhandled anomalies, default to notification
        Some(ActionType::NotifyUser {
            message: anomaly.event.description(),
            urgency: match anomaly.severity {
                crate::types::Severity::Emergency | crate::types::Severity::Critical => {
                    NotifyUrgency::High
                }
                crate::types::Severity::Warning => NotifyUrgency::Medium,
                crate::types::Severity::Info => NotifyUrgency::Low,
            },
        })
    }

    /// Get the policy (for introspection).
    pub fn policy(&self) -> &RemediationPolicy {
        &self.policy
    }

    /// Get mutable policy reference.
    pub fn policy_mut(&mut self) -> &mut RemediationPolicy {
        &mut self.policy
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SystemEvent;

    #[test]
    fn test_decision_engine_basic() {
        let policy = RemediationPolicy::default();
        let mut engine = DecisionEngine::new(policy);
        let loop_state = ControlLoopState::default();

        let anomaly = Anomaly::new(SystemEvent::ContainerCrashed {
            name: "test".to_string(),
            exit_code: Some(1),
        });

        let decision = engine.evaluate(anomaly, &loop_state, 20);
        assert!(decision.handled);
        assert!(!decision.actions.is_empty());
    }

    #[test]
    fn test_rate_limit() {
        let policy = RemediationPolicy::default();
        let mut engine = DecisionEngine::new(policy);
        let mut loop_state = ControlLoopState::default();

        // Max out the rate limit
        for _ in 0..20 {
            loop_state.record_action();
        }

        let anomaly = Anomaly::new(SystemEvent::ContainerCrashed {
            name: "test".to_string(),
            exit_code: Some(1),
        });

        let decision = engine.evaluate(anomaly, &loop_state, 20);
        assert!(decision.reason.contains("rate limit"));
    }

    #[test]
    fn test_concurrent_limit() {
        let policy = RemediationPolicy::default();
        let mut engine = DecisionEngine::new(policy);
        engine.set_max_concurrent_actions(3);
        engine.set_executing_count(3);

        let loop_state = ControlLoopState::default();
        let anomaly = Anomaly::new(SystemEvent::ContainerCrashed {
            name: "test".to_string(),
            exit_code: Some(1),
        });

        let decision = engine.evaluate(anomaly, &loop_state, 20);
        assert!(!decision.handled);
        assert!(decision.reason.contains("concurrent"));
    }
}
