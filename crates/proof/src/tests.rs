//! Proof against the fake runtime, with Plumb creating the containers and
//! real responders listening on the ports Plumb hands out.

use std::sync::atomic::{AtomicU16, Ordering};

use canon::Source;
use hold::fake::Fake;
use hush::{Hush, Key};
use plumb::Plumb;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;

const LEASE: &str = r#"
name = "draw"

[[service]]
name = "web"
port = 8080
build = { image = "web:1" }
health = { http = "/health" }
public = { hostname = "draw.example.com" }
depends_on = ["db"]

[[service]]
name = "db"
port = 5432
build = { image = "db:1" }
health = { tcp = true }

[[service.volume]]
name = "data"
mount = "/data"
"#;

/// An HTTP responder whose status code can be changed, or a plain TCP
/// listener when `status` is `None`.
async fn responder(port: u16, status: Option<Arc<AtomicU16>>) {
    let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { return };
            let status = status.clone();
            tokio::spawn(async move {
                let Some(status) = status else { return };
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf).await;
                let code = status.load(Ordering::SeqCst);
                let reply = format!("HTTP/1.1 {code} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
                let _ = s.write_all(reply.as_bytes()).await;
            });
        }
    });
}

struct World {
    canon: Arc<Canon>,
    fake: Arc<Fake>,
    plumb: Plumb<Arc<Fake>, Arc<Fake>>,
    proof: Proof<Arc<Fake>>,
    web_status: Arc<AtomicU16>,
    settings: Settings,
}

fn fast() -> Settings {
    Settings {
        soak: Duration::ZERO,
        streak: 2,
        soak_limit: Duration::from_secs(60),
        start_timeout: Duration::from_secs(60),
        settle_streak: 1,
        settle_timeout: Duration::from_secs(60),
        probe_timeout: Duration::from_millis(500),
        degraded_after: 2,
        restart_after: 3,
        restart_cooldown: Duration::from_secs(60),
        ..Settings::default()
    }
}

/// A unique port range per test, so parallel tests never share responders.
fn ports() -> std::ops::Range<u16> {
    static NEXT: std::sync::atomic::AtomicU16 = AtomicU16::new(0);
    let base = 42000 + (std::process::id() % 200) as u16 * 100 + NEXT.fetch_add(10, Ordering::SeqCst);
    base..base + 10
}

fn world_with(settings: Settings) -> World {
    let canon = Arc::new(Canon::open_in_memory().unwrap());
    let hush = Arc::new(Hush::new(canon.clone(), &Key::generate().unwrap()));
    let fake = Arc::new(Fake::new());
    let plumb_settings = plumb::Settings { ports: ports(), ..plumb::Settings::default() };
    let plumb = Plumb::new(canon.clone(), hush, fake.clone(), fake.clone(), plumb_settings);
    let proof = Proof::new(canon.clone(), fake.clone(), settings.clone());
    canon.insert_app(&Lease::parse(LEASE).unwrap(), LEASE, "repo", Source::Operator).unwrap();
    World { canon, fake, plumb, proof, web_status: Arc::new(AtomicU16::new(200)), settings }
}

fn world() -> World {
    world_with(fast())
}

impl World {
    /// A release on trial, its containers running and its ports answering.
    async fn trying(&self, lease: &str) -> i64 {
        let r = self.canon.create_release("draw", "sha", lease).unwrap();
        self.canon.set_release_state(r.id, ReleaseState::Trial, None).unwrap();
        self.plumb.pass().await.unwrap();
        let web = self.canon.existing_port("draw", "web", r.id).unwrap().unwrap();
        responder(web, Some(self.web_status.clone())).await;
        if let Some(db) = self.canon.existing_port("draw", "db", 0).unwrap()
            && tokio::net::TcpStream::connect(("127.0.0.1", db)).await.is_err()
        {
            responder(db, None).await;
        }
        r.id
    }

