use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post, put};
use canon::serde_json::{self, Value, json};
use canon::{ReleaseState, Source};

use super::caddy::Caddy;
use super::cloudflare::{Cloudflare, DnsReport, MANAGED};
use super::*;

// --- fakes -----------------------------------------------------------------

#[derive(Default)]
struct FakeProxy {
    applied: Mutex<Vec<Vec<Route>>>,
}

impl Proxy for Arc<FakeProxy> {
    async fn apply(&self, routes: &[Route]) -> Result<bool> {
        let mut applied = self.applied.lock().unwrap();
        let changed = applied.last().is_none_or(|last| last != routes);
        if changed {
            applied.push(routes.to_vec());
        }
        Ok(changed)
    }
}

#[derive(Default)]
struct FakeTunnel {
    ingress: Mutex<Vec<Vec<String>>>,
}

impl Tunnel for Arc<FakeTunnel> {
    async fn set_ingress(&self, hosts: &[String], _origin: &str) -> Result<()> {
        self.ingress.lock().unwrap().push(hosts.to_vec());
        Ok(())
    }
    async fn sync_dns(&self, hosts: &[String]) -> Result<DnsReport> {
        Ok(DnsReport { created: hosts.to_vec(), ..DnsReport::default() })
    }
}

const LEASE: &str = r#"
name = "draw"
[[service]]
name = "web"
port = 80
build = { image = "x" }
health = { http = "/" }
public = { hostname = "draw.example.com" }
[[service]]
name = "api"
port = 81
build = { image = "y" }
health = { http = "/" }
public = { hostname = "draw.example.com", path = "/api" }
"#;

fn canon_with_app() -> Arc<Canon> {
    let canon = Arc::new(Canon::open_in_memory().unwrap());
    canon.insert_app(&Lease::parse(LEASE).unwrap(), LEASE, "repo", Source::Operator).unwrap();
    canon
}

/// A release that Plumb has published, optionally live.
fn release(canon: &Canon, live: bool) -> (i64, u16, u16) {
    let r = canon.create_release("draw", "sha", LEASE).unwrap();
    canon.set_release_state(r.id, if live { ReleaseState::Live } else { ReleaseState::Trial }, None).unwrap();
    let web = canon.port_for("draw", "web", r.id, 40000..41000).unwrap();
    let api = canon.port_for("draw", "api", r.id, 40000..41000).unwrap();
    (r.id, web, api)
}

fn settings() -> Settings {
    Settings {
        origin: "http://127.0.0.1:8080".into(),
        own: vec![Route {
            host: "hooks.example.com".into(),
            path: "/hooks".into(),
            upstream: 7777,
            owner: "henry".into(),
        }],
        gate: Duration::from_millis(200),
    }
}

// --- what gets routed ----------------------------------------------------------

#[test]
fn only_live_releases_are_routed() {
    let canon = canon_with_app();
    let (_, _, _) = release(&canon, false);
    assert!(desired(&canon).unwrap().is_empty(), "a release on trial has no public route");

    let (_, web, api) = release(&canon, true);
    let routes = desired(&canon).unwrap();
    assert_eq!(routes.len(), 2);
    assert!(routes.contains(&Route {
        host: "draw.example.com".into(),
        path: "/".into(),
        upstream: web,
        owner: "draw".into()
    }));
    assert!(routes.contains(&Route {
        host: "draw.example.com".into(),
        path: "/api".into(),
        upstream: api,
        owner: "draw".into()
    }));

    canon.set_desired("draw", Desired::Stopped).unwrap();
    assert!(desired(&canon).unwrap().is_empty(), "a stopped app keeps no route");
}

