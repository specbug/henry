//! Docker image building for apps.

use crate::AppsError;
use henry_claude::ClaudeManager;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

/// Generate a Dockerfile using Claude AI.
/// Tries Claude API first (if configured), then falls back to Claude CLI.
///
/// `service_dir`: Optional subdirectory relative to repo root for monorepo services.
/// When set, detection and file gathering happen in the subdirectory, and the
/// Claude prompt includes monorepo-aware instructions.
pub async fn generate_dockerfile(
    repo_dir: &Path,
    service_dir: Option<&str>,
    claude: Option<Arc<RwLock<ClaudeManager>>>,
) -> Result<String, AppsError> {
    let target_dir = match service_dir {
        Some(sd) => repo_dir.join(sd),
        None => repo_dir.to_path_buf(),
    };

    // First try to detect project type and use template
    if let Some(project_type) = crate::git::detect_project_type(&target_dir) {
        info!("Detected project type: {:?}", project_type);
        return Ok(project_type.dockerfile_template().to_string());
    }

    // Read files to understand the project
    let files_info = gather_project_info(&target_dir)?;

    let monorepo_context = if let Some(sd) = service_dir {
        format!(
            "\n\nIMPORTANT: This is a monorepo. The service is located at: {}/\n\
             Docker build context is the REPO ROOT. All COPY paths must be relative to repo root.\n\
             Example: COPY {}/requirements.txt ./requirements.txt\n\
             Example: COPY {}/package.json ./package.json\n",
            sd, sd, sd
        )
    } else {
        String::new()
    };

    let prompt = format!(
        r#"Generate a Dockerfile for this project. The project directory contains:

{}{}

Requirements:
1. Use multi-stage builds where appropriate
2. Minimize image size
3. Use specific version tags (not latest)
4. The app should listen on port 8080
5. Include health check if possible
6. Follow security best practices

Return ONLY the Dockerfile content, no explanations or markdown code blocks."#,
        files_info, monorepo_context
    );

    // Try Claude API if available
    if let Some(ref claude_mgr) = claude {
        let manager = claude_mgr.read().await;
        if manager.has_api_client() {
            info!("Using Claude API for Dockerfile generation");
            let response = manager
                .ask(&prompt)
                .await
                .map_err(|e| AppsError::DockerfileGeneration(e.to_string()))?;
            return parse_dockerfile_response(&response);
        }
    }

    // Fall back to Claude CLI (works with Claude Max subscription)
    info!("Using Claude CLI for Dockerfile generation");
    generate_dockerfile_with_cli(&prompt).await
}

/// Generate Dockerfile using Claude CLI (no API key needed, uses session token).
async fn generate_dockerfile_with_cli(prompt: &str) -> Result<String, AppsError> {
    // Check if claude CLI is available
    let check = tokio::process::Command::new("which")
        .arg("claude")
        .output()
        .await;

    if check.map(|o| !o.status.success()).unwrap_or(true) {
        return Err(AppsError::DockerfileGeneration(
            "Claude CLI not found. Install with: npm install -g @anthropic-ai/claude-code".to_string(),
        ));
    }

    // Use claude CLI with --print flag for non-interactive output
    let output = tokio::process::Command::new("claude")
        .args(["--print", "-p", prompt])
        .output()
        .await
        .map_err(|e| AppsError::DockerfileGeneration(format!("Failed to run claude CLI: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppsError::DockerfileGeneration(format!(
            "Claude CLI failed: {}",
            stderr
        )));
    }

    let response = String::from_utf8_lossy(&output.stdout).to_string();
    parse_dockerfile_response(&response)
}

/// Parse and validate Dockerfile from Claude response.
fn parse_dockerfile_response(response: &str) -> Result<String, AppsError> {
    let dockerfile = response
        .trim()
        .trim_start_matches("```dockerfile")
        .trim_start_matches("```Dockerfile")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
        .to_string();

    if dockerfile.is_empty() || !dockerfile.to_uppercase().contains("FROM") {
        return Err(AppsError::DockerfileGeneration(
            "Claude did not generate a valid Dockerfile".to_string(),
        ));
    }

    Ok(dockerfile)
}

/// Gather project information for Claude.
fn gather_project_info(app_dir: &Path) -> Result<String, AppsError> {
    let mut info = Vec::new();

    // List top-level files
    if let Ok(entries) = std::fs::read_dir(app_dir) {
        let files: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter_map(|e| e.file_name().to_str().map(String::from))
            .collect();

        info.push(format!("Files: {}", files.join(", ")));
    }

    // Read package.json if exists
    let package_json = app_dir.join("package.json");
    if package_json.exists() {
        if let Ok(content) = std::fs::read_to_string(&package_json) {
            info.push(format!("package.json:\n{}", truncate_content(&content, 1000)));
        }
    }

    // Read requirements.txt if exists
    let requirements = app_dir.join("requirements.txt");
    if requirements.exists() {
        if let Ok(content) = std::fs::read_to_string(&requirements) {
            info.push(format!(
                "requirements.txt:\n{}",
                truncate_content(&content, 500)
            ));
        }
    }

    // Read Cargo.toml if exists
    let cargo_toml = app_dir.join("Cargo.toml");
    if cargo_toml.exists() {
        if let Ok(content) = std::fs::read_to_string(&cargo_toml) {
            info.push(format!("Cargo.toml:\n{}", truncate_content(&content, 500)));
        }
    }

    // Read go.mod if exists
    let go_mod = app_dir.join("go.mod");
    if go_mod.exists() {
        if let Ok(content) = std::fs::read_to_string(&go_mod) {
            info.push(format!("go.mod:\n{}", truncate_content(&content, 500)));
        }
    }

    Ok(info.join("\n\n"))
}

/// Truncate content to a maximum length.
fn truncate_content(content: &str, max_len: usize) -> String {
    if content.len() <= max_len {
        content.to_string()
    } else {
        format!("{}...(truncated)", &content[..max_len])
    }
}

/// Build a Docker image from a directory.
///
/// `service_dir`: Optional subdirectory for monorepo services. When set, the Dockerfile
/// is expected at `{service_dir}/Dockerfile` and the build context is the repo root.
pub async fn build_image(
    repo_dir: &Path,
    service_dir: Option<&str>,
    image_name: &str,
    _containers: &henry_server::ContainerManager,
) -> Result<(), AppsError> {
    info!("Building image {} from {}", image_name, repo_dir.display());

    let output = match service_dir {
        Some(sd) => {
            let dockerfile_path = format!("{}/Dockerfile", sd);
            info!("Using Dockerfile at {} with repo root as build context", dockerfile_path);
            tokio::process::Command::new("docker")
                .current_dir(repo_dir)
                .args(["build", "-t", image_name, "-f", &dockerfile_path, "."])
                .output()
                .await
        }
        None => {
            tokio::process::Command::new("docker")
                .current_dir(repo_dir)
                .args(["build", "-t", image_name, "."])
                .output()
                .await
        }
    }
    .map_err(|e| AppsError::Build(format!("Failed to execute docker build: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppsError::Build(format!("Docker build failed: {}", stderr)));
    }

    debug!("Image {} built successfully", image_name);
    Ok(())
}

/// Check if an image exists locally.
pub async fn image_exists(image_name: &str) -> bool {
    let output = tokio::process::Command::new("docker")
        .args(["image", "inspect", image_name])
        .output()
        .await;

    output.map(|o| o.status.success()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_content() {
        assert_eq!(truncate_content("hello", 10), "hello");
        assert_eq!(truncate_content("hello world", 5), "hello...(truncated)");
    }
}
