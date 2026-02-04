//! Authentication utilities for Telegram bot.
//!
//! Uses timing-safe comparison for security.

use subtle::ConstantTimeEq;

/// Check if a user is authorized to use the bot.
///
/// Authorization is checked in order:
/// 1. User ID allowlist (faster, numeric comparison)
/// 2. Username allowlist (timing-safe string comparison)
pub fn is_authorized(
    user_id: i64,
    username: Option<&str>,
    allowed_ids: &[i64],
    allowed_usernames: &[String],
) -> bool {
    // Check user ID first (faster)
    if allowed_ids.contains(&user_id) {
        return true;
    }

    // Check username with timing-safe comparison
    if let Some(username) = username {
        for allowed in allowed_usernames {
            if constant_time_str_eq(username, allowed) {
                return true;
            }
        }
    }

    false
}

/// Timing-safe string comparison to prevent timing attacks.
fn constant_time_str_eq(a: &str, b: &str) -> bool {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();

    // Length comparison must be constant-time
    if a_bytes.len() != b_bytes.len() {
        // Still do the comparison to maintain constant time
        let _ = a_bytes.ct_eq(&vec![0u8; a_bytes.len()]);
        return false;
    }

    a_bytes.ct_eq(b_bytes).into()
}

/// Timing-safe token comparison.
#[allow(dead_code)]
pub fn verify_token(provided: &str, expected: &str) -> bool {
    constant_time_str_eq(provided, expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_id_auth() {
        let allowed_ids = vec![12345, 67890];
        let allowed_usernames = vec![];

        assert!(is_authorized(12345, None, &allowed_ids, &allowed_usernames));
        assert!(is_authorized(67890, Some("user"), &allowed_ids, &allowed_usernames));
        assert!(!is_authorized(99999, None, &allowed_ids, &allowed_usernames));
    }

    #[test]
    fn test_username_auth() {
        let allowed_ids = vec![];
        let allowed_usernames = vec!["admin".to_string(), "operator".to_string()];

        assert!(is_authorized(99999, Some("admin"), &allowed_ids, &allowed_usernames));
        assert!(is_authorized(99999, Some("operator"), &allowed_ids, &allowed_usernames));
        assert!(!is_authorized(99999, Some("hacker"), &allowed_ids, &allowed_usernames));
        assert!(!is_authorized(99999, None, &allowed_ids, &allowed_usernames));
    }

    #[test]
    fn test_combined_auth() {
        let allowed_ids = vec![12345];
        let allowed_usernames = vec!["admin".to_string()];

        // ID match
        assert!(is_authorized(12345, Some("unknown"), &allowed_ids, &allowed_usernames));
        // Username match
        assert!(is_authorized(99999, Some("admin"), &allowed_ids, &allowed_usernames));
        // No match
        assert!(!is_authorized(99999, Some("hacker"), &allowed_ids, &allowed_usernames));
    }

    #[test]
    fn test_verify_token() {
        assert!(verify_token("secret123", "secret123"));
        assert!(!verify_token("secret123", "secret124"));
        assert!(!verify_token("secret123", "secret12"));
        assert!(!verify_token("secret12", "secret123"));
    }

    #[test]
    fn test_empty_allowlists() {
        let allowed_ids = vec![];
        let allowed_usernames = vec![];

        assert!(!is_authorized(12345, Some("user"), &allowed_ids, &allowed_usernames));
    }
}
