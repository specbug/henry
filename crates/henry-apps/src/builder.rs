//! Docker image building for apps.

use crate::AppsError;
use henry_claude::ClaudeManager;
use std::collections::HashMap;
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
        let template = project_type.dockerfile_template().to_string();
        // For monorepo services, rewrite COPY/ADD paths since build context is repo root
        if let Some(sd) = service_dir {
            return Ok(rewrite_dockerfile_paths(&template, sd));
        }
        return Ok(template);
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

/// Rewrite COPY/ADD source paths in a Dockerfile to prepend the service directory.
/// Used for monorepo services where the build context is the repo root but the
/// Dockerfile template assumes sources are in the current directory.
fn rewrite_dockerfile_paths(dockerfile: &str, service_dir: &str) -> String {
    let sd = service_dir.trim_end_matches('/');
    dockerfile
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            let is_copy = trimmed.starts_with("COPY ");
            let is_add = trimmed.starts_with("ADD ");

            if !is_copy && !is_add {
                return line.to_string();
            }

            // Skip --from=builder style multi-stage copies
            if trimmed.contains("--from=") {
                return line.to_string();
            }

            let prefix = if is_copy { "COPY " } else { "ADD " };
            let rest = trimmed.strip_prefix(prefix).unwrap();

            // Split into parts; last part is destination
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() < 2 {
                return line.to_string();
            }

            let dest = parts.last().unwrap();
            let sources = &parts[..parts.len() - 1];

            let new_sources: Vec<String> = sources
                .iter()
                .map(|src| {
                    // Skip sources that start with -- (flags like --chown)
                    if src.starts_with("--") {
                        return src.to_string();
                    }
                    // Don't rewrite absolute paths or already-prefixed paths
                    if src.starts_with('/') || src.starts_with(sd) {
                        return src.to_string();
                    }
                    // "." or "./" means copy the whole service dir
                    if *src == "." || *src == "./" {
                        return format!("{}/", sd);
                    }
                    format!("{}/{}", sd, src)
                })
                .collect();

            format!("{}{} {}", prefix, new_sources.join(" "), dest)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Truncate content to a maximum length.
fn truncate_content(content: &str, max_len: usize) -> String {
    if content.len() <= max_len {
        content.to_string()
    } else {
        format!("{}...(truncated)", &content[..max_len])
    }
}

/// Build a container image from a directory.
///
/// `service_dir`: Optional subdirectory for monorepo services. When set, the Dockerfile
/// is expected at `{service_dir}/Dockerfile` and the build context is the repo root.
/// `build_args`: Optional build-time variables (e.g., PUBLIC_URL for frontends).
pub async fn build_image(
    repo_dir: &Path,
    service_dir: Option<&str>,
    image_name: &str,
    _containers: &henry_server::ContainerManager,
    runtime_cmd: &str,
    build_args: &HashMap<String, String>,
) -> Result<(), AppsError> {
    info!("Building image {} from {}", image_name, repo_dir.display());

    let mut args = vec!["build".to_string(), "-t".to_string(), image_name.to_string()];

    // Add build args
    for (key, value) in build_args {
        args.push("--build-arg".to_string());
        args.push(format!("{}={}", key, value));
    }

    if let Some(sd) = service_dir {
        let dockerfile_path = format!("{}/Dockerfile", sd);
        info!("Using Dockerfile at {} with repo root as build context", dockerfile_path);
        args.extend(["-f".to_string(), dockerfile_path]);
    }

    args.push(".".to_string());

    let output = tokio::process::Command::new(runtime_cmd)
        .current_dir(repo_dir)
        .args(&args)
        .output()
        .await
        .map_err(|e| AppsError::Build(format!("Failed to execute {} build: {}", runtime_cmd, e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppsError::Build(format!("{} build failed: {}", runtime_cmd, stderr)));
    }

    debug!("Image {} built successfully", image_name);
    Ok(())
}

/// Check if an image exists locally.
pub async fn image_exists(image_name: &str, runtime_cmd: &str) -> bool {
    let output = tokio::process::Command::new(runtime_cmd)
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

    #[test]
    fn test_rewrite_dockerfile_paths() {
        let template = "\
FROM python:3.12-slim
WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
COPY . .
ENV PYTHONUNBUFFERED=1
EXPOSE 8080
CMD [\"python\", \"app.py\"]";

        let result = rewrite_dockerfile_paths(template, "apps/api");
        assert!(result.contains("COPY apps/api/requirements.txt ."));
        assert!(result.contains("COPY apps/api/ ."));
        assert!(result.contains("FROM python:3.12-slim"));
        assert!(result.contains("RUN pip install"));
    }

    #[test]
    fn test_rewrite_dockerfile_paths_multistage() {
        let template = "\
FROM rust:1.75 AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/
RUN cargo build --release
FROM debian:bookworm-slim
COPY --from=builder /app/target/release/myapp /usr/local/bin/";

        let result = rewrite_dockerfile_paths(template, "services/backend");
        assert!(result.contains("COPY services/backend/Cargo.toml services/backend/Cargo.lock ./"));
        assert!(result.contains("COPY services/backend/src/ ./src/"));
        // --from= copies should NOT be rewritten
        assert!(result.contains("COPY --from=builder /app/target/release/myapp /usr/local/bin/"));
    }
}
