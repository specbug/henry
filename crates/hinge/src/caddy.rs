//! Caddy, driven through its admin API.
//!
//! Henry owns Caddy's whole config and loads it in one request, which Caddy
//! applies atomically and without dropping connections. Caddy runs under its
//! own launchd agent with `--resume`, so the last routes survive a restart of
//! Caddy, of Henry, or of the Mac.

use std::time::Duration;

use canon::serde_json::{self, Value, json};

use crate::{Error, Proxy, Result, Route};

/// The header on Caddy's catch-all 404.
pub const NO_ROUTE_HEADER: &str = "X-Henry-Route";
/// The header Caddy sets on every request it sends to Henry's own routes,
/// replacing any the client sent, so Henry knows the request came from the
/// edge and holds it to the edge's rules.
pub const EDGE_HEADER: &str = "X-Henry-Via";

pub struct Caddy {
    /// The admin endpoint, e.g. `http://127.0.0.1:2019`.
    pub admin: String,
    /// Where Caddy listens for the tunnel, e.g. `127.0.0.1:8080`.
    pub listen: String,
    client: reqwest::Client,
}

impl Caddy {
    pub fn new(admin: impl Into<String>, listen: impl Into<String>) -> Caddy {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("a plain HTTP client always builds");
        Caddy { admin: admin.into(), listen: listen.into(), client }
    }

    /// The full Caddy config for these routes.
    pub fn config(&self, routes: &[Route]) -> Value {
        let admin_listen = self.admin.trim_start_matches("http://").trim_start_matches("https://");
        // Longer paths first, so `/api` is matched before `/` on one host.
        let mut ordered: Vec<&Route> = routes.iter().collect();
        ordered.sort_by(|a, b| a.host.cmp(&b.host).then(b.path.len().cmp(&a.path.len())).then(a.path.cmp(&b.path)));
        let mut routes: Vec<Value> = ordered
            .into_iter()
            .map(|r| {
                let mut matcher = json!({ "host": [r.host] });
                if r.path != "/" {
                    let base = r.path.trim_end_matches('/');
                    matcher["path"] = json!([base, format!("{base}/*")]);
                }
                let mut proxy = json!({
                    "handler": "reverse_proxy",
                    "upstreams": [{ "dial": format!("127.0.0.1:{}", r.upstream) }],
                });
                if r.owner == crate::HENRY {
                    proxy["headers"] = json!({ "request": { "set": { EDGE_HEADER: ["edge"] } } });
                }
                json!({ "match": [matcher], "handle": [proxy], "terminal": true })
            })
            .collect();
        // Anything no route claims gets a 404, not Caddy's empty 200, marked
        // so Proof can tell "no route" from an app's own 404.
        routes.push(json!({ "handle": [{
            "handler": "static_response",
            "status_code": 404,
            "headers": { "X-Henry-Route": ["none"] },
            "body": "no route\n",
        }] }));
        json!({
            "admin": { "listen": admin_listen },
            "apps": { "http": { "servers": { "henry": {
                "listen": [self.listen],
                "routes": routes,
                // TLS ends at Cloudflare; the tunnel to Caddy is local.
                "automatic_https": { "disable": true },
            }}}},
        })
    }
}

/// How long a refused connection is retried. Caddy restarts its admin
/// endpoint after every load, and a request in that gap is refused.
const ADMIN_RESTART: Duration = Duration::from_millis(1500);

impl Caddy {
    async fn send(&self, req: impl Fn() -> reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let deadline = tokio::time::Instant::now() + ADMIN_RESTART;
        loop {
            match req().send().await {
                Err(e) if e.is_connect() && tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                other => return other.map_err(|e| Error::Unreachable("Caddy", e.to_string())),
            }
        }
    }
}

impl Proxy for Caddy {
    async fn apply(&self, routes: &[Route]) -> Result<bool> {
        let want = self.config(routes);
        let current = self.send(|| self.client.get(format!("{}/config/", self.admin))).await?;
        if current.status().is_success() {
            let have: Value = current.json().await.unwrap_or(Value::Null);
            if have == want {
                return Ok(false);
            }
        }
        let body = serde_json::to_vec(&want).expect("a config always serializes");
        let loaded = self
            .send(|| {
                self.client
                    .post(format!("{}/load", self.admin))
                    .header("content-type", "application/json")
                    .body(body.clone())
            })
            .await?;
        if !loaded.status().is_success() {
            let body = loaded.text().await.unwrap_or_default();
            return Err(Error::Refused("Caddy", body));
        }
        Ok(true)
    }
}
