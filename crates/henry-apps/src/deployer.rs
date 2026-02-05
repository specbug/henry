//! Container deployment for apps.

use crate::types::{App, Service};
use crate::AppsError;
use henry_config::AppsConfig;
use henry_server::ContainerManager;
use std::collections::HashMap;
use tracing::{debug, info};

/// Deploy a container for an app (legacy single-service).
pub async fn deploy_container(
    app: &App,
    _containers: &ContainerManager,
    config: &AppsConfig,
) -> Result<(), AppsError> {
    deploy_container_inner(
        &app.container_name,
        &app.image_name,
        &app.id,
        &app.name,
        None,
        app.port,
        &app.env_vars,
        &HashMap::new(),
        config,
    )
    .await
}

/// Deploy a container for a specific service within a multi-service app.
pub async fn deploy_service_container(
    app_name: &str,
    app_id: &str,
    service: &Service,
    extra_env: &HashMap<String, String>,
    config: &AppsConfig,
) -> Result<(), AppsError> {
    deploy_container_inner(
        &service.container_name,
        &service.image_name,
        app_id,
        app_name,
        Some(&service.name),
        service.port,
        &service.env_vars,
        extra_env,
        config,
    )
    .await
}

/// Internal container deployment.
async fn deploy_container_inner(
    container_name: &str,
    image_name: &str,
    app_id: &str,
    app_name: &str,
    service_name: Option<&str>,
    port: u16,
    env_vars: &HashMap<String, String>,
    extra_env: &HashMap<String, String>,
    config: &AppsConfig,
) -> Result<(), AppsError> {
    info!(
        "Deploying container {} from image {}",
        container_name, image_name
    );

    // Build environment variables
    let mut env_list: Vec<String> = env_vars
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect();

    // Add extra env vars (inter-service wiring)
    for (k, v) in extra_env {
        env_list.push(format!("{}={}", k, v));
    }

    // Add default PORT env var
    env_list.push(format!("PORT={}", port));

    // Use docker CLI for container creation
    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        container_name.to_string(),
        "--network".to_string(),
        config.network_name.clone(),
        "--restart".to_string(),
        if config.auto_restart {
            "unless-stopped".to_string()
        } else {
            "no".to_string()
        },
    ];

    // Add labels
    args.extend([
        "--label".to_string(),
        format!("henry.app.id={}", app_id),
        "--label".to_string(),
        format!("henry.app.name={}", app_name),
        "--label".to_string(),
        "henry.managed=true".to_string(),
    ]);

    if let Some(sn) = service_name {
        args.extend([
            "--label".to_string(),
            format!("henry.service.name={}", sn),
        ]);
    }

    // Add environment variables
    for env in &env_list {
        args.extend(["-e".to_string(), env.clone()]);
    }

    // Add image name
    args.push(image_name.to_string());

    debug!("Docker run args: {:?}", args);

    let output = tokio::process::Command::new("docker")
        .args(&args)
        .output()
        .await
        .map_err(|e| AppsError::Deploy(format!("Failed to execute docker run: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Check if container already exists
        if stderr.contains("is already in use") {
            info!("Container {} already exists, restarting", container_name);
            return restart_container(container_name).await;
        }

        return Err(AppsError::Deploy(format!(
            "Docker run failed: {}",
            stderr
        )));
    }

    info!("Container {} deployed successfully", container_name);
    Ok(())
}

/// Restart a container.
async fn restart_container(name: &str) -> Result<(), AppsError> {
    let output = tokio::process::Command::new("docker")
        .args(["restart", name])
        .output()
        .await
        .map_err(|e| AppsError::Deploy(format!("Failed to restart container: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppsError::Deploy(format!(
            "Docker restart failed: {}",
            stderr
        )));
    }

    Ok(())
}

/// Stop a container.
pub async fn stop_container(name: &str) -> Result<(), AppsError> {
    let output = tokio::process::Command::new("docker")
        .args(["stop", name])
        .output()
        .await
        .map_err(|e| AppsError::Deploy(format!("Failed to stop container: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppsError::Deploy(format!("Docker stop failed: {}", stderr)));
    }

    Ok(())
}

/// Remove a container.
pub async fn remove_container(name: &str) -> Result<(), AppsError> {
    // Stop first
    stop_container(name).await.ok();

    let output = tokio::process::Command::new("docker")
        .args(["rm", "-f", name])
        .output()
        .await
        .map_err(|e| AppsError::Deploy(format!("Failed to remove container: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppsError::Deploy(format!("Docker rm failed: {}", stderr)));
    }

    Ok(())
}

/// Ensure the Docker network exists.
pub async fn ensure_network(network_name: &str) -> Result<(), AppsError> {
    // Check if network exists
    let output = tokio::process::Command::new("docker")
        .args(["network", "inspect", network_name])
        .output()
        .await
        .map_err(|e| AppsError::Deploy(format!("Failed to inspect network: {}", e)))?;

    if output.status.success() {
        return Ok(());
    }

    // Create network
    info!("Creating Docker network: {}", network_name);
    let output = tokio::process::Command::new("docker")
        .args(["network", "create", network_name])
        .output()
        .await
        .map_err(|e| AppsError::Deploy(format!("Failed to create network: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppsError::Deploy(format!(
            "Docker network create failed: {}",
            stderr
        )));
    }

    Ok(())
}

/// Get container health status.
pub async fn get_container_health(name: &str) -> Result<ContainerHealth, AppsError> {
    let output = tokio::process::Command::new("docker")
        .args([
            "inspect",
            "--format",
            "{{.State.Status}}|{{.State.Running}}|{{.State.Health.Status}}",
            name,
        ])
        .output()
        .await
        .map_err(|e| AppsError::Deploy(format!("Failed to inspect container: {}", e)))?;

    if !output.status.success() {
        return Ok(ContainerHealth {
            status: "not_found".to_string(),
            running: false,
            health: None,
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = stdout.trim().split('|').collect();

    Ok(ContainerHealth {
        status: parts.first().unwrap_or(&"unknown").to_string(),
        running: parts.get(1).map(|s| *s == "true").unwrap_or(false),
        health: parts.get(2).filter(|s| !s.is_empty()).map(|s| s.to_string()),
    })
}

/// Container health status.
#[derive(Debug, Clone)]
pub struct ContainerHealth {
    pub status: String,
    pub running: bool,
    pub health: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_get_nonexistent_container_health() {
        let health = get_container_health("nonexistent-container-xyz").await.unwrap();
        assert!(!health.running);
    }
}
