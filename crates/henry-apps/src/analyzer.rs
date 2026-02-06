//! Claude-powered repository analysis for multi-service app detection.

use crate::AppsError;
use henry_claude::ClaudeManager;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Manifest describing an analyzed repository.
#[derive(Debug, Clone)]
pub struct AppManifest {
    pub name: String,
    pub services: Vec<ServiceManifest>,
    pub is_monorepo: bool,
}

/// Manifest for a single service discovered in a repo.
#[derive(Debug, Clone)]
pub struct ServiceManifest {
    pub name: String,
    pub service_type: String,
    pub path: String,
    pub framework: String,
    pub port: u16,
    pub depends_on: Vec<String>,
    pub env_vars: HashMap<String, String>,
}

/// Analyze a repository to discover services.
///
/// For single-project repos, returns a single-service manifest without calling Claude.
/// For monorepos, uses Claude to analyze the structure and discover services.
pub async fn analyze_repo(
    repo_dir: &Path,
    app_name: &str,
    claude: Option<Arc<RwLock<ClaudeManager>>>,
) -> Result<AppManifest, AppsError> {
    // Detect monorepo patterns
    let monorepo_dirs = detect_monorepo_structure(repo_dir);

    if monorepo_dirs.is_empty() {
        // Single-project at root
        info!("Single-project repository detected");
        let service_type = detect_service_type(repo_dir);
        let port = default_port_for_type(&service_type);
        let framework = detect_framework(repo_dir).unwrap_or_else(|| "unknown".to_string());

        return Ok(AppManifest {
            name: app_name.to_string(),
            services: vec![ServiceManifest {
                name: "app".to_string(),
                service_type,
                path: ".".to_string(),
                framework,
                port,
                depends_on: Vec::new(),
                env_vars: HashMap::new(),
            }],
            is_monorepo: false,
        });
    }

    info!(
        "Monorepo structure detected with {} potential service directories",
        monorepo_dirs.len()
    );

    // Try Claude-powered analysis for monorepos
    if let Some(ref claude_mgr) = claude {
        match analyze_with_claude(repo_dir, app_name, &monorepo_dirs, claude_mgr).await {
            Ok(manifest) => return Ok(manifest),
            Err(e) => {
                warn!("Claude analysis failed, falling back to heuristic: {}", e);
            }
        }
    }

    // Fallback: heuristic-based analysis
    analyze_heuristic(repo_dir, app_name, &monorepo_dirs)
}

/// Detect monorepo directory structure.
fn detect_monorepo_structure(repo_dir: &Path) -> Vec<String> {
    let mut service_dirs = Vec::new();

    // Check common monorepo patterns
    let patterns = [
        "apps", "packages", "services", "modules", "projects",
    ];

    for pattern in &patterns {
        let dir = repo_dir.join(pattern);
        if dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.filter_map(|e| e.ok()) {
                    if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        let name = entry.file_name().to_string_lossy().to_string();
                        // Skip hidden dirs and common non-service dirs
                        if !name.starts_with('.') && name != "node_modules" && name != "__pycache__" {
                            let subdir = format!("{}/{}", pattern, name);
                            // Verify it looks like a project (has some config file)
                            if has_project_indicators(&entry.path()) {
                                service_dirs.push(subdir);
                            }
                        }
                    }
                }
            }
        }
    }

    // Also check workspace config files
    if repo_dir.join("pnpm-workspace.yaml").exists()
        || repo_dir.join("lerna.json").exists()
        || repo_dir.join("nx.json").exists()
    {
        // Already detected via directory scanning above
        debug!("Workspace config file found");
    }

    service_dirs
}

/// Check if a directory has project indicators (config files, etc.).
fn has_project_indicators(dir: &Path) -> bool {
    let indicators = [
        "package.json",
        "requirements.txt",
        "pyproject.toml",
        "Cargo.toml",
        "go.mod",
        "Gemfile",
        "composer.json",
        "pom.xml",
        "build.gradle",
        "Dockerfile",
        "main.py",
        "app.py",
        "index.js",
        "index.ts",
        "main.go",
    ];

    indicators.iter().any(|f| dir.join(f).exists())
}

