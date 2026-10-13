use std::sync::Arc;

use canon::serde_json::{Value, json};
use canon::{Canon, Source, Status};
use hush::{Hush, Key};
use lease::Lease;
use reqwest::{Client, StatusCode};

use super::*;
use crate::access::tests as jwt;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

struct NoEngine;

impl Logs for NoEngine {
    async fn tail(&self, app: &str, lines: usize) -> Result<Vec<Log>, String> {
        Ok(vec![Log {
            container: format!("{app}-web"),
            service: "web".into(),
            release: Some("1".into()),
            text: format!("{lines} lines"),
        }])
    }
}

struct Served {
    base: String,
    canon: Arc<Canon>,
    hush: Arc<Hush>,
    client: Client,
}

impl Served {
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.client.get(format!("{}{path}", self.base))
    }

    fn local(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client.request(method, format!("{}{path}", self.base)).header("cookie", format!("{COOKIE}={TOKEN}"))
    }

    async fn json(&self, path: &str) -> Value {
        let r = self.local(reqwest::Method::GET, path).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{path}");
        r.json().await.unwrap()
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.local(reqwest::Method::POST, path).json(&body).send().await.unwrap()
    }
}

async fn serve(public: bool) -> Served {
    let canon = Arc::new(Canon::open_in_memory().unwrap());
    let hush = Arc::new(Hush::new(canon.clone(), &Key::generate().unwrap()));
    let text = "name = \"draw\"\n[[service]]\nname = \"web\"\nport = 80\nbuild = { image = \"x\" }\nhealth = { http = \"/\" }\nsecrets = [\"API_KEY\"]\npublic = { hostname = \"draw.example.com\" }\n";
    canon.insert_app(&Lease::parse(text).unwrap(), text, "https://github.com/me/draw", Source::Operator).unwrap();
    let access = public.then(|| {
        let a = Access::with_certs(jwt::ISSUER, jwt::AUD, "http://127.0.0.1:9/certs");
        a.set_keys(&jwt::jwks("k1"));
        a
    });
    let settings = Settings { token: TOKEN.into(), access, upkeep: mend::Task::ALL.to_vec(), backups: false };
    let router = Glass::new(canon.clone(), hush.clone(), NoEngine, settings).router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    Served { base, canon, hush, client }
}

fn access_token(email: &str) -> String {
    jwt::sign(
        "k1",
        json!({ "aud": [jwt::AUD], "iss": jwt::ISSUER, "sub": "u-1", "email": email, "exp": jwt::now() + 300 }),
    )
}

