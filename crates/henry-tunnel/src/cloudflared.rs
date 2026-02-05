//! cloudflared container management utilities.

use crate::TunnelError;
use tracing::{debug, info};

/// Get cloudflared logs from container.
pub async fn get_logs(container_name: &str, tail: usize) -> Result<String, TunnelError> {
    let output = tokio::process::Command::new("docker")
        .args(["logs", "--tail", &tail.to_string(), container_name])
        .output()
        .await
        .map_err(|e| TunnelError::Io(e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TunnelError::CloudflareApi(format!(
            "Failed to get logs: {}",
            stderr
        )));
    }

    // Combine stdout and stderr (cloudflared logs to stderr)
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(format!("{}{}", stdout, stderr))
}

/// Check if cloudflared is healthy.
pub async fn check_health(container_name: &str) -> Result<bool, TunnelError> {
    let output = tokio::process::Command::new("docker")
        .args([
            "inspect",
            "--format",
            "{{.State.Running}}",
            container_name,
        ])
        .output()
        .await
        .map_err(|e| TunnelError::Io(e))?;

    if !output.status.success() {
        return Ok(false);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.trim() == "true")
}

/// Get tunnel connection status from logs.
pub async fn get_connection_status(container_name: &str) -> Result<TunnelConnectionStatus, TunnelError> {
    let logs = get_logs(container_name, 50).await?;

    let connected = logs.contains("Connection") && logs.contains("registered");
    let reconnecting = logs.contains("Retrying") || logs.contains("reconnect");
    let error = logs.lines().filter(|l| l.contains("ERR")).last().map(String::from);

    Ok(TunnelConnectionStatus {
        connected,
        reconnecting,
        last_error: error,
    })
}

/// Tunnel connection status.
#[derive(Debug, Clone)]
pub struct TunnelConnectionStatus {
    pub connected: bool,
    pub reconnecting: bool,
    pub last_error: Option<String>,
}

/// Generate cloudflared config YAML for ingress rules.
pub fn generate_config_yaml(ingress_rules: &[IngressRule], tunnel_id: &str) -> String {
    let mut yaml = String::new();

    yaml.push_str(&format!("tunnel: {}\n", tunnel_id));
    yaml.push_str("credentials-file: /etc/cloudflared/credentials.json\n\n");
    yaml.push_str("ingress:\n");

    for rule in ingress_rules {
        match &rule.path {
            Some(path) => {
                yaml.push_str(&format!(
                    "  - hostname: {}\n    path: {}\n    service: {}\n",
                    rule.hostname, path, rule.service
                ));
            }
            None => {
                yaml.push_str(&format!(
                    "  - hostname: {}\n    service: {}\n",
                    rule.hostname, rule.service
                ));
            }
        }
    }

    // Catch-all rule (required)
    yaml.push_str("  - service: http_status:404\n");

    yaml
}

/// Ingress rule for cloudflared config.
#[derive(Debug, Clone)]
pub struct IngressRule {
    pub hostname: String,
    pub path: Option<String>,
    pub service: String,
}

impl IngressRule {
    pub fn new(hostname: impl Into<String>, service: impl Into<String>) -> Self {
        Self {
            hostname: hostname.into(),
            path: None,
            service: service.into(),
        }
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_config_yaml() {
        let rules = vec![
            IngressRule::new("app.example.com", "http://app:8080"),
            IngressRule::new("api.example.com", "http://api:3000").with_path("/v1/*"),
        ];

        let yaml = generate_config_yaml(&rules, "test-tunnel-id");

        assert!(yaml.contains("tunnel: test-tunnel-id"));
        assert!(yaml.contains("hostname: app.example.com"));
        assert!(yaml.contains("hostname: api.example.com"));
        assert!(yaml.contains("path: /v1/*"));
        assert!(yaml.contains("service: http_status:404"));
    }
}