    async fn passes(&self, n: usize) -> Report {
        let mut last = Report::default();
        for _ in 0..n {
            self.plumb.pass().await.unwrap();
            last = self.proof.pass().await.unwrap();
        }
        last
    }

    fn release(&self, id: i64) -> Release {
        self.canon.release(id).unwrap()
    }

    fn status(&self) -> (Status, Option<String>) {
        let a = self.canon.app("draw").unwrap();
        (a.status, a.status_detail)
    }

    fn reason(&self, id: i64) -> String {
        self.release(id).reason.unwrap_or_default()
    }
}

#[tokio::test]
async fn a_healthy_release_is_proven_and_goes_live() {
    let w = world();
    let r = w.trying(LEASE).await;
    w.passes(1).await;
    assert_eq!(w.release(r).state, ReleaseState::Proof, "all running: the soak begins");
    w.fake.take_ops();
    w.passes(2).await;
    let ops = w.fake.take_ops();
    assert!(ops.contains(&format!("stop draw-web-r{r}")) && ops.contains(&format!("start draw-web-r{r}")), "{ops:?}");
    assert!(ops.contains(&"stop draw-db".to_owned()), "with nothing live yet, the database is restart-tested too");

    let report = w.passes(1).await;
    assert_eq!(report.promoted, [("draw".to_owned(), r)]);
    let release = w.release(r);
    assert_eq!(release.state, ReleaseState::Live);
    assert!(release.reason.unwrap().starts_with("proven:"));
    assert_eq!(w.status().0, Status::Live);
    assert_eq!(w.canon.app("draw").unwrap().live_release, Some(r));
}

#[tokio::test]
async fn probes_that_never_hold_fail_the_release() {
    let w = world_with(Settings { soak_limit: Duration::ZERO, ..fast() });
    w.web_status.store(503, Ordering::SeqCst);
    let r = w.trying(LEASE).await;
    let report = w.passes(2).await;
    assert_eq!(report.failed.len(), 1);
    assert_eq!(w.release(r).state, ReleaseState::Failed);
    assert!(w.reason(r).contains("never held"), "{}", w.reason(r));
    assert!(w.reason(r).contains("503"), "the last failure is named: {}", w.reason(r));
    let (status, detail) = w.status();
    assert_eq!(status, Status::Stuck, "an app with nothing live is stuck, and says why");
    assert!(detail.unwrap().contains("never held"));
}

#[tokio::test]
async fn a_crash_during_the_soak_fails_it() {
    let w = world();
    let r = w.trying(LEASE).await;
    w.passes(1).await;
    w.fake.bounce(&format!("draw-web-r{r}"));
    w.passes(1).await;
    assert_eq!(w.release(r).state, ReleaseState::Failed);
    assert!(w.reason(r).contains("`web` crashed and was restarted 1 time(s)"), "{}", w.reason(r));
}

#[tokio::test]
async fn a_crash_in_the_logs_fails_it() {
    let w = world();
    let r = w.trying(LEASE).await;
    w.passes(1).await;
    w.fake.log(&format!("draw-web-r{r}"), "thread 'main' panicked at src/main.rs:12:5: index out of bounds");
    w.passes(1).await;
    assert_eq!(w.release(r).state, ReleaseState::Failed);
    assert!(w.reason(r).contains("`web` logged a crash: thread 'main' panicked"), "{}", w.reason(r));
}

#[tokio::test]
async fn running_out_of_memory_fails_it() {
    let w = world();
    let r = w.trying(LEASE).await;
    w.passes(1).await;
    w.fake.oom("draw-db");
    w.proof.pass().await.unwrap();
    assert!(w.reason(r).contains("`db` was killed for using too much memory"), "{}", w.reason(r));
}

