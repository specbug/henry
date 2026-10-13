//! The JSON API under `/api`, and the login that hands out the cookie.

use std::sync::Arc;

use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use canon::serde_json::{Value, json};
use canon::{Desired, Error as CanonError};
use serde::Deserialize;

use crate::{COOKIE, Glass, Logs, Viewer, refuse, view};

type Shared<L> = State<Arc<Glass<L>>>;

pub fn routes<L: Logs>() -> Router<Arc<Glass<L>>> {
    Router::new()
        .route("/api/session", get(session))
        .route("/api/apps", get(apps::<L>))
        .route("/api/apps/{name}", get(app::<L>))
        .route("/api/apps/{name}/logs", get(logs::<L>))
        .route("/api/apps/{name}/deploy", post(deploy::<L>))
        .route("/api/apps/{name}/start", post(start::<L>))
        .route("/api/apps/{name}/stop", post(stop::<L>))
        .route("/api/apps/{name}/backup", post(backup::<L>))
        .route("/api/apps/{name}/secrets/{secret}", put(set_secret::<L>))
        .route("/api/onboard", post(onboard::<L>))
        .route("/api/house", get(house::<L>))
        .route("/api/house/{task}", post(upkeep::<L>))
        .route("/api/events", get(events::<L>))
}

/// `/login?token=…`: trade the token `henry open` prints for a cookie, on
/// the Mac only.
pub fn login<L: Logs>(glass: Arc<Glass<L>>) -> Router {
    Router::new().route("/login", get(sign_in::<L>)).with_state(glass)
}

#[derive(Deserialize)]
struct Token {
    token: String,
}

async fn sign_in<L: Logs>(State(glass): Shared<L>, headers: HeaderMap, Query(q): Query<Token>) -> Response {
    if crate::from_edge(&headers) || !crate::loopback_host(&headers) {
        return refuse(StatusCode::FORBIDDEN, "sign in through Cloudflare Access");
    }
    if !crate::token_matches(&q.token, &glass.settings.token) {
        return refuse(StatusCode::UNAUTHORIZED, "that login link is not current; run `henry open` again");
    }
    let cookie = format!("{COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=2592000", q.token);
    ([(header::SET_COOKIE, cookie)], Redirect::to("/")).into_response()
}

