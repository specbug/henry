//! Henry's own HTTP endpoint, on loopback.
//!
//! For now it serves two things: `/healthz`, and `/hooks/github`, where
//! GitHub reports pushes so a deploy starts within seconds rather than at the
//! next poll. Hinge exposes the hook path through the tunnel. Every delivery
//! must carry a valid HMAC signature made with `henry/GITHUB_WEBHOOK_SECRET`;
//! anything else is refused before its body is read as JSON.

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use canon::{Canon, Desired, serde_json};
use hmac::{Hmac, Mac};
use hush::{HOST, Hush};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tracing::{info, warn};

pub const WEBHOOK_SECRET: &str = "GITHUB_WEBHOOK_SECRET";

#[derive(Clone)]
struct Ctx {
    canon: Arc<Canon>,
    hush: Arc<Hush>,
}

pub async fn serve(listen: &str, canon: Arc<Canon>, hush: Arc<Hush>) -> Result<()> {
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/hooks/github", post(github))
        .with_state(Ctx { canon, hush });
    let listener = tokio::net::TcpListener::bind(listen).await.with_context(|| format!("listening on {listen}"))?;
    info!(%listen, "http endpoint up");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn github(State(ctx): State<Ctx>, headers: HeaderMap, body: Bytes) -> (StatusCode, String) {
    let secret = match ctx.hush.get(HOST, WEBHOOK_SECRET) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return (StatusCode::SERVICE_UNAVAILABLE, format!("set henry/{WEBHOOK_SECRET} to accept webhooks"));
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let signature = headers.get("x-hub-signature-256").and_then(|v| v.to_str().ok()).unwrap_or_default();
    if !verify(secret.expose().as_bytes(), &body, signature) {
        warn!("refused a webhook delivery with a bad signature");
        return (StatusCode::UNAUTHORIZED, "bad signature".into());
    }
    let event = headers.get("x-github-event").and_then(|v| v.to_str().ok()).unwrap_or_default();
    match event {
        "ping" => (StatusCode::OK, "pong".into()),
        "push" => match on_push(&ctx.canon, &body) {
            Ok(apps) => {
                if !apps.is_empty() {
                    info!(?apps, "push received, deploys queued");
                }
                (StatusCode::ACCEPTED, serde_json::json!({ "queued": apps }).to_string())
            }
            Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")),
        },
        _ => (StatusCode::NO_CONTENT, String::new()),
    }
}

/// GitHub's `X-Hub-Signature-256`: `sha256=` and the hex HMAC of the body.
/// Compared in constant time.
pub fn verify(secret: &[u8], body: &[u8], header: &str) -> bool {
    let Some(hex_sig) = header.strip_prefix("sha256=") else { return false };
    let Ok(given) = hex::decode(hex_sig) else { return false };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC takes any key length");
    mac.update(body);
    let expected = mac.finalize().into_bytes();
    expected.as_slice().ct_eq(&given).into()
}

/// Queue a deploy for every app this push is for. Returns their names.
pub fn on_push(canon: &Canon, body: &[u8]) -> Result<Vec<String>> {
    let push: serde_json::Value = serde_json::from_slice(body).context("the push body is not JSON")?;
    if push["deleted"].as_bool() == Some(true) {
        return Ok(vec![]);
    }
    let branch =
        push["ref"].as_str().and_then(|r| r.strip_prefix("refs/heads/")).context("the push names no branch")?;
    let commit = push["after"].as_str().filter(|c| c.len() == 40 && *c != "0".repeat(40)).context("no commit")?;
    let repo = push["repository"]["full_name"].as_str().context("the push names no repository")?.to_lowercase();

    let mut queued = Vec::new();
    for app in canon.apps()? {
        let Ok(lease) = app.lease() else { continue };
        if press::git::repo_slug(&app.repo).as_deref() == Some(repo.as_str())
            && lease.branch == branch
            && lease.auto_deploy
            && app.desired == Desired::Running
        {
            press::request(canon, &app.name, Some(commit), false)?;
            queued.push(app.name);
        }
    }
    Ok(queued)
}

#[cfg(test)]
mod tests {
    use canon::Source;
    use lease::Lease;

    use super::*;

    fn sign(secret: &[u8], body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn signatures_must_match_exactly() {
        let body = br#"{"zen":"hi"}"#;
        assert!(verify(b"s3cret", body, &sign(b"s3cret", body)));
        assert!(!verify(b"other", body, &sign(b"s3cret", body)));
        assert!(!verify(b"s3cret", b"tampered", &sign(b"s3cret", body)));
        assert!(!verify(b"s3cret", body, ""));
        assert!(!verify(b"s3cret", body, "sha1=abc"));
        assert!(!verify(b"s3cret", body, "sha256=not-hex"));
    }

    fn canon_with(apps: &[(&str, &str, &str)]) -> Canon {
        let canon = Canon::open_in_memory().unwrap();
        for (name, repo, branch) in apps {
            let text = format!(
                "name = \"{name}\"\nbranch = \"{branch}\"\n[[service]]\nname = \"web\"\nport = 80\nbuild = {{ image = \"x\" }}\nhealth = {{ tcp = true }}\n"
            );
            canon.insert_app(&Lease::parse(&text).unwrap(), &text, repo, Source::Operator).unwrap();
        }
        canon
    }

    fn push(repo: &str, branch: &str, after: &str) -> Vec<u8> {
        serde_json::json!({ "ref": format!("refs/heads/{branch}"), "after": after, "deleted": false, "repository": { "full_name": repo } })
            .to_string()
            .into_bytes()
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn a_push_deploys_the_apps_built_from_that_branch() {
        let canon = canon_with(&[
            ("draw", "https://github.com/specbug/draw", "main"),
            ("draw-beta", "git@github.com:specbug/draw.git", "beta"),
            ("odyssey", "https://github.com/specbug/odyssey", "main"),
        ]);
        assert_eq!(on_push(&canon, &push("SpecBug/draw", "main", SHA)).unwrap(), ["draw"]);
        let job = canon.claim().unwrap().unwrap();
        assert_eq!(job.kind, press::JOB);
        assert_eq!(job.payload["commit"], SHA);

        assert_eq!(on_push(&canon, &push("specbug/draw", "beta", SHA)).unwrap(), ["draw-beta"]);
        assert!(on_push(&canon, &push("specbug/draw", "feature", SHA)).unwrap().is_empty());
        assert!(on_push(&canon, &push("someone/else", "main", SHA)).unwrap().is_empty());
    }

    #[test]
    fn deletions_stopped_apps_and_junk_queue_nothing() {
        let canon = canon_with(&[("draw", "https://github.com/specbug/draw", "main")]);
        let deleted = serde_json::json!({ "ref": "refs/heads/main", "after": "0".repeat(40), "deleted": true, "repository": { "full_name": "specbug/draw" } });
        assert!(on_push(&canon, deleted.to_string().as_bytes()).unwrap().is_empty());

        canon.set_desired("draw", Desired::Stopped).unwrap();
        assert!(on_push(&canon, &push("specbug/draw", "main", SHA)).unwrap().is_empty());

        assert!(on_push(&canon, b"not json").is_err());
        assert!(on_push(&canon, &push("specbug/draw", "main", "short")).is_err());
    }
}
