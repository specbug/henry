//! Git operations for app deployment.

use crate::AppsError;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, info};

/// Validate that a repository URL is allowed.
pub fn validate_repo_url(url: &str) -> Result<(), AppsError> {
    // Only allow HTTPS URLs
    if !url.starts_with("https://") {
        return Err(AppsError::InvalidRepoUrl(
            "Only HTTPS URLs are allowed".to_string(),
        ));
    }

    // Must be from GitHub or GitLab
    let allowed_hosts = ["github.com", "gitlab.com", "bitbucket.org"];
    let is_allowed = allowed_hosts.iter().any(|host| url.contains(host));

    if !is_allowed {
        return Err(AppsError::InvalidRepoUrl(format!(
            "Repository must be from: {}",
            allowed_hosts.join(", ")
        )));
    }

    // Basic URL structure validation
    if !url.contains('/') || url.split('/').count() < 4 {
        return Err(AppsError::InvalidRepoUrl(
            "Invalid repository URL format".to_string(),
        ));
    }

    Ok(())
}

/// Clone a repository to a directory.
pub async fn clone_repo(url: &str, branch: &str, dest: &Path) -> Result<(), AppsError> {
    // Remove existing directory if it exists
    if dest.exists() {
        std::fs::remove_dir_all(dest).map_err(|e| AppsError::Git(e.to_string()))?;
    }

    info!("Cloning {} (branch: {}) to {}", url, branch, dest.display());

    let output = Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            "--branch",
            branch,
            url,
            dest.to_str().unwrap_or("."),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| AppsError::Git(format!("Failed to execute git: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppsError::Git(format!("Git clone failed: {}", stderr)));
    }

    debug!("Clone completed successfully");
    Ok(())
}

/// Pull latest changes in a repository.
pub async fn pull_repo(dir: &Path) -> Result<(), AppsError> {
    if !dir.exists() {
        return Err(AppsError::Git("Repository directory does not exist".to_string()));
    }

    info!("Pulling latest changes in {}", dir.display());

    let output = Command::new("git")
        .current_dir(dir)
        .args(["pull", "--rebase"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| AppsError::Git(format!("Failed to execute git: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppsError::Git(format!("Git pull failed: {}", stderr)));
    }

    debug!("Pull completed successfully");
    Ok(())
}

/// Get the current commit hash.
pub async fn get_commit_hash(dir: &Path) -> Result<String, AppsError> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "HEAD"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| AppsError::Git(format!("Failed to execute git: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppsError::Git(format!("Git rev-parse failed: {}", stderr)));
    }

    let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(hash)
}

/// Detect the main language/framework in a repository.
pub fn detect_project_type(dir: &Path) -> Option<ProjectType> {
    // Check for various configuration files
    if dir.join("package.json").exists() {
        if dir.join("next.config.js").exists() || dir.join("next.config.mjs").exists() {
            return Some(ProjectType::NextJs);
        }
        if dir.join("nuxt.config.js").exists() || dir.join("nuxt.config.ts").exists() {
            return Some(ProjectType::NuxtJs);
        }
        // Detect CRA (react-scripts) apps
        if let Ok(content) = std::fs::read_to_string(dir.join("package.json")) {
            if content.contains("react-scripts") {
                return Some(ProjectType::ReactCra);
            }
        }
        return Some(ProjectType::NodeJs);
    }

    if dir.join("requirements.txt").exists() || dir.join("pyproject.toml").exists() {
        if dir.join("manage.py").exists() {
            return Some(ProjectType::Django);
        }
        // Detect FastAPI from requirements
        for file in &["requirements.txt", "pyproject.toml"] {
            if let Ok(content) = std::fs::read_to_string(dir.join(file)) {
                let lower = content.to_lowercase();
                if lower.contains("fastapi") || lower.contains("uvicorn") {
                    return Some(ProjectType::FastApi);
                }
            }
        }
        return Some(ProjectType::Python);
    }

    if dir.join("Cargo.toml").exists() {
        return Some(ProjectType::Rust);
    }

    if dir.join("go.mod").exists() {
        return Some(ProjectType::Go);
    }

    if dir.join("pom.xml").exists() || dir.join("build.gradle").exists() {
        return Some(ProjectType::Java);
    }

    if dir.join("Gemfile").exists() {
        if dir.join("config/routes.rb").exists() {
            return Some(ProjectType::Rails);
        }
        return Some(ProjectType::Ruby);
    }

    if dir.join("composer.json").exists() {
        return Some(ProjectType::Php);
    }

    None
}

/// Project type detected from repository structure.
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectType {
    NodeJs,
    NextJs,
    NuxtJs,
    ReactCra,
    Python,
    FastApi,
    Django,
    Rust,
    Go,
    Java,
    Ruby,
    Rails,
    Php,
    Static,
}

impl ProjectType {
    /// Get a template Dockerfile for this project type.
    pub fn dockerfile_template(&self) -> &'static str {
        match self {
            Self::NodeJs => include_str!("../templates/dockerfile_nodejs.txt"),
            Self::NextJs => include_str!("../templates/dockerfile_nextjs.txt"),
            Self::ReactCra => include_str!("../templates/dockerfile_react_cra.txt"),
            Self::Python => include_str!("../templates/dockerfile_python.txt"),
            Self::FastApi => include_str!("../templates/dockerfile_fastapi.txt"),
            Self::Django => include_str!("../templates/dockerfile_django.txt"),
            Self::Rust => include_str!("../templates/dockerfile_rust.txt"),
            Self::Go => include_str!("../templates/dockerfile_go.txt"),
            _ => include_str!("../templates/dockerfile_generic.txt"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_repo_url_valid() {
        assert!(validate_repo_url("https://github.com/user/repo").is_ok());
        assert!(validate_repo_url("https://gitlab.com/user/repo").is_ok());
        assert!(validate_repo_url("https://bitbucket.org/user/repo").is_ok());
    }

    #[test]
    fn test_validate_repo_url_invalid() {
        assert!(validate_repo_url("http://github.com/user/repo").is_err());
        assert!(validate_repo_url("git@github.com:user/repo.git").is_err());
        assert!(validate_repo_url("https://example.com/user/repo").is_err());
    }
}
