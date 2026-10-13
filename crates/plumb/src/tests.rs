use std::sync::Arc;

use canon::{ReleaseState, Source};
use hold::fake::Fake;
use hush::{Key, Secret};
use lease::Lease;

use super::*;

const LEASE: &str = r#"
name = "draw"

[[service]]
name = "web"
port = 8080
build = { image = "draw-web:1" }
health = { http = "/health" }
public = { hostname = "draw.example.com" }
secrets = ["TOKEN"]
env = { LOG = "info" }
depends_on = ["db"]

[[service]]
name = "db"
port = 5432
build = { image = "postgres:17" }
health = { postgres = true }

[[service.volume]]
name = "pg"
mount = "/var/lib/postgresql/data"
backup = "postgres"
"#;

struct World {
    canon: Arc<Canon>,
    hush: Arc<Hush>,
    fake: Arc<Fake>,
    plumb: Plumb<Arc<Fake>, Arc<Fake>>,
}

fn world_with(settings: Settings) -> World {
    let canon = Arc::new(Canon::open_in_memory().unwrap());
    let hush = Arc::new(Hush::new(canon.clone(), &Key::generate().unwrap()));
    let fake = Arc::new(Fake::new());
    let plumb = Plumb::new(canon.clone(), hush.clone(), fake.clone(), fake.clone(), settings);
    World { canon, hush, fake, plumb }
}

fn world() -> World {
    world_with(Settings::default())
}

impl World {
    fn register(&self, text: &str) {
        let lease = Lease::parse(text).unwrap();
        self.canon.insert_app(&lease, text, "https://github.com/me/draw", Source::Operator).unwrap();
    }

    fn release(&self, app: &str, text: &str, state: ReleaseState) -> i64 {
        let r = self.canon.create_release(app, "0123456789abcdef", text).unwrap();
        self.canon.set_release_state(r.id, state, None).unwrap();
        r.id
    }

    fn secret(&self, name: &str, value: &str) {
        self.hush.set("draw", name, &Secret::new(value)).unwrap();
    }

    /// The standard app, with its secret, trying release 1.
    fn trying(&self) -> i64 {
        self.register(LEASE);
        self.secret("TOKEN", "t0k3n");
        self.release("draw", LEASE, ReleaseState::Trial)
    }

    fn names(&self) -> Vec<String> {
        let mut n: Vec<String> = self.fake.containers().into_iter().map(|c| c.spec.name).collect();
        n.sort();
        n
    }

    fn status(&self) -> Status {
        self.canon.app("draw").unwrap().status
    }
}

#[tokio::test]
async fn converges_from_nothing_and_then_holds_still() {
    let w = world();
    let r = w.trying();
    let report = w.plumb.pass().await.unwrap();
    assert_eq!(report.created, ["draw-db", format!("draw-web-r{r}").as_str()], "db first: web depends on it");
    assert_eq!(w.names(), ["draw-db", &format!("draw-web-r{r}")]);
    assert!(w.fake.containers().iter().all(|c| c.state == State::Running));
    assert_eq!(w.fake.networks(), ["henry-draw"]);
    assert_eq!(w.fake.volumes(), ["henry-draw-pg"]);

    w.fake.take_ops();
    let again = w.plumb.pass().await.unwrap();
    assert!(again.is_quiet(), "{again}");
    assert!(w.fake.take_ops().is_empty(), "a converged pass changes nothing");
    assert_eq!(w.status(), Status::Onboarding, "running is not proven: only Proof makes an app Live");
}

