//! Remediation policies for the reactive control loop.
//!
//! Policies define what actions are auto-approved, their cooldowns, and rate limits.

use crate::types::{ActionType, Severity, SystemEvent};
use chrono::{DateTime, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::debug;

/// A rule that defines how to respond to a specific event pattern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationRule {
    /// Unique name for this rule.
    pub name: String,
    /// Event pattern to match (event type string).
    pub event_pattern: String,
    /// Action to take when the event matches.
    pub action: String,
    /// Minimum severity to trigger this rule.
    #[serde(default = "default_min_severity")]
    pub min_severity: Severity,
    /// Whether this action requires human approval.
    #[serde(default)]
    pub requires_approval: bool,
    /// Cooldown period after triggering (seconds).
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
    /// Maximum times this rule can trigger per day.
    #[serde(default = "default_max_triggers")]
    pub max_triggers_per_day: u32,
    /// Optional action to run before the main action.
    pub pre_action: Option<String>,
    /// Whether this rule is enabled.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_min_severity() -> Severity {
    Severity::Warning
}

fn default_cooldown() -> u64 {
    300 // 5 minutes
}

fn default_max_triggers() -> u32 {
    10
}

fn default_enabled() -> bool {
    true
}

impl Default for RemediationRule {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            event_pattern: "*".to_string(),
            action: "notify_user".to_string(),
            min_severity: Severity::Warning,
            requires_approval: true,
            cooldown_secs: 300,
            max_triggers_per_day: 10,
            pre_action: None,
            enabled: true,
        }
    }
}

/// Tracks rule trigger history for cooldowns and daily limits.
#[derive(Debug, Clone, Default)]
pub struct RuleTriggerHistory {
    /// Last trigger time per rule.
    last_trigger: HashMap<String, DateTime<Utc>>,
    /// Daily trigger counts per rule (resets at midnight).
    daily_counts: HashMap<String, u32>,
    /// When daily counts were last reset.
    daily_reset_date: Option<chrono::NaiveDate>,
}

impl RuleTriggerHistory {
    /// Record a rule trigger.
    pub fn record_trigger(&mut self, rule_name: &str) {
        let now = Utc::now();
        self.last_trigger.insert(rule_name.to_string(), now);

        // Reset daily counts if date changed
        let today = now.date_naive();
        if self.daily_reset_date != Some(today) {
            self.daily_counts.clear();
            self.daily_reset_date = Some(today);
        }

        *self.daily_counts.entry(rule_name.to_string()).or_insert(0) += 1;
    }

    /// Check if a rule is in cooldown.
    pub fn in_cooldown(&self, rule_name: &str, cooldown_secs: u64) -> bool {
        if let Some(last) = self.last_trigger.get(rule_name) {
            let elapsed = Utc::now().signed_duration_since(*last).num_seconds();
            elapsed < cooldown_secs as i64
        } else {
            false
        }
    }

    /// Check if a rule has exceeded its daily limit.
    pub fn exceeded_daily_limit(&self, rule_name: &str, max_per_day: u32) -> bool {
        // Reset daily counts if date changed
        let today = Utc::now().date_naive();
        if self.daily_reset_date != Some(today) {
            return false;
        }

        self.daily_counts.get(rule_name).copied().unwrap_or(0) >= max_per_day
    }

    /// Get the remaining cooldown time for a rule.
    pub fn remaining_cooldown(&self, rule_name: &str, cooldown_secs: u64) -> Option<u64> {
        if let Some(last) = self.last_trigger.get(rule_name) {
            let elapsed = Utc::now().signed_duration_since(*last).num_seconds();
            if elapsed < cooldown_secs as i64 {
                return Some((cooldown_secs as i64 - elapsed) as u64);
            }
        }
        None
    }

    /// Get today's trigger count for a rule.
    pub fn daily_count(&self, rule_name: &str) -> u32 {
        let today = Utc::now().date_naive();
        if self.daily_reset_date != Some(today) {
            return 0;
        }
        self.daily_counts.get(rule_name).copied().unwrap_or(0)
    }
}

/// The remediation policy manager.
#[derive(Debug, Clone)]
pub struct RemediationPolicy {
    /// Configured rules.
    rules: Vec<RemediationRule>,
    /// Trigger history for cooldowns and limits.
    history: RuleTriggerHistory,
    /// Commands that are never allowed to be auto-executed.
    command_blocklist: Vec<String>,
    /// Quiet hours (hour of day, 0-23) when only critical actions are allowed.
    quiet_hours: (u8, u8), // (start, end)
}

