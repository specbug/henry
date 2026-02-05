//! Caddyfile generation for dynamic reverse proxy configuration.

use crate::types::Route;
use std::collections::HashMap;

/// Generate a Caddyfile from a list of routes.
pub fn generate_caddyfile(routes: &[Route], acme_email: Option<&str>) -> String {
    let mut output = String::new();

    // Global options
    output.push_str("{\n");
    if let Some(email) = acme_email {
        output.push_str(&format!("\temail {}\n", email));
    }
    output.push_str("}\n\n");

    // Group routes by domain
    let mut by_domain: HashMap<&str, Vec<&Route>> = HashMap::new();
    for route in routes {
        by_domain.entry(&route.domain).or_default().push(route);
    }

    // Generate site blocks for each domain
    for (domain, domain_routes) in by_domain {
        // Determine if any route has TLS enabled
        let has_tls = domain_routes.iter().any(|r| r.tls_enabled);

        // Site block header
        if has_tls {
            output.push_str(&format!("{} {{\n", domain));
        } else {
            output.push_str(&format!("http://{} {{\n", domain));
        }

        // Sort routes by path length (longer paths first for proper matching)
        let mut sorted_routes = domain_routes.clone();
        sorted_routes.sort_by(|a, b| b.path_prefix.len().cmp(&a.path_prefix.len()));

        for route in sorted_routes {
            if route.path_prefix == "/" {
                // Root path - no matcher needed
                output.push_str(&format!("\treverse_proxy {}\n", route.upstream));
            } else {
                // Path prefix handler
                let matcher_name = sanitize_matcher_name(&route.path_prefix);
                output.push_str(&format!("\t@{} path {}*\n", matcher_name, route.path_prefix));

                if route.strip_prefix {
                    output.push_str(&format!(
                        "\thandle @{} {{\n\t\turi strip_prefix {}\n\t\treverse_proxy {}\n\t}}\n",
                        matcher_name, route.path_prefix, route.upstream
                    ));
                } else {
                    output.push_str(&format!(
                        "\thandle @{} {{\n\t\treverse_proxy {}\n\t}}\n",
                        matcher_name, route.upstream
                    ));
                }
            }
        }

        output.push_str("}\n\n");
    }

    output
}

/// Sanitize a path to use as a Caddy matcher name.
fn sanitize_matcher_name(path: &str) -> String {
    path.trim_start_matches('/')
        .replace('/', "_")
        .replace('-', "_")
        .replace('.', "_")
}

/// Generate a minimal Caddyfile for health check.
pub fn generate_health_caddyfile(http_port: u16) -> String {
    format!(
        r#"{{
    auto_https off
}}

:{} {{
    respond /health "OK" 200
}}
"#,
        http_port
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_simple_caddyfile() {
        let routes = vec![Route::new("example.com", "/", "localhost:8080")];

        let caddyfile = generate_caddyfile(&routes, Some("admin@example.com"));
        assert!(caddyfile.contains("email admin@example.com"));
        assert!(caddyfile.contains("example.com {"));
        assert!(caddyfile.contains("reverse_proxy localhost:8080"));
    }

    #[test]
    fn test_generate_caddyfile_with_path() {
        let routes = vec![Route::new("example.com", "/app", "localhost:8080")];

        let caddyfile = generate_caddyfile(&routes, None);
        assert!(caddyfile.contains("@app path /app*"));
        assert!(caddyfile.contains("uri strip_prefix /app"));
        assert!(caddyfile.contains("reverse_proxy localhost:8080"));
    }

    #[test]
    fn test_generate_caddyfile_multiple_routes() {
        let routes = vec![
            Route::new("example.com", "/api", "api-service:3000"),
            Route::new("example.com", "/app", "app-service:8080"),
            Route::new("example.com", "/", "frontend:80"),
        ];

        let caddyfile = generate_caddyfile(&routes, None);
        // Longer paths should come first
        let api_pos = caddyfile.find("@api").unwrap_or(999999);
        let app_pos = caddyfile.find("@app").unwrap_or(999999);
        let root_pos = caddyfile
            .find("reverse_proxy frontend:80")
            .unwrap_or(999999);
        assert!(api_pos < root_pos || app_pos < root_pos);
    }

    #[test]
    fn test_sanitize_matcher_name() {
        assert_eq!(sanitize_matcher_name("/my-app"), "my_app");
        assert_eq!(sanitize_matcher_name("/api/v1"), "api_v1");
        assert_eq!(sanitize_matcher_name("/my.app"), "my_app");
    }

    #[test]
    fn test_generate_health_caddyfile() {
        let caddyfile = generate_health_caddyfile(80);
        assert!(caddyfile.contains(":80"));
        assert!(caddyfile.contains("/health"));
        assert!(caddyfile.contains("auto_https off"));
    }
}
