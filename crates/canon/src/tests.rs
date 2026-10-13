use super::*;

fn lease(name: &str, host: &str) -> (Lease, String) {
    let text = format!(
        "name = \"{name}\"\n[[service]]\nname = \"web\"\nport = 80\nbuild = {{ image = \"x\" }}\nhealth = {{ http = \"/\" }}\npublic = {{ hostname = \"{host}\" }}\n"
    );
    (Lease::parse(&text).unwrap(), text)
}

fn db_with(name: &str, host: &str) -> Canon {
    let db = Canon::open_in_memory().unwrap();
    let (c, t) = lease(name, host);
    db.insert_app(&c, &t, "https://github.com/me/x", Source::Operator).unwrap();
    db
}

#[test]
fn migrates_to_latest() {
    let db = Canon::open_in_memory().unwrap();
    assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
}

#[test]
fn reopening_a_file_keeps_everything() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("canon.db");
    {
        let db = Canon::open(&path).unwrap();
        let (c, t) = lease("draw", "draw.example.com");
        db.insert_app(&c, &t, "repo", Source::Repo).unwrap();
    }
    let db = Canon::open(&path).unwrap();
    assert_eq!(db.app("draw").unwrap().source, Source::Repo);
    assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn a_new_app_starts_onboarding_and_wants_to_run() {
    let db = db_with("draw", "draw.example.com");
    let app = db.app("draw").unwrap();
    assert_eq!(app.status, Status::Onboarding);
    assert_eq!(app.desired, Desired::Running);
    assert_eq!(app.lease().unwrap().name, "draw");
    assert_eq!(db.routes().unwrap()[0].app, "draw");
}

#[test]
fn names_and_routes_are_unique() {
    let db = db_with("draw", "draw.example.com");

    let (c, t) = lease("draw", "other.example.com");
    assert!(matches!(db.insert_app(&c, &t, "r", Source::Operator), Err(Error::AppExists(_))));

    let (c, t) = lease("thief", "draw.example.com");
    let err = db.insert_app(&c, &t, "r", Source::Operator).unwrap_err();
    assert!(matches!(err, Error::RouteTaken { ref app, .. } if app == "draw"), "{err}");
    // The refused insert left nothing behind.
    assert!(matches!(db.app("thief"), Err(Error::NoSuchApp(_))));
}

#[test]
fn a_new_lease_moves_its_routes() {
    let db = db_with("draw", "draw.example.com");
    let (c, t) = lease("draw", "sketch.example.com");
    db.update_lease(&c, &t, Source::Ghost).unwrap();
    let routes = db.routes().unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].hostname, "sketch.example.com");
    assert_eq!(db.app("draw").unwrap().source, Source::Ghost);
}

#[test]
fn unchanged_status_writes_nothing() {
    let db = db_with("draw", "draw.example.com");
    let before = db.events(Some("draw"), 100).unwrap().len();
    assert!(db.set_status("draw", Status::Live, None).unwrap());
    assert!(!db.set_status("draw", Status::Live, None).unwrap());
    assert!(db.set_status("draw", Status::Degraded, Some("2 of 30 probes failed")).unwrap());
    assert_eq!(db.events(Some("draw"), 100).unwrap().len(), before + 2);
    assert_eq!(db.app("draw").unwrap().status_detail.as_deref(), Some("2 of 30 probes failed"));
    assert!(matches!(db.set_status("ghost", Status::Live, None), Err(Error::NoSuchApp(_))));
}

#[test]
fn going_live_supersedes_the_last_release() {
    let db = db_with("draw", "draw.example.com");
    let a = db.create_release("draw", "aaaaaaaaaaaaaaaa", "c").unwrap();
    db.set_release_state(a.id, ReleaseState::Live, None).unwrap();
    assert_eq!(db.app("draw").unwrap().live_release, Some(a.id));

    let b = db.create_release("draw", "bbbbbbbbbbbbbbbb", "c").unwrap();
    db.set_release_state(b.id, ReleaseState::Building, None).unwrap();
    assert_eq!(db.app("draw").unwrap().live_release, Some(a.id), "building does not displace live");

    let b = db.set_release_state(b.id, ReleaseState::Live, None).unwrap();
    assert!(b.finished_at.is_some());
    assert_eq!(db.app("draw").unwrap().live_release, Some(b.id));
    assert_eq!(db.release(a.id).unwrap().state, ReleaseState::Superseded);

    let ids: Vec<i64> = db.releases("draw", 10).unwrap().iter().map(|r| r.id).collect();
    assert_eq!(ids, [b.id, a.id]);
}