impl Default for RemediationPolicy {
    fn default() -> Self {
        Self {
            rules: Self::default_rules(),
            history: RuleTriggerHistory::default(),
            command_blocklist: Self::default_blocklist(),
            quiet_hours: (2, 6), // 2 AM to 6 AM
        }
    }
}

impl RemediationPolicy {
    /// Create a new policy with custom rules.
    pub fn new(rules: Vec<RemediationRule>) -> Self {
        Self {
            rules,
            history: RuleTriggerHistory::default(),
            command_blocklist: Self::default_blocklist(),
            quiet_hours: (2, 6),
        }
    }

    /// Get default remediation rules.
    pub fn default_rules() -> Vec<RemediationRule> {
        vec![
            RemediationRule {
                name: "container_crash_restart".to_string(),
                event_pattern: "container_crashed".to_string(),
                action: "restart_container".to_string(),
                min_severity: Severity::Warning,
                requires_approval: false,
                cooldown_secs: 300,
                max_triggers_per_day: 5,
                pre_action: None,
                enabled: true,
            },
            RemediationRule {
                name: "disk_space_rotate_logs".to_string(),
                event_pattern: "disk_space_low".to_string(),
                action: "rotate_logs".to_string(),
                min_severity: Severity::Warning,
                requires_approval: false,
                cooldown_secs: 3600,
                max_triggers_per_day: 3,
                pre_action: None,
                enabled: true,
            },
            RemediationRule {
                name: "self_update".to_string(),
                event_pattern: "update_available".to_string(),
                action: "self_update".to_string(),
                min_severity: Severity::Info,
                requires_approval: false,
                cooldown_secs: 86400,
                max_triggers_per_day: 1,
                pre_action: Some("backup_binary".to_string()),
                enabled: true,
            },
            RemediationRule {
                name: "backup_retry".to_string(),
                event_pattern: "backup_failed".to_string(),
                action: "trigger_backup".to_string(),
                min_severity: Severity::Warning,
                requires_approval: false,
                cooldown_secs: 1800,
                max_triggers_per_day: 3,
                pre_action: None,
                enabled: true,
            },
            RemediationRule {
                name: "service_restart".to_string(),
                event_pattern: "service_unreachable".to_string(),
                action: "restart_container".to_string(),
                min_severity: Severity::Critical,
                requires_approval: false,
                cooldown_secs: 600,
                max_triggers_per_day: 5,
                pre_action: None,
                enabled: true,
            },
        ]
    }

    /// Get default command blocklist.
    pub fn default_blocklist() -> Vec<String> {
        vec![
            "rm -rf".to_string(),
            "rm -r /".to_string(),
            "dd if=".to_string(),
            "mkfs".to_string(),
            "chmod 777".to_string(),
            "chmod -R 777".to_string(),
            "> /dev/".to_string(),
            ":(){ :|:& };:".to_string(), // fork bomb
        ]
    }

    /// Set quiet hours.
    pub fn set_quiet_hours(&mut self, start: u8, end: u8) {
        self.quiet_hours = (start.min(23), end.min(23));
    }

    /// Check if we're currently in quiet hours.
    pub fn is_quiet_hours(&self) -> bool {
        let hour = Utc::now().hour() as u8;
        let (start, end) = self.quiet_hours;
        if start <= end {
            hour >= start && hour < end
        } else {
            // Wraps midnight (e.g., 22 to 6)
            hour >= start || hour < end
        }
    }

    /// Add a rule to the policy.
    pub fn add_rule(&mut self, rule: RemediationRule) {
        self.rules.push(rule);
    }

    /// Remove a rule by name.
    pub fn remove_rule(&mut self, name: &str) -> bool {
        let len_before = self.rules.len();
        self.rules.retain(|r| r.name != name);
        self.rules.len() < len_before
    }

    /// Get all rules.
    pub fn rules(&self) -> &[RemediationRule] {
        &self.rules
    }