#[tokio::test]
async fn containers_get_what_their_lease_promises() {
    let w = world();
    let r = w.trying();
    w.plumb.pass().await.unwrap();

    let web = w.fake.container(&format!("draw-web-r{r}")).unwrap().spec;
    let env: BTreeMap<_, _> = web.env.iter().map(|e| (e.name.as_str(), e.value.as_str())).collect();
    assert_eq!(env["LOG"], "info");
    assert_eq!(env["PORT"], "8080");
    assert_eq!(env["TOKEN"], "t0k3n");
    assert_eq!(env["HENRY_APP"], "draw");
    assert_eq!(env["HENRY_DB_HOST"], "draw-db");
    assert!(web.env.iter().find(|e| e.name == "TOKEN").unwrap().secret);
    assert!(!format!("{web:?}").contains("t0k3n"));
    assert_eq!(web.publish.as_ref().map(|p| (p.host_ip.as_str(), p.host_port)), Some(("127.0.0.1", 20000)));
    assert!(web.aliases.is_empty(), "a candidate does not answer to the service name");
    assert_eq!(web.memory_bytes, 512 << 20);

    let db = w.fake.container("draw-db").unwrap().spec;
    assert_eq!(db.aliases, ["db"]);
    assert_eq!(db.mounts[0].volume, "henry-draw-pg");
    assert!(db.publish.is_none(), "private services are not published");
}

#[tokio::test]
async fn nothing_starts_while_secrets_are_missing() {
    let w = world();
    w.register(LEASE);
    w.release("draw", LEASE, ReleaseState::Trial);
    w.plumb.pass().await.unwrap();
    assert!(w.names().is_empty());
    assert_eq!(w.status(), Status::NeedsYou);
    assert_eq!(w.canon.app("draw").unwrap().status_detail.as_deref(), Some("missing secrets: TOKEN"));

    w.secret("TOKEN", "x");
    w.plumb.pass().await.unwrap();
    assert_eq!(w.names().len(), 2);
    assert_eq!(w.status(), Status::Onboarding);
}

#[tokio::test]
async fn a_missing_secret_never_takes_down_what_runs() {
    let w = world();
    let r = w.trying();
    w.canon.set_release_state(r, ReleaseState::Live, None).unwrap();
    w.plumb.pass().await.unwrap();
    assert_eq!(w.status(), Status::Live);

    w.hush.remove("draw", "TOKEN").unwrap();
    w.fake.take_ops();
    w.plumb.pass().await.unwrap();
    assert!(w.fake.take_ops().is_empty(), "frozen: nothing stopped, nothing removed");
    assert_eq!(w.status(), Status::NeedsYou);
}

#[tokio::test]
async fn a_candidate_runs_beside_live_and_replaces_it() {
    let w = world();
    let r1 = w.trying();
    w.canon.set_release_state(r1, ReleaseState::Live, None).unwrap();
    w.plumb.pass().await.unwrap();

    let next = LEASE.replace("draw-web:1", "draw-web:2");
    let r2 = w.release("draw", &next, ReleaseState::Trial);
    w.plumb.pass().await.unwrap();
    assert_eq!(w.names(), ["draw-db", &format!("draw-web-r{r1}"), &format!("draw-web-r{r2}")]);
    let live = w.fake.container(&format!("draw-web-r{r1}")).unwrap().spec;
    let cand = w.fake.container(&format!("draw-web-r{r2}")).unwrap().spec;
    assert_eq!(live.aliases, ["web"]);
    assert_ne!(live.publish.unwrap().host_port, cand.publish.unwrap().host_port);
    assert_eq!(w.status(), Status::Live);

    w.canon.set_release_state(r2, ReleaseState::Live, None).unwrap();
    let report = w.plumb.pass().await.unwrap();
    assert_eq!(report.removed, [format!("draw-web-r{r1}")]);
    // r2's web is now live, so it is recreated with the service alias.
    assert_eq!(report.recreated, [format!("draw-web-r{r2}")]);
    assert_eq!(w.fake.container(&format!("draw-web-r{r2}")).unwrap().spec.aliases, ["web"]);
    // r1's port went back to the pool.
    assert_eq!(w.canon.port_for("draw", "web", 99, 20000..30000).unwrap(), 20000);
}