/// Detect the service type (frontend/backend/worker) from directory contents.
fn detect_service_type(dir: &Path) -> String {
    // Frontend indicators
    let frontend_indicators = [
        "src/App.tsx",
        "src/App.jsx",
        "src/App.js",
        "src/main.tsx",
        "src/main.jsx",
        "public/index.html",
        "next.config.js",
        "next.config.mjs",
        "nuxt.config.js",
        "nuxt.config.ts",
        "vite.config.ts",
        "vite.config.js",
        "angular.json",
    ];

    if frontend_indicators.iter().any(|f| dir.join(f).exists()) {
        return "frontend".to_string();
    }

    // Backend indicators
    let backend_indicators = [
        "manage.py",
        "app.py",
        "main.py",
        "server.py",
        "server.js",
        "server.ts",
        "main.go",
        "cmd/",
        "api/",
    ];

    if backend_indicators.iter().any(|f| dir.join(f).exists()) {
        return "backend".to_string();
    }

    // Check package.json for clues
    if let Ok(content) = std::fs::read_to_string(dir.join("package.json")) {
        let lower = content.to_lowercase();
        if lower.contains("react") || lower.contains("vue") || lower.contains("svelte") || lower.contains("angular") {
            return "frontend".to_string();
        }
        if lower.contains("express") || lower.contains("fastify") || lower.contains("koa") || lower.contains("hapi") {
            return "backend".to_string();
        }
    }

    // Check pyproject.toml / requirements.txt for clues
    for file in &["requirements.txt", "pyproject.toml"] {
        if let Ok(content) = std::fs::read_to_string(dir.join(file)) {
            let lower = content.to_lowercase();
            if lower.contains("fastapi") || lower.contains("flask") || lower.contains("django") {
                return "backend".to_string();
            }
        }
    }

    "backend".to_string()
}

/// Detect the framework from directory contents.
fn detect_framework(dir: &Path) -> Option<String> {
    if dir.join("next.config.js").exists() || dir.join("next.config.mjs").exists() {
        return Some("nextjs".to_string());
    }
    if dir.join("vite.config.ts").exists() || dir.join("vite.config.js").exists() {
        if let Ok(content) = std::fs::read_to_string(dir.join("package.json")) {
            if content.contains("react") {
                return Some("react".to_string());
            }
            if content.contains("vue") {
                return Some("vue".to_string());
            }
        }
        return Some("vite".to_string());
    }

    if let Ok(content) = std::fs::read_to_string(dir.join("package.json")) {
        if content.contains("\"react\"") {
            return Some("react".to_string());
        }
        if content.contains("express") {
            return Some("express".to_string());
        }
    }

    for file in &["requirements.txt", "pyproject.toml"] {
        if let Ok(content) = std::fs::read_to_string(dir.join(file)) {
            let lower = content.to_lowercase();
            if lower.contains("fastapi") {
                return Some("fastapi".to_string());
            }
            if lower.contains("flask") {
                return Some("flask".to_string());
            }
            if lower.contains("django") {
                return Some("django".to_string());
            }
        }
    }

    if dir.join("Cargo.toml").exists() {
        return Some("rust".to_string());
    }
    if dir.join("go.mod").exists() {
        return Some("go".to_string());
    }

    None
}

/// Get a sensible default port for a service type/framework.
fn default_port_for_type(service_type: &str) -> u16 {
    match service_type {
        "frontend" => 3000,
        "backend" => 8000,
        "worker" => 8080,
        _ => 8080,
    }
}

