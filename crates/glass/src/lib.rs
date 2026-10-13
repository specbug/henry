//! Glass: Henry's window.
//!
//! A small web UI, served by Henry itself from files built into the binary,
//! over a JSON API on the same loopback port as the webhook. Glass shows
//! what Canon knows and asks Henry to do what the CLI can: deploy, stop,
//! start, onboard a repo, set a secret an app is waiting on. It never shows
//! a secret's value, and it can mark nothing live: every action it takes is
//! a request that the daemon carries out under the same rules as the CLI's.
//!
//! Who may look:
//!
//! - Through the edge (Caddy marks those requests `X-Henry-Via: edge`), only
//!   holders of a valid Cloudflare Access token for Glass's application.
//!   Without `[glass]` Access settings, Henry routes no hostname to Glass.
//! - On the Mac itself, only a browser holding Glass's login cookie, which
//!   `henry open` hands out. Loopback is not trust: a container can reach
//!   the host's loopback too.

pub mod access;
mod api;
mod assets;
pub mod strip;
mod view;

use std::future::Future;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use canon::Canon;
use hush::Hush;
use subtle::ConstantTimeEq;

pub use access::{Access, Identity};

/// The header Caddy sets on every request it sends Henry from the edge.
pub const EDGE_HEADER: &str = "x-henry-via";
pub const EDGE_VALUE: &str = "edge";
/// The login cookie for requests made on the Mac.
pub const COOKIE: &str = "henry_glass";

/// A container's recent output.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Log {
    pub container: String,
    pub service: String,
    pub release: Option<String>,
    pub text: String,
}

/// Where Glass reads an app's container logs.
pub trait Logs: Send + Sync + 'static {
    fn tail(&self, app: &str, lines: usize) -> impl Future<Output = Result<Vec<Log>, String>> + Send;
}

/// The last `lines` of every container an app has in `rt`.
pub async fn tail_from<R: hold::Runtime>(rt: &R, app: &str, lines: usize) -> Result<Vec<Log>, String> {
    let mut out = Vec::new();
    let containers = rt.list().await.map_err(|e| e.to_string())?;
    for c in containers.iter().filter(|c| c.label(hold::label::APP) == Some(app)) {
        let text = rt.logs(&c.id, lines, None).await.unwrap_or_else(|e| format!("(no logs: {e})"));
        out.push(Log {
            container: c.name.clone(),
            service: c.label(hold::label::SERVICE).unwrap_or_default().to_owned(),
            release: c.label(hold::label::RELEASE).map(str::to_owned),
            text,
        });
    }
    out.sort_by(|a, b| a.service.cmp(&b.service).then(a.container.cmp(&b.container)));
    Ok(out)
}

pub struct Settings {
    /// The login token `henry open` puts in the cookie.
    pub token: String,
    /// Cloudflare Access, when Glass is public.
    pub access: Option<Access>,
    /// The upkeep tasks that run on a schedule.
    pub upkeep: Vec<mend::Task>,
    /// Whether `[ark]` is set, so a backup can be asked for.
    pub backups: bool,
}

pub struct Glass<L> {
    pub(crate) canon: Arc<Canon>,
    pub(crate) hush: Arc<Hush>,
    pub(crate) logs: L,
    pub(crate) settings: Settings,
}

impl<L: Logs> Glass<L> {
    pub fn new(canon: Arc<Canon>, hush: Arc<Hush>, logs: L, settings: Settings) -> Self {
        Glass { canon, hush, logs, settings }
    }

    /// The UI and its API. Merge it into Henry's router.
    pub fn router(self) -> Router {
        let glass = Arc::new(self);
        let api = api::routes::<L>()
            .route_layer(middleware::from_fn_with_state(glass.clone(), guard::<L>))
            .with_state(glass.clone());
        Router::new().merge(assets::routes()).merge(api).merge(api::login(glass))
    }
}

/// Who is asking, as Glass knows them.
#[derive(Debug, Clone, PartialEq)]
pub enum Viewer {
    /// Through Cloudflare Access.
    Edge(Identity),
    /// On the Mac, with the login cookie.
    Local,
}

impl Viewer {
    pub fn name(&self) -> String {
        match self {
            Viewer::Edge(id) => id.email.clone().unwrap_or_else(|| id.subject.clone()),
            Viewer::Local => "you on the Mac".into(),
        }
    }
}

pub(crate) fn refuse(status: StatusCode, why: impl Into<String>) -> Response {
    (status, axum::Json(canon::serde_json::json!({ "error": why.into() }))).into_response()
}

pub(crate) fn from_edge(headers: &HeaderMap) -> bool {
    // Any value at all means the request came through Caddy, or claims to;
    // either way it is held to the edge's rule.
    headers.contains_key(EDGE_HEADER)
}

/// Whether a request's Host is the Mac's own loopback. A page on another
/// site that rebinds its name to 127.0.0.1 is refused here.
pub(crate) fn loopback_host(headers: &HeaderMap) -> bool {
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or_default();
    let name = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    matches!(name, "127.0.0.1" | "localhost" | "::1")
}

pub(crate) fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|c| {
        let (k, v) = c.trim().split_once('=')?;
        (k == name).then_some(v)
    })
}

pub(crate) fn token_matches(given: &str, token: &str) -> bool {
    !token.is_empty() && given.as_bytes().ct_eq(token.as_bytes()).into()
}

/// Every API request: who is asking, and, for a change, that the request
/// is the page's own and not a form on another site.
async fn guard<L: Logs>(State(glass): State<Arc<Glass<L>>>, mut req: Request, next: Next) -> Response {
    let headers = req.headers();
    let viewer = if from_edge(headers) {
        let Some(access) = &glass.settings.access else {
            return refuse(StatusCode::FORBIDDEN, "Glass is not public: set [glass] access_team and access_aud");
        };
        let token = headers.get(access::HEADER).and_then(|v| v.to_str().ok());
        match access.verify(token).await {
            Ok(id) => Viewer::Edge(id),
            Err(e @ access::Refused::Keys(_)) => return refuse(StatusCode::SERVICE_UNAVAILABLE, e.to_string()),
            Err(e) => return refuse(StatusCode::UNAUTHORIZED, e.to_string()),
        }
    } else {
        if !loopback_host(headers) {
            return refuse(StatusCode::FORBIDDEN, "Glass answers on loopback only");
        }
        let bearer =
            headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
        let given = cookie(headers, COOKIE).or(bearer).unwrap_or_default();
        if !token_matches(given, &glass.settings.token) {
            return refuse(StatusCode::UNAUTHORIZED, "sign in: run `henry open` on the Mac");
        }
        Viewer::Local
    };
    if req.method() != Method::GET && req.method() != Method::HEAD {
        let json = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/json"));
        if !json {
            return refuse(StatusCode::UNSUPPORTED_MEDIA_TYPE, "send JSON");
        }
        if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or_default();
            let authority = origin.split_once("://").map_or("", |(_, a)| a);
            if authority != host {
                return refuse(StatusCode::FORBIDDEN, "a request from another site");
            }
        }
    }
    req.extensions_mut().insert(viewer);
    next.run(req).await
}

#[cfg(test)]
mod tests;