fn fail(e: CanonError) -> Response {
    match e {
        CanonError::NoSuchApp(_) => refuse(StatusCode::NOT_FOUND, e.to_string()),
        CanonError::AppExists(_) | CanonError::RouteTaken { .. } => refuse(StatusCode::CONFLICT, e.to_string()),
        e => refuse(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn reply(r: Result<Value, CanonError>) -> Response {
    match r {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(e),
    }
}

/// Record who asked for what, so the event log says.
fn note<L>(glass: &Glass<L>, app: Option<&str>, viewer: &Viewer, what: &str) {
    let via = if matches!(viewer, Viewer::Edge(_)) { "edge" } else { "local" };
    let _ = glass.canon.record(
        app,
        "glass.request",
        &format!("{what}, asked by {} in Glass", viewer.name()),
        &json!({ "via": via }),
    );
}

async fn session(Extension(viewer): Extension<Viewer>) -> Json<Value> {
    Json(json!({ "who": viewer.name(), "via": if matches!(viewer, Viewer::Edge(_)) { "edge" } else { "local" } }))
}

async fn apps<L: Logs>(State(glass): Shared<L>) -> Response {
    reply(view::apps(&glass.canon))
}

async fn app<L: Logs>(State(glass): Shared<L>, Path(name): Path<String>) -> Response {
    match view::app(&glass.canon, &name, glass.settings.backups) {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => refuse(StatusCode::NOT_FOUND, format!("no app named `{name}`")),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct Lines {
    lines: Option<usize>,
}

async fn logs<L: Logs>(State(glass): Shared<L>, Path(name): Path<String>, Query(q): Query<Lines>) -> Response {
    if let Err(e) = glass.canon.app(&name) {
        return fail(e);
    }
    match glass.logs.tail(&name, q.lines.unwrap_or(200).clamp(1, 2000)).await {
        Ok(logs) => Json(json!({ "logs": logs })).into_response(),
        Err(e) => refuse(StatusCode::SERVICE_UNAVAILABLE, format!("cannot reach the container engine: {e}")),
    }
}

async fn deploy<L: Logs>(
    State(glass): Shared<L>,
    Extension(v): Extension<Viewer>,
    Path(name): Path<String>,
) -> Response {
    if let Err(e) = glass.canon.app(&name) {
        return fail(e);
    }
    match press::request(&glass.canon, &name, None, true) {
        Ok(job) => {
            note(&glass, Some(&name), &v, "a deploy of the branch head");
            Json(json!({ "job": job.id })).into_response()
        }
        Err(e) => refuse(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn desire<L>(glass: &Glass<L>, name: &str, v: &Viewer, d: Desired) -> Response {
    match glass.canon.set_desired(name, d) {
        Ok(()) => {
            note(glass, Some(name), v, if d == Desired::Running { "start" } else { "stop" });
            Json(json!({ "desired": d.as_str() })).into_response()
        }
        Err(e) => fail(e),
    }
}

async fn start<L: Logs>(
    State(glass): Shared<L>,
    Extension(v): Extension<Viewer>,
    Path(name): Path<String>,
) -> Response {
    desire(&glass, &name, &v, Desired::Running)
}

async fn stop<L: Logs>(State(glass): Shared<L>, Extension(v): Extension<Viewer>, Path(name): Path<String>) -> Response {
    desire(&glass, &name, &v, Desired::Stopped)
}

async fn backup<L: Logs>(
    State(glass): Shared<L>,
    Extension(v): Extension<Viewer>,
    Path(name): Path<String>,
) -> Response {
    if !glass.settings.backups {
        return refuse(StatusCode::CONFLICT, "backups are off: set [ark] in henry.toml");
    }
    if let Err(e) = glass.canon.app(&name) {
        return fail(e);
    }
    match glass.canon.enqueue(ark::JOB, Some(&name), &json!({ "force": true })) {
        Ok(job) => {
            note(&glass, Some(&name), &v, "a backup");
            Json(json!({ "job": job.id })).into_response()
        }
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct SecretValue {
    value: String,
}

/// Set one secret the app's lease asks for. The value goes straight into
/// Hush and is never sent back.
async fn set_secret<L: Logs>(
    State(glass): Shared<L>,
    Extension(v): Extension<Viewer>,
    Path((name, secret)): Path<(String, String)>,
    Json(body): Json<SecretValue>,
) -> Response {
    if body.value.is_empty() {
        return refuse(StatusCode::BAD_REQUEST, "an empty value; nothing was set");
    }
    let value = hush::Secret::new(body.value);
    let app = match glass.canon.app(&name) {
        Ok(a) => a,
        Err(e) => return fail(e),
    };
    let wanted = app.lease().map(|l| l.services.iter().any(|s| s.secrets.contains(&secret))).unwrap_or(false);
    if !wanted {
        return refuse(StatusCode::BAD_REQUEST, format!("`{name}` does not ask for a secret named {secret}"));
    }
    match glass.hush.set(&name, &secret, &value) {
        Ok(()) => {
            note(&glass, Some(&name), &v, &format!("{name}/{secret} set"));
            Json(json!({ "set": secret })).into_response()
        }
        Err(e) => refuse(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

#[derive(Deserialize)]
struct Onboard {
    repo: String,
    name: Option<String>,
    hostname: Option<String>,
    branch: Option<String>,
}

fn blank(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

async fn onboard<L: Logs>(
    State(glass): Shared<L>,
    Extension(v): Extension<Viewer>,
    Json(o): Json<Onboard>,
) -> Response {
    let repo = o.repo.trim();
    if !(repo.starts_with("https://") || repo.starts_with("git@")) {
        return refuse(StatusCode::BAD_REQUEST, "give the repo as an https:// or git@ URL");
    }
    let Some(name) = blank(o.name).or_else(|| draft::name_for(repo)) else {
        return refuse(StatusCode::BAD_REQUEST, "cannot tell the app's name from the URL; give one");
    };
    let (hostname, branch) = (blank(o.hostname), blank(o.branch));
    match draft::start(&glass.canon, &name, repo, branch.as_deref(), hostname.as_deref()) {
        Ok(_) => {
            note(&glass, Some(&name), &v, &format!("onboarding of {repo}"));
            Json(json!({ "name": name })).into_response()
        }
        Err(draft::Error::Canon(e)) => fail(e),
        Err(e) => refuse(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

async fn house<L: Logs>(State(glass): Shared<L>) -> Response {
    let mut v = match view::house(&glass.canon, &glass.settings.upkeep) {
        Ok(v) => v,
        Err(e) => return fail(e),
    };
    let checks: Vec<Value> = mend::host::checks()
        .await
        .into_iter()
        .map(|c| json!({ "name": c.name, "want": c.want, "got": c.got, "ok": c.ok }))
        .collect();
    v["checks"] = json!(checks);
    v["backups"] = json!(glass.settings.backups);
    Json(v).into_response()
}

async fn upkeep<L: Logs>(
    State(glass): Shared<L>,
    Extension(v): Extension<Viewer>,
    Path(task): Path<String>,
) -> Response {
    let Some(t) = mend::Task::parse(&task) else {
        return refuse(StatusCode::NOT_FOUND, format!("no upkeep task `{task}`"));
    };
    match glass.canon.enqueue(mend::JOB, Some(t.as_str()), &json!({})) {
        Ok(job) => {
            note(&glass, None, &v, &format!("upkeep task {task} now"));
            Json(json!({ "job": job.id })).into_response()
        }
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct After {
    after: Option<i64>,
    app: Option<String>,
}

async fn events<L: Logs>(State(glass): Shared<L>, Query(q): Query<After>) -> Response {
    let r = match q.after {
        Some(after) => glass.canon.events_after(after, q.app.as_deref(), 200),
        None => glass.canon.events(q.app.as_deref(), 50).map(|mut e| {
            e.reverse();
            e
        }),
    };
    reply(r.map(|e| json!({ "events": e.iter().map(view::event).collect::<Vec<_>>() })))
}