/// Analyze repo using Claude API/CLI.
async fn analyze_with_claude(
    repo_dir: &Path,
    app_name: &str,
    service_dirs: &[String],
    claude: &Arc<RwLock<ClaudeManager>>,
) -> Result<AppManifest, AppsError> {
    // Gather directory tree info
    let mut tree_info = String::new();
    tree_info.push_str("Repository structure:\n");

    // Top-level files
    if let Ok(entries) = std::fs::read_dir(repo_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with('.') && name != "node_modules" {
                let kind = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    "/"
                } else {
                    ""
                };
                tree_info.push_str(&format!("  {}{}\n", name, kind));
            }
        }
    }

    // Service directory details
    for sd in service_dirs {
        let dir = repo_dir.join(sd);
        tree_info.push_str(&format!("\n{}/ contents:\n", sd));

        if let Ok(entries) = std::fs::read_dir(&dir) {
            let files: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    !name.starts_with('.') && name != "node_modules" && name != "__pycache__"
                })
                .map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let kind = if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        "/"
                    } else {
                        ""
                    };
                    format!("{}{}", name, kind)
                })
                .collect();
            tree_info.push_str(&format!("  {}\n", files.join(", ")));
        }

        // Include key config files
        for config_file in &["package.json", "requirements.txt", "pyproject.toml", "Cargo.toml", "go.mod"] {
            let path = dir.join(config_file);
            if path.exists() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let truncated = if content.len() > 800 {
                        format!("{}...(truncated)", &content[..800])
                    } else {
                        content
                    };
                    tree_info.push_str(&format!("\n{}/{}:\n{}\n", sd, config_file, truncated));
                }
            }
        }
    }

    let prompt = format!(
        r#"Analyze this monorepo and identify deployable services. Return ONLY valid JSON, no explanations.

{tree_info}

Return a JSON object with this exact schema:
{{
  "services": [
    {{
      "name": "string (short identifier like 'api', 'webapp', 'worker')",
      "service_type": "frontend|backend|worker",
      "path": "string (subdirectory relative to repo root, e.g. 'apps/api')",
      "framework": "string (e.g. 'fastapi', 'react', 'express')",
      "port": number (default port for this framework),
      "depends_on": ["list of other service names this depends on"],
      "env_vars": {{"KEY": "VALUE"}}
    }}
  ]
}}

Rules:
- Only include directories that contain actual deployable services
- Frontend services should depend on backend services
- Use standard ports: React/Next=3000, FastAPI/Flask=8000, Express=3001, Django=8000
- Keep env_vars minimal (only what's needed for inter-service communication)"#
    );

    let manager = claude.read().await;

    let response = if manager.has_api_client() {
        info!("Using Claude API for repo analysis");
        manager
            .ask(&prompt)
            .await
            .map_err(|e| AppsError::Build(format!("Claude analysis failed: {}", e)))?
    } else {
        drop(manager);
        // Fallback to CLI
        info!("Using Claude CLI for repo analysis");
        let output = tokio::process::Command::new("claude")
            .args(["--print", "-p", &prompt])
            .output()
            .await
            .map_err(|e| AppsError::Build(format!("Claude CLI failed: {}", e)))?;

        if !output.status.success() {
            return Err(AppsError::Build("Claude CLI analysis failed".to_string()));
        }
        String::from_utf8_lossy(&output.stdout).to_string()
    };

    parse_manifest_response(&response, app_name, repo_dir)
}

/// Parse Claude's JSON response into an AppManifest.
fn parse_manifest_response(
    response: &str,
    app_name: &str,
    repo_dir: &Path,
) -> Result<AppManifest, AppsError> {
    // Extract JSON from response (handle potential markdown code blocks)
    let json_str = response
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let parsed: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| AppsError::Build(format!("Failed to parse Claude response as JSON: {}", e)))?;

    let services_arr = parsed
        .get("services")
        .and_then(|v| v.as_array())
        .ok_or_else(|| AppsError::Build("Missing 'services' array in response".to_string()))?;

    let mut services = Vec::new();

    for svc in services_arr {
        let name = svc.get("name").and_then(|v| v.as_str()).unwrap_or("app").to_string();
        let service_type = svc.get("service_type").and_then(|v| v.as_str()).unwrap_or("backend").to_string();
        let path = svc.get("path").and_then(|v| v.as_str()).unwrap_or(".").to_string();
        let framework = svc.get("framework").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
        let port = svc.get("port").and_then(|v| v.as_u64()).unwrap_or(8080) as u16;

        let depends_on: Vec<String> = svc
            .get("depends_on")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let env_vars: HashMap<String, String> = svc
            .get("env_vars")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        // Validate: path must exist
        let full_path = if path == "." {
            repo_dir.to_path_buf()
        } else {
            repo_dir.join(&path)
        };

        if !full_path.exists() {
            warn!("Service path '{}' does not exist, skipping", path);
            continue;
        }

        services.push(ServiceManifest {
            name,
            service_type,
            path,
            framework,
            port,
            depends_on,
            env_vars,
        });
    }

    if services.is_empty() {
        return Err(AppsError::Build("No valid services found in repository".to_string()));
    }

    // Validate unique names
    let names: Vec<&str> = services.iter().map(|s| s.name.as_str()).collect();
    let unique_names: std::collections::HashSet<&str> = names.iter().copied().collect();
    if names.len() != unique_names.len() {
        return Err(AppsError::Build("Duplicate service names detected".to_string()));
    }

    Ok(AppManifest {
        name: app_name.to_string(),
        services,
        is_monorepo: true,
    })
}