#[test]
fn a_failed_release_keeps_its_reason() {
    let db = db_with("draw", "draw.example.com");
    let r = db.create_release("draw", "cccccccccc", "c").unwrap();
    let r = db.set_release_state(r.id, ReleaseState::Failed, Some("health probe timed out")).unwrap();
    assert_eq!(r.reason.as_deref(), Some("health probe timed out"));
    assert_eq!(db.app("draw").unwrap().live_release, None);
}

#[test]
fn duplicate_jobs_collapse() {
    let db = db_with("draw", "draw.example.com");
    let a = db.enqueue("deploy", Some("draw"), &serde_json::json!({ "sha": "1" })).unwrap();
    let b = db.enqueue("deploy", Some("draw"), &serde_json::json!({ "sha": "1" })).unwrap();
    assert_eq!(a.id, b.id);
    let other = db.enqueue("deploy", Some("odyssey"), &serde_json::Value::Null).unwrap();
    assert_ne!(a.id, other.id);
}

#[test]
fn jobs_survive_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("canon.db");
    let id = {
        let db = Canon::open(&path).unwrap();
        let job = db.enqueue("deploy", None, &serde_json::Value::Null).unwrap();
        let claimed = db.claim().unwrap().unwrap();
        assert_eq!(claimed.id, job.id);
        assert_eq!(claimed.state, JobState::Running);
        assert!(db.claim().unwrap().is_none(), "a running job is not handed out twice");
        job.id
        // Dropped mid-job: the process "crashed".
    };
    let db = Canon::open(&path).unwrap();
    assert_eq!(db.requeue_interrupted().unwrap(), 1);
    let again = db.claim().unwrap().unwrap();
    assert_eq!(again.id, id);
    assert_eq!(again.attempts, 2);
    db.finish(id).unwrap();
    assert_eq!(db.job(id).unwrap().state, JobState::Done);
    assert_eq!(db.requeue_interrupted().unwrap(), 0);
}

#[test]
fn failures_retry_later_or_stop() {
    let db = Canon::open_in_memory().unwrap();
    let job = db.enqueue("build", None, &serde_json::Value::Null).unwrap();
    db.claim().unwrap();
    db.fail(job.id, "registry timed out", Some(now_ms() + 60_000)).unwrap();
    let j = db.job(job.id).unwrap();
    assert_eq!(j.state, JobState::Queued);
    assert_eq!(j.error.as_deref(), Some("registry timed out"));
    assert!(db.claim().unwrap().is_none(), "not due yet");

    let job = db.enqueue("other", None, &serde_json::Value::Null).unwrap();
    db.claim().unwrap();
    db.fail(job.id, "bad lease", None).unwrap();
    assert_eq!(db.job(job.id).unwrap().state, JobState::Failed);
    assert!(db.fail(job.id, "again", None).is_err(), "only a running job can be settled");
}

#[test]
fn removing_an_app_takes_its_secrets_and_routes() {
    let db = db_with("draw", "draw.example.com");
    db.put_sealed("draw", "TOKEN", b"ciphertext").unwrap();
    db.put_sealed("", "ANTHROPIC_API_KEY", b"other").unwrap();
    db.create_release("draw", "abc", "c").unwrap();
    db.remove_app("draw").unwrap();
    assert!(db.routes().unwrap().is_empty());
    assert!(db.sealed_names("draw").unwrap().is_empty());
    assert_eq!(db.sealed_names("").unwrap(), ["ANTHROPIC_API_KEY"]);
    assert!(db.releases("draw", 10).unwrap().is_empty());
}

#[test]
fn sealed_values_overwrite_in_place() {
    let db = Canon::open_in_memory().unwrap();
    db.put_sealed("app", "K", b"one").unwrap();
    db.put_sealed("app", "K", b"two").unwrap();
    assert_eq!(db.get_sealed("app", "K").unwrap().as_deref(), Some(&b"two"[..]));
    assert!(db.delete_sealed("app", "K").unwrap());
    assert!(!db.delete_sealed("app", "K").unwrap());
    assert_eq!(db.get_sealed("app", "K").unwrap(), None);
}

#[test]
fn snapshots_are_complete_copies() {
    let dir = tempfile::tempdir().unwrap();
    let db = db_with("draw", "draw.example.com");
    let copy = dir.path().join("copy.db");
    db.snapshot_to(&copy).unwrap();
    let restored = Canon::open(&copy).unwrap();
    assert_eq!(restored.app("draw").unwrap().name, "draw");
}