#[tokio::test]
async fn a_stateful_service_is_never_run_twice() {
    let w = world();
    let r1 = w.trying();
    w.canon.set_release_state(r1, ReleaseState::Live, None).unwrap();
    w.plumb.pass().await.unwrap();

    let next = LEASE.replace("postgres:17", "postgres:18");
    w.release("draw", &next, ReleaseState::Trial);
    let report = w.plumb.pass().await.unwrap();
    assert_eq!(report.recreated, ["draw-db"], "replaced in place");
    let dbs: Vec<_> = w.names().into_iter().filter(|n| n.contains("-db")).collect();
    assert_eq!(dbs, ["draw-db"]);
    assert_eq!(w.fake.container("draw-db").unwrap().spec.image, "postgres:18");
}

#[tokio::test]
async fn a_crashed_container_is_started_again() {
    let w = world();
    let r = w.trying();
    w.canon.set_release_state(r, ReleaseState::Live, None).unwrap();
    w.plumb.pass().await.unwrap();

    w.fake.crash("draw-db");
    let report = w.plumb.pass().await.unwrap();
    assert_eq!(report.started, ["draw-db"]);
    assert_eq!(w.status(), Status::Live);
}

#[tokio::test]
async fn a_new_secret_value_recreates_its_containers() {
    let w = world();
    let r = w.trying();
    w.plumb.pass().await.unwrap();
    std::thread::sleep(Duration::from_millis(2));
    w.secret("TOKEN", "rotated");
    let report = w.plumb.pass().await.unwrap();
    assert_eq!(report.recreated, [format!("draw-web-r{r}")], "the db does not use the secret");
    let web = w.fake.container(&format!("draw-web-r{r}")).unwrap().spec;
    assert_eq!(web.env.iter().find(|e| e.name == "TOKEN").unwrap().value, "rotated");
}

#[tokio::test]
async fn a_failed_listing_is_not_an_empty_one() {
    let w = world();
    w.trying();
    w.plumb.pass().await.unwrap();
    let before = w.names();

    w.fake.fail_list(1);
    let report = w.plumb.pass().await.unwrap();
    assert!(report.skipped.is_none(), "one failure is retried");

    w.fake.fail_list(2);
    w.fake.take_ops();
    let report = w.plumb.pass().await.unwrap();
    assert!(report.skipped.is_some());
    assert!(w.fake.take_ops().is_empty(), "nothing was created or removed on a blind pass");
    assert_eq!(w.names(), before);
}

#[tokio::test]
async fn an_unreachable_engine_is_reported_once() {
    let w = world();
    w.trying();
    w.fake.set_down(true);
    for _ in 0..3 {
        assert!(w.plumb.pass().await.unwrap().skipped.is_some());
    }
    let problems = w.canon.events(None, 100).unwrap().into_iter().filter(|e| e.kind == "host.problem").count();
    assert_eq!(problems, 1);

    w.fake.set_down(false);
    w.plumb.pass().await.unwrap();
    assert!(w.canon.events(None, 5).unwrap().iter().any(|e| e.kind == "host.recovered"));
    assert_eq!(w.names().len(), 2);
}

#[tokio::test]
async fn only_henrys_own_leftovers_are_removed() {
    let w = world();
    w.fake.plant("someone-elses", Labels::new());
    w.fake.plant(
        "ghost-web-r3",
        Labels::from([(label::MANAGED.to_owned(), "1".to_owned()), (label::APP.to_owned(), "ghost".to_owned())]),
    );
    let report = w.plumb.pass().await.unwrap();
    assert_eq!(report.removed, ["ghost-web-r3"]);
    assert_eq!(w.names(), ["someone-elses"]);
}

#[tokio::test]
async fn stopping_and_starting_an_app() {
    let w = world();
    w.trying();
    w.plumb.pass().await.unwrap();

    w.canon.set_desired("draw", Desired::Stopped).unwrap();
    let report = w.plumb.pass().await.unwrap();
    assert_eq!(report.removed.len(), 2);
    assert!(w.names().is_empty());
    assert_eq!(w.fake.volumes(), ["henry-draw-pg"], "data stays");
    assert_eq!(w.status(), Status::Stopped);

    w.canon.set_desired("draw", Desired::Running).unwrap();
    w.plumb.pass().await.unwrap();
    assert_eq!(w.names().len(), 2);
    assert_eq!(w.status(), Status::Onboarding);
}