    /// Find matching rule for an event.
    pub fn find_matching_rule(&self, event: &SystemEvent, severity: Severity) -> Option<&RemediationRule> {
        let event_type = event.event_type();

        for rule in &self.rules {
            if !rule.enabled {
                continue;
            }

            // Check event pattern match
            if rule.event_pattern != "*" && rule.event_pattern != event_type {
                continue;
            }

            // Check minimum severity
            if severity < rule.min_severity {
                continue;
            }

            // Check cooldown
            if self.history.in_cooldown(&rule.name, rule.cooldown_secs) {
                debug!(
                    "Rule '{}' in cooldown ({} secs remaining)",
                    rule.name,
                    self.history.remaining_cooldown(&rule.name, rule.cooldown_secs).unwrap_or(0)
                );
                continue;
            }

            // Check daily limit
            if self.history.exceeded_daily_limit(&rule.name, rule.max_triggers_per_day) {
                debug!(
                    "Rule '{}' exceeded daily limit ({}/{})",
                    rule.name,
                    self.history.daily_count(&rule.name),
                    rule.max_triggers_per_day
                );
                continue;
            }

            return Some(rule);
        }

        None
    }

    /// Record a rule trigger (updates cooldowns and daily counts).
    pub fn record_trigger(&mut self, rule_name: &str) {
        self.history.record_trigger(rule_name);
    }

    /// Check if a command is blocked.
    pub fn is_command_blocked(&self, command: &str) -> bool {
        let cmd_lower = command.to_lowercase();
        self.command_blocklist.iter().any(|blocked| cmd_lower.contains(&blocked.to_lowercase()))
    }

    /// Convert a rule's action string to an ActionType.
    pub fn action_from_rule(&self, rule: &RemediationRule, event: &SystemEvent) -> Option<ActionType> {
        match rule.action.as_str() {
            "restart_container" => {
                let name = match event {
                    SystemEvent::ContainerCrashed { name, .. } => name.clone(),
                    SystemEvent::ServiceUnreachable { service, .. } => service.clone(),
                    _ => return None,
                };
                Some(ActionType::RestartContainer { name })
            }
            "rotate_logs" => Some(ActionType::RotateLogs),
            "trigger_backup" => Some(ActionType::TriggerBackup),
            "self_update" => Some(ActionType::SelfUpdate),
            "notify_user" => Some(ActionType::NotifyUser {
                message: event.description(),
                urgency: crate::types::NotifyUrgency::Medium,
            }),
            "escalate" => Some(ActionType::EscalateToHuman {
                reason: event.description(),
            }),
            _ => {
                // Treat unknown actions as custom commands
                if !self.is_command_blocked(&rule.action) {
                    Some(ActionType::RunCommand {
                        command: rule.action.clone(),
                        args: vec![],
                        timeout_secs: 60,
                    })
                } else {
                    None
                }
            }
        }
    }

    /// Get trigger history.
    pub fn history(&self) -> &RuleTriggerHistory {
        &self.history
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rule_matching() {
        let policy = RemediationPolicy::default();
        let event = SystemEvent::ContainerCrashed {
            name: "test".to_string(),
            exit_code: Some(1),
        };

        let rule = policy.find_matching_rule(&event, Severity::Critical);
        assert!(rule.is_some());
        assert_eq!(rule.unwrap().name, "container_crash_restart");
    }

    #[test]
    fn test_cooldown() {
        let mut policy = RemediationPolicy::default();
        let event = SystemEvent::ContainerCrashed {
            name: "test".to_string(),
            exit_code: Some(1),
        };

        // First trigger should work
        let rule = policy.find_matching_rule(&event, Severity::Critical);
        assert!(rule.is_some());

        // Record the trigger
        policy.record_trigger("container_crash_restart");

        // Second trigger should be blocked by cooldown
        let rule = policy.find_matching_rule(&event, Severity::Critical);
        assert!(rule.is_none());
    }

    #[test]
    fn test_command_blocklist() {
        let policy = RemediationPolicy::default();
        assert!(policy.is_command_blocked("rm -rf /"));
        assert!(policy.is_command_blocked("RM -RF /home"));
        assert!(!policy.is_command_blocked("ls -la"));
    }

    #[test]
    fn test_quiet_hours() {
        let mut policy = RemediationPolicy::default();
        policy.set_quiet_hours(2, 6);
        // We can't reliably test the actual time, but we can test the logic
        assert_eq!(policy.quiet_hours, (2, 6));
    }

    #[test]
    fn test_action_from_rule() {
        let policy = RemediationPolicy::default();
        let rule = &policy.rules[0]; // container_crash_restart
        let event = SystemEvent::ContainerCrashed {
            name: "nginx".to_string(),
            exit_code: Some(1),
        };

        let action = policy.action_from_rule(rule, &event);
        assert!(action.is_some());
        match action.unwrap() {
            ActionType::RestartContainer { name } => assert_eq!(name, "nginx"),
            _ => panic!("Wrong action type"),
        }
    }
}