#[tokio::test]
async fn a_pass_applies_routes_and_tunnel_once() {
    let canon = canon_with_app();
    release(&canon, true);
    let (proxy, tunnel) = (Arc::new(FakeProxy::default()), Arc::new(FakeTunnel::default()));
    let hinge = Hinge::new(canon, proxy.clone(), Some(tunnel.clone()), settings());

    let first = hinge.pass().await.unwrap();
    assert!(first.proxy_changed && first.tunnel_changed);
    assert_eq!(proxy.applied.lock().unwrap()[0].len(), 3, "two app routes and Henry's own");
    assert_eq!(*tunnel.ingress.lock().unwrap(), [vec!["draw.example.com".to_owned(), "hooks.example.com".to_owned()]]);

    let second = hinge.pass().await.unwrap();
    assert!(!second.proxy_changed && !second.tunnel_changed, "nothing changed, nothing sent");
    assert_eq!(tunnel.ingress.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_route_waits_for_its_new_upstream_to_answer() {
    let canon = canon_with_app();
    let (_, old_web, _) = release(&canon, true);
    let (proxy, tunnel) = (Arc::new(FakeProxy::default()), Arc::new(FakeTunnel::default()));
    let hinge = Hinge::new(canon.clone(), proxy.clone(), Some(tunnel), settings());
    hinge.pass().await.unwrap();

    // A new release goes live, but nothing listens on its port yet.
    let (_, new_web, _) = release(&canon, true);
    let report = hinge.pass().await.unwrap();
    assert!(report.held.contains(&"draw.example.com/".to_owned()), "{report:?}");
    let web =
        |p: &FakeProxy| p.applied.lock().unwrap().last().unwrap().iter().find(|r| r.path == "/").unwrap().upstream;
    assert_eq!(web(&proxy), old_web, "traffic stays on the old release");

    // Once it listens, the route swings.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", new_web)).await.unwrap();
    let report = hinge.pass().await.unwrap();
    assert!(report.held.iter().all(|h| h != "draw.example.com/"));
    assert_eq!(web(&proxy), new_web);
    drop(listener);
}

#[tokio::test]
async fn an_app_cannot_take_a_hostname_henry_serves_itself() {
    let canon = Arc::new(Canon::open_in_memory().unwrap());
    let text = LEASE.replace("draw.example.com", "hooks.example.com");
    canon.insert_app(&Lease::parse(&text).unwrap(), &text, "repo", Source::Operator).unwrap();
    let r = canon.create_release("draw", "sha", &text).unwrap();
    canon.set_release_state(r.id, ReleaseState::Live, None).unwrap();
    canon.port_for("draw", "web", r.id, 40000..41000).unwrap();
    canon.port_for("draw", "api", r.id, 40000..41000).unwrap();
    let proxy = Arc::new(FakeProxy::default());
    let hinge = Hinge::new(canon, proxy.clone(), None::<Arc<FakeTunnel>>, settings());
    hinge.pass().await.unwrap();
    let applied = proxy.applied.lock().unwrap()[0].clone();
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].owner, "henry");
}

#[test]
fn caddy_marks_requests_to_henrys_own_routes() {
    let caddy = Caddy::new("http://127.0.0.1:2019", "127.0.0.1:8080");
    let own = Route { host: "henry.example.com".into(), path: "/".into(), upstream: 7777, owner: HENRY.into() };
    let c = caddy.config(&[own]);
    let proxy = &c["apps"]["http"]["servers"]["henry"]["routes"][0]["handle"][0];
    assert_eq!(proxy["headers"]["request"]["set"][caddy::EDGE_HEADER], json!(["edge"]));
}

// --- Caddy ---------------------------------------------------------------------

#[test]
fn caddy_config_routes_longer_paths_first() {
    let caddy = Caddy::new("http://127.0.0.1:2019", "127.0.0.1:8080");
    let routes = vec![
        Route { host: "a.example.com".into(), path: "/".into(), upstream: 20000, owner: "a".into() },
        Route { host: "a.example.com".into(), path: "/api".into(), upstream: 20001, owner: "a".into() },
    ];
    let c = caddy.config(&routes);
    let server = &c["apps"]["http"]["servers"]["henry"];
    assert_eq!(server["listen"], json!(["127.0.0.1:8080"]));
    assert_eq!(server["automatic_https"]["disable"], true);
    let r = server["routes"].as_array().unwrap();
    assert_eq!(r[0]["match"][0]["path"], json!(["/api", "/api/*"]));
    assert_eq!(r[0]["handle"][0]["upstreams"][0]["dial"], "127.0.0.1:20001");
    assert!(r[1]["match"][0].get("path").is_none(), "`/` matches the whole host");
    assert!(r[0]["handle"][0].get("headers").is_none(), "an app's requests are not marked");
    assert_eq!(r[2]["handle"][0]["status_code"], 404, "everything else is a 404");
    assert!(r[2].get("match").is_none());
    assert_eq!(c["admin"]["listen"], "127.0.0.1:2019");
}