#[tokio::test]
async fn failed_starts_back_off_and_are_logged_once() {
    let w = world();
    let r = w.trying();
    w.canon.set_release_state(r, ReleaseState::Live, None).unwrap();
    let web = format!("draw-web-r{r}");
    w.fake.unstartable(&web);

    let report = w.plumb.pass().await.unwrap();
    assert_eq!(report.failed.len(), 1);
    assert_eq!(w.status(), Status::Down);
    assert_eq!(w.canon.app("draw").unwrap().status_detail.as_deref(), Some("web not running"));

    w.fake.take_ops();
    let report = w.plumb.pass().await.unwrap();
    assert!(report.failed.is_empty(), "inside the backoff window nothing is retried");
    assert!(w.fake.take_ops().is_empty());
    let logged =
        w.canon.events(Some("draw"), 100).unwrap().into_iter().filter(|e| e.kind == "container.failed").count();
    assert_eq!(logged, 1);
}

#[tokio::test]
async fn a_retry_after_backoff_recovers() {
    let w = world_with(Settings { backoff: Duration::ZERO, ..Settings::default() });
    let r = w.trying();
    w.canon.set_release_state(r, ReleaseState::Live, None).unwrap();
    let web = format!("draw-web-r{r}");
    w.fake.unstartable(&web);
    w.plumb.pass().await.unwrap();
    assert_eq!(w.status(), Status::Down);

    w.fake.fix_start(&web);
    let report = w.plumb.pass().await.unwrap();
    assert_eq!(report.started, [web]);
    assert_eq!(w.status(), Status::Live);
}

#[tokio::test]
async fn an_image_that_will_not_pull_fails_only_its_container() {
    let w = world();
    w.trying();
    w.fake.unpullable("draw-web:1");
    let report = w.plumb.pass().await.unwrap();
    assert_eq!(report.created, ["draw-db"]);
    assert_eq!(report.failed.len(), 1);
}

#[tokio::test]
async fn a_release_without_an_image_waits_for_its_build() {
    let w = world();
    let text = LEASE.replace("build = { image = \"draw-web:1\" }", "build = { dockerfile = \"Dockerfile\" }");
    w.register(&text);
    w.secret("TOKEN", "x");
    let r = w.release("draw", &text, ReleaseState::Trial);
    w.plumb.pass().await.unwrap();
    assert_eq!(w.names(), ["draw-db"], "web has no image until Draft builds one");

    w.canon.set_release_images(r, &serde_json::json!({ "web": "draw-web:built" })).unwrap();
    w.plumb.pass().await.unwrap();
    assert_eq!(w.fake.container(&format!("draw-web-r{r}")).unwrap().spec.image, "draw-web:built");
}

#[tokio::test]
async fn nothing_starts_before_what_it_depends_on() {
    let w = world_with(Settings { backoff: Duration::ZERO, ..Settings::default() });
    let r = w.trying();
    w.fake.unstartable("draw-db");
    let report = w.plumb.pass().await.unwrap();
    assert_eq!(report.waiting, [(format!("draw-web-r{r}"), vec!["db".to_owned()])]);
    assert!(w.fake.container(&format!("draw-web-r{r}")).is_none(), "web was never created");

    w.fake.fix_start("draw-db");
    w.plumb.pass().await.unwrap();
    assert!(w.fake.containers().iter().all(|c| c.state == State::Running));
    assert_eq!(w.names().len(), 2);

    // A running dependent is left alone when its dependency dies.
    w.fake.crash("draw-db");
    w.fake.unstartable("draw-db");
    w.fake.take_ops();
    w.plumb.pass().await.unwrap();
    assert!(!w.fake.take_ops().iter().any(|op| op.contains("web")));
}
