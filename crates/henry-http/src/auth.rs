//! Authentication middleware for HTTP API.
//!
//! Supports Bearer token authentication with timing-safe comparison.

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use subtle::ConstantTimeEq;
use tracing::warn;

use crate::AppState;

/// Authentication middleware.
///
/// Checks for Bearer token in Authorization header.
/// Returns 401 if token is missing or invalid.
pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let Some(expected_token) = &state.api_token else {
        // No token configured, allow all requests
        return next.run(request).await;
    };

    // Check for local requests (skip auth for localhost)
    if is_local_request(&request) {
        return next.run(request).await;
    }

    // Extract Authorization header
    let auth_header = request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok());

    let provided_token = match auth_header {
        Some(header) if header.starts_with("Bearer ") => &header[7..],
        _ => {
            warn!("Missing or invalid Authorization header");
            return (StatusCode::UNAUTHORIZED, "Missing or invalid Authorization header")
                .into_response();
        }
    };

    // Timing-safe comparison
    if !verify_token(provided_token, expected_token) {
        warn!("Invalid API token provided");
        return (StatusCode::UNAUTHORIZED, "Invalid API token").into_response();
    }

    next.run(request).await
}

/// Check if request is from localhost.
fn is_local_request(request: &Request<Body>) -> bool {
    // Check X-Forwarded-For first (reverse proxy)
    if let Some(forwarded) = request
        .headers()
        .get("X-Forwarded-For")
        .and_then(|v| v.to_str().ok())
    {
        let first_ip = forwarded.split(',').next().unwrap_or("").trim();
        return is_local_ip(first_ip);
    }

    // Fall back to connection info (would need to be passed via extension)
    // For now, we don't skip auth for non-forwarded requests
    false
}

fn is_local_ip(ip: &str) -> bool {
    ip == "127.0.0.1" || ip == "::1" || ip.starts_with("192.168.") || ip.starts_with("10.")
}

/// Timing-safe token comparison.
fn verify_token(provided: &str, expected: &str) -> bool {
    let provided_bytes = provided.as_bytes();
    let expected_bytes = expected.as_bytes();

    // Length check must also be constant-time
    if provided_bytes.len() != expected_bytes.len() {
        // Still perform comparison to maintain constant time
        let _ = provided_bytes.ct_eq(&vec![0u8; provided_bytes.len()]);
        return false;
    }

    provided_bytes.ct_eq(expected_bytes).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_token() {
        assert!(verify_token("my-secret-token", "my-secret-token"));
        assert!(!verify_token("wrong-token", "my-secret-token"));
        assert!(!verify_token("my-secret-token!", "my-secret-token"));
        assert!(!verify_token("my-secret-toke", "my-secret-token"));
    }

    #[test]
    fn test_is_local_ip() {
        assert!(is_local_ip("127.0.0.1"));
        assert!(is_local_ip("::1"));
        assert!(is_local_ip("192.168.1.100"));
        assert!(is_local_ip("10.0.0.1"));
        assert!(!is_local_ip("8.8.8.8"));
        assert!(!is_local_ip("203.0.113.1"));
    }
}