#[derive(Default)]
struct CaddyState {
    config: Mutex<Value>,
    loads: Mutex<u32>,
}

async fn mock_caddy() -> (String, Arc<CaddyState>) {
    mock_caddy_on(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap()).await
}

async fn mock_caddy_on(listener: tokio::net::TcpListener) -> (String, Arc<CaddyState>) {
    let state = Arc::new(CaddyState::default());
    let app = Router::new()
        .route(
            "/config/",
            get(|State(s): State<Arc<CaddyState>>| async move { axum::Json(s.config.lock().unwrap().clone()) }),
        )
        .route(
            "/load",
            post(|State(s): State<Arc<CaddyState>>, axum::Json(v): axum::Json<Value>| async move {
                *s.config.lock().unwrap() = v;
                *s.loads.lock().unwrap() += 1;
            }),
        )
        .with_state(state.clone());
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state)
}

#[tokio::test]
async fn caddy_is_waited_for_while_its_admin_endpoint_restarts() {
    // A port nothing listens on yet, the way Caddy's admin port is for a
    // moment after each load.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let late = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        mock_caddy_on(tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap()).await
    });
    let caddy = Caddy::new(format!("http://127.0.0.1:{port}"), "127.0.0.1:8080");
    assert!(caddy.apply(&[]).await.unwrap());
    let (_, state) = late.await.unwrap();
    assert_eq!(*state.loads.lock().unwrap(), 1);

    // Nothing ever answering is still an error, soon.
    let gone = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let started = std::time::Instant::now();
    let err = Caddy::new(format!("http://127.0.0.1:{gone}"), "127.0.0.1:8080").apply(&[]).await.unwrap_err();
    assert!(matches!(err, Error::Unreachable("Caddy", _)), "{err}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn caddy_loads_only_when_the_config_differs() {
    let (admin, state) = mock_caddy().await;
    let caddy = Caddy::new(admin, "127.0.0.1:8080");
    let routes = vec![Route { host: "a.example.com".into(), path: "/".into(), upstream: 20000, owner: "a".into() }];
    assert!(caddy.apply(&routes).await.unwrap());
    assert!(!caddy.apply(&routes).await.unwrap());
    assert_eq!(*state.loads.lock().unwrap(), 1);
    assert!(caddy.apply(&[]).await.unwrap());
}

#[tokio::test]
async fn an_unreachable_caddy_is_an_error_not_a_silent_skip() {
    let caddy = Caddy::new("http://127.0.0.1:1", "127.0.0.1:8080");
    assert!(matches!(caddy.apply(&[]).await, Err(Error::Unreachable("Caddy", _))));
}

// --- Cloudflare ----------------------------------------------------------------

#[derive(Default)]
struct Cf {
    records: Mutex<BTreeMap<String, (String, String, String)>>, // id -> (name, content, comment)
    ingress: Mutex<Value>,
    next: Mutex<u32>,
}

fn ok(result: Value) -> axum::Json<Value> {
    axum::Json(json!({ "success": true, "errors": [], "result": result, "result_info": { "total_pages": 1 } }))
}

async fn mock_cloudflare() -> (String, Arc<Cf>) {
    let cf = Arc::new(Cf::default());
    type S = State<Arc<Cf>>;
    let app = Router::new()
        .route(
            "/zones",
            get(|Query(q): Query<BTreeMap<String, String>>| async move {
                if q.get("name").map(String::as_str) == Some("example.com") {
                    ok(json!([{ "id": "z1" }]))
                } else {
                    ok(json!([]))
                }
            }),
        )
        .route(
            "/zones/z1/dns_records",
            get(|State(s): S| async move {
                let r: Vec<Value> = s
                    .records
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(id, (n, c, m))| json!({ "id": id, "name": n, "content": c, "comment": m }))
                    .collect();
                ok(json!(r))
            })
            .post(|State(s): S, axum::Json(v): axum::Json<Value>| async move {
                let mut n = s.next.lock().unwrap();
                *n += 1;
                let id = format!("r{n}");
                s.records.lock().unwrap().insert(
                    id.clone(),
                    (
                        v["name"].as_str().unwrap().into(),
                        v["content"].as_str().unwrap().into(),
                        v["comment"].as_str().unwrap_or("").into(),
                    ),
                );
                ok(json!({ "id": id }))
            }),
        )
        .route(
            "/zones/z1/dns_records/{id}",
            put(|State(s): S, Path(id): Path<String>, axum::Json(v): axum::Json<Value>| async move {
                s.records.lock().unwrap().insert(
                    id,
                    (
                        v["name"].as_str().unwrap().into(),
                        v["content"].as_str().unwrap().into(),
                        v["comment"].as_str().unwrap_or("").into(),
                    ),
                );
                ok(json!({}))
            })
            .delete(|State(s): S, Path(id): Path<String>| async move {
                s.records.lock().unwrap().remove(&id);
                ok(json!({}))
            }),
        )
        .route(
            "/accounts/acct/cfd_tunnel/tun/configurations",
            put(|State(s): S, axum::Json(v): axum::Json<Value>| async move {
                *s.ingress.lock().unwrap() = v;
                ok(json!({}))
            }),
        )
        .with_state(cf.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), cf)
}