/// Heuristic-based analysis fallback (no Claude needed).
fn analyze_heuristic(
    repo_dir: &Path,
    app_name: &str,
    service_dirs: &[String],
) -> Result<AppManifest, AppsError> {
    let mut services = Vec::new();

    for sd in service_dirs {
        let dir = repo_dir.join(sd);
        let dir_name = sd.rsplit('/').next().unwrap_or(sd);
        let service_type = detect_service_type(&dir);
        let port = default_port_for_type(&service_type);
        let framework = detect_framework(&dir).unwrap_or_else(|| "unknown".to_string());

        services.push(ServiceManifest {
            name: dir_name.to_string(),
            service_type,
            path: sd.clone(),
            framework,
            port,
            depends_on: Vec::new(),
            env_vars: HashMap::new(),
        });
    }

    if services.is_empty() {
        return Err(AppsError::Build("No services found in repository".to_string()));
    }

    // Wire up basic dependencies: frontends depend on backends
    let backend_names: Vec<String> = services
        .iter()
        .filter(|s| s.service_type == "backend")
        .map(|s| s.name.clone())
        .collect();

    for svc in &mut services {
        if svc.service_type == "frontend" && !backend_names.is_empty() {
            svc.depends_on = backend_names.clone();
        }
    }

    Ok(AppManifest {
        name: app_name.to_string(),
        services,
        is_monorepo: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_manifest_response() {
        let temp_dir = tempfile::tempdir().unwrap();
        let api_dir = temp_dir.path().join("apps/api");
        let web_dir = temp_dir.path().join("apps/webapp");
        std::fs::create_dir_all(&api_dir).unwrap();
        std::fs::create_dir_all(&web_dir).unwrap();

        let response = r#"{"services": [
            {"name": "api", "service_type": "backend", "path": "apps/api", "framework": "fastapi", "port": 8000, "depends_on": [], "env_vars": {}},
            {"name": "webapp", "service_type": "frontend", "path": "apps/webapp", "framework": "react", "port": 3000, "depends_on": ["api"], "env_vars": {}}
        ]}"#;

        let manifest = parse_manifest_response(response, "testapp", temp_dir.path()).unwrap();
        assert_eq!(manifest.services.len(), 2);
        assert!(manifest.is_monorepo);
        assert_eq!(manifest.services[0].name, "api");
        assert_eq!(manifest.services[1].depends_on, vec!["api"]);
    }

    #[test]
    fn test_parse_manifest_with_code_blocks() {
        let temp_dir = tempfile::tempdir().unwrap();
        let app_dir = temp_dir.path().join("src");
        std::fs::create_dir_all(&app_dir).unwrap();

        let response = "```json\n{\"services\": [{\"name\": \"app\", \"service_type\": \"backend\", \"path\": \"src\", \"framework\": \"express\", \"port\": 3000}]}\n```";
        let manifest = parse_manifest_response(response, "test", temp_dir.path()).unwrap();
        assert_eq!(manifest.services.len(), 1);
    }

    #[test]
    fn test_detect_service_type_defaults_to_backend() {
        let temp_dir = tempfile::tempdir().unwrap();
        assert_eq!(detect_service_type(temp_dir.path()), "backend");
    }

    #[test]
    fn test_detect_service_type_frontend() {
        let temp_dir = tempfile::tempdir().unwrap();
        let src = temp_dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("App.tsx"), "export default function App() {}").unwrap();
        assert_eq!(detect_service_type(temp_dir.path()), "frontend");
    }
}