#[tokio::test]
async fn on_the_mac_only_the_login_cookie_gets_in() {
    let s = serve(false).await;
    assert_eq!(s.get("/api/apps").send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    let wrong = s.get("/api/apps").header("cookie", format!("{COOKIE}=nope")).send().await.unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    let bearer = s.get("/api/apps").header("authorization", format!("Bearer {TOKEN}")).send().await.unwrap();
    assert_eq!(bearer.status(), StatusCode::OK);
    assert_eq!(s.json("/api/session").await["via"], "local");

    // Another site's name rebound to 127.0.0.1 holds no cookie and is refused anyway.
    let rebound = s.local(reqwest::Method::GET, "/api/apps").header("host", "evil.example").send().await.unwrap();
    assert_eq!(rebound.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_login_link_trades_the_token_for_a_cookie() {
    let s = serve(false).await;
    let r = s.get(&format!("/login?token={TOKEN}")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::SEE_OTHER);
    let cookie = r.headers()["set-cookie"].to_str().unwrap();
    assert!(
        cookie.starts_with(&format!("{COOKIE}={TOKEN};"))
            && cookie.contains("HttpOnly")
            && cookie.contains("SameSite=Strict")
    );
    assert_eq!(s.get("/login?token=guess").send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    let edge = s.get(&format!("/login?token={TOKEN}")).header(EDGE_HEADER, "edge").send().await.unwrap();
    assert_eq!(edge.status(), StatusCode::FORBIDDEN, "the cookie is for the Mac only");
}

#[tokio::test]
async fn through_the_edge_only_cloudflare_access_gets_in() {
    let closed = serve(false).await;
    let r = closed.get("/api/apps").header(EDGE_HEADER, "edge").header(access::HEADER, access_token("me@example.com"));
    assert_eq!(r.send().await.unwrap().status(), StatusCode::FORBIDDEN, "no [glass] access: not public");

    let s = serve(true).await;
    let edge = |req: reqwest::RequestBuilder| req.header(EDGE_HEADER, "edge").header("host", "glass.example.com");
    let ok = edge(s.get("/api/session")).header(access::HEADER, access_token("me@example.com")).send().await.unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(ok.json::<Value>().await.unwrap()["who"], "me@example.com");

    // The Mac's cookie is not Access.
    let cookie = edge(s.get("/api/apps")).header("cookie", format!("{COOKIE}={TOKEN}")).send().await.unwrap();
    assert_eq!(cookie.status(), StatusCode::UNAUTHORIZED);
    let forged = edge(s.get("/api/apps")).header(access::HEADER, "eyJhbGciOiJSUzI1NiJ9.e30.c2ln").send().await.unwrap();
    assert_eq!(forged.status(), StatusCode::UNAUTHORIZED);

    // A change through the edge must come from Glass's own page.
    let token = access_token("me@example.com");
    let other_site = edge(s.client.post(format!("{}/api/apps/draw/deploy", s.base)))
        .header(access::HEADER, &token)
        .header("origin", "https://evil.example")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(other_site.status(), StatusCode::FORBIDDEN);
    let own = edge(s.client.post(format!("{}/api/apps/draw/deploy", s.base)))
        .header(access::HEADER, &token)
        .header("origin", "https://glass.example.com")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(own.status(), StatusCode::OK);
    let asked = s.canon.events(Some("draw"), 1).unwrap().remove(0);
    assert_eq!(asked.message, "a deploy of the branch head, asked by me@example.com in Glass");
}

#[tokio::test]
async fn a_change_must_be_json_so_a_plain_form_cannot_make_one() {
    let s = serve(false).await;
    let form = s
        .local(reqwest::Method::POST, "/api/apps/draw/stop")
        .header("content-type", "application/x-www-form-urlencoded")
        .body("x=1")
        .send()
        .await
        .unwrap();
    assert_eq!(form.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(s.canon.app("draw").unwrap().desired, canon::Desired::Running);

    assert_eq!(s.post("/api/apps/draw/stop", json!({})).await.status(), StatusCode::OK);
    assert_eq!(s.canon.app("draw").unwrap().desired, canon::Desired::Stopped);
    assert_eq!(s.post("/api/apps/nope/start", json!({})).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(s.post("/api/apps/draw/backup", json!({})).await.status(), StatusCode::CONFLICT, "no [ark]");
}

#[tokio::test]
async fn a_secret_goes_in_and_never_comes_out() {
    let s = serve(false).await;
    s.canon.set_status("draw", Status::NeedsYou, Some("waiting on API_KEY")).unwrap();
    let before = s.json("/api/apps/draw").await;
    assert_eq!(before["secrets"]["missing"], json!(["API_KEY"]));
    assert_eq!(s.json("/api/apps").await["apps"][0]["missing"], json!(["API_KEY"]));

    let put = |name: &str, value: &str| {
        s.local(reqwest::Method::PUT, &format!("/api/apps/draw/secrets/{name}")).json(&json!({ "value": value })).send()
    };
    assert_eq!(put("OTHER", "x").await.unwrap().status(), StatusCode::BAD_REQUEST, "not asked for");
    assert_eq!(put("API_KEY", "").await.unwrap().status(), StatusCode::BAD_REQUEST);
    assert_eq!(put("API_KEY", "sk-live-very-secret").await.unwrap().status(), StatusCode::OK);
    assert_eq!(s.hush.get("draw", "API_KEY").unwrap().unwrap().expose(), "sk-live-very-secret");

    let after = s.local(reqwest::Method::GET, "/api/apps/draw").send().await.unwrap().text().await.unwrap();
    assert!(!after.contains("sk-live"));
    let v: Value = canon::serde_json::from_str(&after).unwrap();
    assert_eq!(v["secrets"], json!({ "set": ["API_KEY"], "missing": [] }));
    let events = s.local(reqwest::Method::GET, "/api/events").send().await.unwrap().text().await.unwrap();
    assert!(events.contains("draw/API_KEY set") && !events.contains("sk-live"));
}

#[tokio::test]
async fn an_app_shows_its_month_and_its_logs() {
    let s = serve(false).await;
    s.canon.set_status("draw", Status::Live, None).unwrap();
    let v = s.json("/api/apps/draw").await;
    let strip = v["strip"].as_str().unwrap();
    assert_eq!(strip.len(), strip::HOURS);
    // Created and made live within this hour: the hour's worst was onboarding.
    assert!(strip.ends_with(".o"), "{}", &strip[strip::HOURS - 4..]);
    assert_eq!(v["services"][0]["public"], "https://draw.example.com");
    assert_eq!(s.json("/api/apps/draw/logs?lines=5").await["logs"][0]["text"], "5 lines");
    assert_eq!(s.local(reqwest::Method::GET, "/api/apps/nope").send().await.unwrap().status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn onboarding_from_glass_starts_a_draft() {
    let s = serve(false).await;
    assert_eq!(s.post("/api/onboard", json!({ "repo": "file:///etc" })).await.status(), StatusCode::BAD_REQUEST);
    let r = s.post("/api/onboard", json!({ "repo": "https://github.com/me/Site.git", "hostname": " " })).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.json::<Value>().await.unwrap()["name"], "site");
    let job = s.canon.claim().unwrap().unwrap();
    assert_eq!((job.kind.as_str(), job.app.as_deref()), (draft::JOB, Some("site")));
    let apps = s.json("/api/apps").await;
    assert_eq!(apps["drafts"][0]["name"], "site");
    assert_eq!(apps["drafts"][0]["onboarding"]["hostname"], Value::Null, "a blank hostname is none");
    let again = s.post("/api/onboard", json!({ "repo": "https://github.com/me/draw" })).await;
    assert_eq!(again.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn the_house_lists_upkeep_and_runs_a_task_on_request() {
    let s = serve(false).await;
    let v = s.json("/api/house").await;
    assert_eq!(v["upkeep"].as_array().unwrap().len(), mend::Task::ALL.len());
    assert_eq!(s.post("/api/house/disk", json!({})).await.status(), StatusCode::OK);
    assert_eq!(s.canon.claim().unwrap().unwrap().app.as_deref(), Some("disk"));
    assert_eq!(s.post("/api/house/nap", json!({})).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_page_is_served_with_a_strict_policy() {
    let s = serve(false).await;
    let page = s.get("/").send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    let csp = page.headers()["content-security-policy"].to_str().unwrap().to_owned();
    assert!(csp.contains("default-src 'none'") && csp.contains("frame-ancestors 'none'"));
    assert!(page.text().await.unwrap().contains("/glass.js"));
    for path in ["/glass.js", "/glass.css", "/icon.svg", "/fonts/rsans.woff2", "/fonts/rmono.woff2"] {
        assert_eq!(s.get(path).send().await.unwrap().status(), StatusCode::OK, "{path}");
    }
}