fn names(cf: &Cf) -> Vec<String> {
    let mut n: Vec<String> = cf.records.lock().unwrap().values().map(|(n, _, _)| n.clone()).collect();
    n.sort();
    n
}

#[tokio::test]
async fn ingress_sends_every_host_to_caddy_and_ends_with_a_404() {
    let (base, cf) = mock_cloudflare().await;
    let api = Cloudflare::new(&base, "acct", "tun", vec!["example.com".into()], "token");
    api.set_ingress(&["a.example.com".into(), "b.example.com".into()], "http://127.0.0.1:8080").await.unwrap();
    let ingress = cf.ingress.lock().unwrap()["config"]["ingress"].clone();
    assert_eq!(
        ingress,
        json!([
            { "hostname": "a.example.com", "service": "http://127.0.0.1:8080" },
            { "hostname": "b.example.com", "service": "http://127.0.0.1:8080" },
            { "service": "http_status:404" },
        ])
    );
}

#[tokio::test]
async fn dns_is_created_fixed_and_cleaned_but_only_where_henry_owns_it() {
    let (base, cf) = mock_cloudflare().await;
    let api = Cloudflare::new(&base, "acct", "tun", vec!["example.com".into()], "token");
    {
        let mut r = cf.records.lock().unwrap();
        r.insert("mine-stale".into(), ("old.example.com".into(), "tun.cfargotunnel.com".into(), MANAGED.into()));
        r.insert("mine-wrong".into(), ("b.example.com".into(), "elsewhere.example.net".into(), MANAGED.into()));
        r.insert("yours".into(), ("blog.example.com".into(), "hosting.example.net".into(), "".into()));
        r.insert("yours-unrelated".into(), ("www.example.com".into(), "x.example.net".into(), "".into()));
    }
    let hosts = vec!["a.example.com".into(), "b.example.com".into(), "blog.example.com".into(), "app.other.org".into()];
    let report = api.sync_dns(&hosts).await.unwrap();

    assert_eq!(report.created, ["a.example.com"]);
    assert_eq!(report.updated, ["b.example.com"]);
    assert_eq!(report.deleted, ["old.example.com"]);
    let skipped: Vec<&str> = report.skipped.iter().map(|(h, _)| h.as_str()).collect();
    assert_eq!(skipped, ["app.other.org", "blog.example.com"]);
    assert_eq!(names(&cf), ["a.example.com", "b.example.com", "blog.example.com", "www.example.com"]);
    let recs = cf.records.lock().unwrap();
    assert_eq!(recs["yours"].1, "hosting.example.net", "a record you made is never overwritten");
    assert!(
        recs.values()
            .filter(|(n, _, _)| n == "a.example.com")
            .all(|(_, c, m)| c == "tun.cfargotunnel.com" && m == MANAGED)
    );
}

#[tokio::test]
async fn a_cloudflare_refusal_is_an_error() {
    let (base, _) = mock_cloudflare().await;
    let api = Cloudflare::new(&base, "acct", "other-tunnel", vec![], "token");
    assert!(api.set_ingress(&[], "http://127.0.0.1:8080").await.is_err());
    let unknown_zone = Cloudflare::new(&base, "acct", "tun", vec!["nope.org".into()], "token");
    assert!(unknown_zone.sync_dns(&[]).await.is_err());
    let _ = serde_json::Value::Null;
}