#[tokio::test]
async fn a_release_that_never_comes_up_fails() {
    let w = world_with(Settings { start_timeout: Duration::ZERO, ..fast() });
    let r = w.canon.create_release("draw", "sha", LEASE).unwrap().id;
    w.canon.set_release_state(r, ReleaseState::Trial, None).unwrap();
    w.proof.pass().await.unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    w.proof.pass().await.unwrap();
    assert!(w.reason(r).contains("never came up: web, db not running"), "{}", w.reason(r));
}

#[tokio::test]
async fn a_failed_candidate_leaves_the_live_release_alone() {
    let w = world_with(Settings { soak_limit: Duration::ZERO, ..fast() });
    let r1 = w.trying(LEASE).await;
    w.canon.set_release_state(r1, ReleaseState::Live, None).unwrap();
    w.canon.set_status("draw", Status::Live, None).unwrap();

    let next = LEASE.replace("web:1", "web:2");
    let r2 = w.canon.create_release("draw", "sha2", &next).unwrap().id;
    w.canon.set_release_state(r2, ReleaseState::Trial, None).unwrap();
    w.passes(3).await; // r2's port has no responder: its probes fail.
    assert_eq!(w.release(r2).state, ReleaseState::Failed);
    assert_eq!(w.canon.app("draw").unwrap().live_release, Some(r1));
    assert_eq!(w.status().0, Status::Live);
}

#[tokio::test]
async fn a_shared_database_is_not_restarted_to_test_a_candidate() {
    let w = world();
    let r1 = w.trying(LEASE).await;
    w.passes(4).await;
    assert_eq!(w.release(r1).state, ReleaseState::Live);

    let r2 = w.trying(&LEASE.replace("web:1", "web:2")).await;
    w.passes(1).await;
    w.fake.take_ops();
    w.passes(2).await;
    let ops = w.fake.take_ops();
    assert!(ops.contains(&format!("stop draw-web-r{r2}")), "{ops:?}");
    assert!(!ops.iter().any(|o| o.contains("draw-db")), "the live release's database was left alone: {ops:?}");
    w.passes(1).await;
    assert_eq!(w.release(r2).state, ReleaseState::Live);
    assert_eq!(w.release(r1).state, ReleaseState::Superseded);
}

#[tokio::test]
async fn a_trial_resumes_after_a_restart_of_henry() {
    let w = world();
    let r = w.trying(LEASE).await;
    w.passes(2).await; // into the soak, one probe in
    let fresh = Proof::new(w.canon.clone(), w.fake.clone(), w.settings.clone());
    w.plumb.pass().await.unwrap();
    fresh.pass().await.unwrap(); // second probe: restart test
    w.plumb.pass().await.unwrap();
    let report = fresh.pass().await.unwrap();
    assert_eq!(report.promoted, [("draw".to_owned(), r)], "the saved streak carried over");
}

#[tokio::test]
async fn a_live_app_that_stops_answering_is_degraded_restarted_and_recovers() {
    let w = world();
    let r = w.trying(LEASE).await;
    w.passes(4).await;
    assert_eq!(w.release(r).state, ReleaseState::Live);

    w.web_status.store(500, Ordering::SeqCst);
    w.passes(1).await;
    assert_eq!(w.status().0, Status::Live, "one failure is not a pattern");
    let report = w.passes(1).await;
    assert_eq!(report.degraded, ["draw"]);
    let (status, detail) = w.status();
    assert_eq!(status, Status::Degraded);
    assert!(detail.unwrap().contains("`web` failing its health check"));

    w.fake.take_ops();
    let report = w.passes(1).await;
    assert_eq!(report.restarted, [format!("draw-web-r{r}")]);
    let report = w.passes(1).await;
    assert!(report.restarted.is_empty(), "one restart per cooldown");
    assert_eq!(w.status().0, Status::Degraded, "Plumb does not paper over a failing app");

    w.web_status.store(200, Ordering::SeqCst);
    let report = w.passes(1).await;
    assert_eq!(report.recovered, ["draw"]);
    assert_eq!(w.status(), (Status::Live, None));
}