#[test]
fn wanted_releases_are_live_plus_the_newest_trial() {
    let db = db_with("draw", "draw.example.com");
    assert_eq!(db.wanted_releases("draw").unwrap(), Wanted::default());

    let a = db.create_release("draw", "a", "c").unwrap();
    db.set_release_state(a.id, ReleaseState::Trial, None).unwrap();
    let w = db.wanted_releases("draw").unwrap();
    assert_eq!((w.live, w.candidate.map(|r| r.id)), (None, Some(a.id)));

    db.set_release_state(a.id, ReleaseState::Live, None).unwrap();
    let b = db.create_release("draw", "b", "c").unwrap();
    db.set_release_state(b.id, ReleaseState::Building, None).unwrap();
    let w = db.wanted_releases("draw").unwrap();
    assert_eq!(w.live.map(|r| r.id), Some(a.id));
    assert_eq!(w.candidate, None, "a release still building has nothing to run");

    db.set_release_state(b.id, ReleaseState::Trial, None).unwrap();
    let c = db.create_release("draw", "c", "c").unwrap();
    db.set_release_state(c.id, ReleaseState::Proof, None).unwrap();
    assert_eq!(db.wanted_releases("draw").unwrap().candidate.map(|r| r.id), Some(c.id), "only the newest is tried");

    db.set_release_state(c.id, ReleaseState::Failed, Some("soak")).unwrap();
    assert_eq!(db.wanted_releases("draw").unwrap().candidate.map(|r| r.id), Some(b.id));
}

#[test]
fn ports_are_stable_unique_and_freed() {
    let db = db_with("draw", "draw.example.com");
    let (c, t) = lease_without_route("odyssey");
    db.insert_app(&c, &t, "r", Source::Operator).unwrap();

    let a = db.port_for("draw", "api", 1, 20000..20003).unwrap();
    assert_eq!(a, 20000);
    assert_eq!(db.port_for("draw", "api", 1, 20000..20003).unwrap(), a, "same slot, same port");
    assert_eq!(db.port_for("draw", "api", 2, 20000..20003).unwrap(), 20001);
    assert_eq!(db.port_for("odyssey", "web", 0, 20000..20003).unwrap(), 20002);
    assert!(matches!(db.port_for("draw", "api", 3, 20000..20003), Err(Error::PortsExhausted)));

    assert_eq!(db.free_ports("draw", &[("api".into(), 2)]).unwrap(), 1);
    assert_eq!(db.port_for("draw", "api", 3, 20000..20003).unwrap(), 20000, "a freed port is reused");

    db.remove_app("odyssey").unwrap();
    assert_eq!(db.port_for("draw", "worker", 0, 20000..20003).unwrap(), 20002, "removal frees an app's ports");
}

#[test]
fn secret_versions_move_when_set_again() {
    let db = Canon::open_in_memory().unwrap();
    db.put_sealed("a", "K", b"1").unwrap();
    let first = db.sealed_versions("a").unwrap()["K"];
    std::thread::sleep(std::time::Duration::from_millis(2));
    db.put_sealed("a", "K", b"2").unwrap();
    assert!(db.sealed_versions("a").unwrap()["K"] > first);
    assert!(db.sealed_versions("b").unwrap().is_empty());
}

fn lease_without_route(name: &str) -> (Lease, String) {
    let text = format!(
        "name = \"{name}\"\n[[service]]\nname = \"web\"\nport = 80\nbuild = {{ image = \"x\" }}\nhealth = {{ tcp = true }}\n"
    );
    (Lease::parse(&text).unwrap(), text)
}

#[test]
fn releases_are_found_by_commit_and_superseded_by_newer_ones() {
    let db = db_with("draw", "draw.example.com");
    assert_eq!(db.release_for_commit("draw", "aaa").unwrap(), None);
    let a = db.create_release("draw", "aaa", "c").unwrap();
    db.set_release_state(a.id, ReleaseState::Trial, None).unwrap();
    let b = db.create_release("draw", "bbb", "c").unwrap();
    assert_eq!(db.release_for_commit("draw", "aaa").unwrap().map(|r| r.id), Some(a.id));

    let live = db.create_release("draw", "ccc", "c").unwrap();
    db.set_release_state(live.id, ReleaseState::Live, None).unwrap();
    assert_eq!(db.supersede_trials("draw", b.id).unwrap(), 1, "only the trial goes; live and the kept one stay");
    assert_eq!(db.release(a.id).unwrap().state, ReleaseState::Superseded);
    assert_eq!(db.release(b.id).unwrap().state, ReleaseState::Pending);
    assert_eq!(db.release(live.id).unwrap().state, ReleaseState::Live);
}
