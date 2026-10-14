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
fn proof_progress_is_saved_per_release() {
    let db = db_with("draw", "draw.example.com");
    let r = db.create_release("draw", "a", "c").unwrap();
    assert_eq!(db.proof(r.id).unwrap(), None);
    db.put_proof(r.id, "draw", &serde_json::json!({ "stage": "soak", "streak": 3 })).unwrap();
    db.put_proof(r.id, "draw", &serde_json::json!({ "stage": "soak", "streak": 4 })).unwrap();
    assert_eq!(db.proof(r.id).unwrap().unwrap()["streak"], 4);
    db.remove_app("draw").unwrap();
    assert_eq!(db.proof(r.id).unwrap(), None, "proofs go with their release");
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

#[test]
fn a_draft_walks_its_stages_and_logs_each_once() {
    let db = Canon::open_in_memory().unwrap();
    let d = db.start_draft("draw", "https://github.com/me/draw", None, Some("draw.example.com")).unwrap();
    assert_eq!((d.stage, d.round, d.spent), (DraftStage::Survey, 0, 0));
    assert_eq!(db.next_draft_round("draw").unwrap(), 1);
    db.set_draft_stage("draw", DraftStage::Plan, None).unwrap();
    db.set_draft_stage("draw", DraftStage::Plan, None).unwrap();
    assert_eq!(db.add_draft_spend("draw", 1200).unwrap(), 1200);
    assert_eq!(db.add_draft_spend("draw", 300).unwrap(), 1500);
    db.set_draft_release("draw", Some(7)).unwrap();
    let d = db.draft("draw").unwrap().unwrap();
    assert_eq!((d.stage, d.round, d.release), (DraftStage::Plan, 1, Some(7)));
    let stages = db.events(Some("draw"), 10).unwrap().iter().filter(|e| e.kind == "draft.stage").count();
    assert_eq!(stages, 1, "an unchanged stage is not logged again");
}

#[test]
fn only_a_stuck_draft_can_start_over() {
    let db = db_with("draw", "draw.example.com");
    assert!(matches!(db.start_draft("draw", "repo", None, None), Err(Error::AppExists(_))), "an app has the name");

    db.start_draft("site", "repo", None, None).unwrap();
    assert!(matches!(db.start_draft("site", "repo", None, None), Err(Error::AppExists(_))), "still onboarding");
    db.next_draft_round("site").unwrap();
    db.set_draft_stage("site", DraftStage::Stuck, Some("out of rounds")).unwrap();
    let d = db.start_draft("site", "repo2", Some("dev"), None).unwrap();
    assert_eq!((d.stage, d.round, d.repo.as_str(), d.branch.as_deref()), (DraftStage::Survey, 0, "repo2", Some("dev")));
}

#[test]
fn proposals_keep_every_plan_and_its_verdict() {
    let db = Canon::open_in_memory().unwrap();
    let files = serde_json::json!({ ".henry/Dockerfile.web": "FROM scratch" });
    let generate = serde_json::json!([{ "name": "POSTGRES_PASSWORD" }]);
    let first = db
        .add_proposal(&NewProposal {
            app: "draw",
            round: 1,
            source: "ghost",
            model: Some("claude/claude-sonnet-5-5"),
            lease: "name = \"draw\"",
            files: &files,
            generate: &generate,
            notes: "a guess",
        })
        .unwrap();
    db.set_proposal_verdict(first, "did not build").unwrap();
    let all = db.proposals("draw", 10).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].verdict.as_deref(), Some("did not build"));
    assert_eq!(all[0].files, files);
    assert_eq!(all[0].generate, generate);
}

#[test]
fn overlays_are_replaced_whole_and_go_with_the_app() {
    let db = db_with("draw", "draw.example.com");
    db.set_overlays("draw", &[(".henry/a".into(), "1".into()), (".henry/b".into(), "2".into())]).unwrap();
    db.set_overlays("draw", &[(".henry/b".into(), "3".into())]).unwrap();
    assert_eq!(db.overlays("draw").unwrap(), [(".henry/b".to_owned(), "3".to_owned())]);
    db.start_draft("other", "repo", None, None).unwrap();
    db.remove_app("draw").unwrap();
    assert!(db.overlays("draw").unwrap().is_empty());
}

#[test]
fn backups_and_drills_remember_the_last_good_one() {
    let db = Canon::open_in_memory().unwrap();
    db.record_backup("draw", "db/pg", Some("aaaa1111"), Some(100), None).unwrap();
    db.record_backup("draw", "db/pg", None, None, Some("pg_dumpall: connection refused")).unwrap();
    assert_eq!(db.last_good_backup("draw", "db/pg").unwrap().unwrap().snapshot.as_deref(), Some("aaaa1111"));
    assert!(db.last_good_backup("draw", "web/data").unwrap().is_none());
    assert_eq!(db.backups("draw", 10).unwrap().len(), 2);
    let failed = db.events(Some("draw"), 5).unwrap();
    assert_eq!(failed.len(), 1, "only the failure is an event");
    assert!(failed[0].message.contains("connection refused"));

    db.record_drill("draw", "db/pg", "aaaa1111", true, "3 databases").unwrap();
    db.record_drill("draw", "db/pg", "bbbb2222", false, "psql exited 2").unwrap();
    assert!(!db.last_drill("draw", "db/pg").unwrap().unwrap().ok);
    assert_eq!(db.last_good_drill("draw", "db/pg").unwrap().unwrap().snapshot, "aaaa1111");
}

#[test]
fn upkeep_tools_and_bench_are_remembered() {
    let db = Canon::open_in_memory().unwrap();
    db.record_upkeep("disk", true, "freed 2 images").unwrap();
    db.record_upkeep("disk", false, "engine away").unwrap();
    let u = db.upkeep("disk").unwrap().unwrap();
    assert!(!u.ok);
    assert_eq!(db.upkeeps().unwrap().len(), 1, "one row per task");
    assert_eq!(db.events(None, 5).unwrap()[0].kind, "mend.failed");

    db.set_tool("caddy", "2.10.2", None, None).unwrap();
    db.set_tool("caddy", "2.10.2", None, Some("2.11.0")).unwrap();
    db.set_tool("caddy", "2.11.0", Some("2.10.2"), None).unwrap();
    let t = db.tool("caddy").unwrap().unwrap();
    assert_eq!((t.current.as_str(), t.previous.as_deref(), t.pending), ("2.11.0", Some("2.10.2"), None));
    let changes = db.events(None, 10).unwrap().iter().filter(|e| e.kind == "mend.tool").count();
    assert_eq!(changes, 2, "only changes of the current version are events");

    db.record_bench("meta/a", 5, 7, 900, &serde_json::json!({})).unwrap();
    db.record_bench("claude/b", 7, 7, 1200, &serde_json::json!({})).unwrap();
    db.record_bench("meta/a", 6, 7, 800, &serde_json::json!({})).unwrap();
    let latest: Vec<(String, i64)> = db.bench_latest().unwrap().into_iter().map(|b| (b.model, b.passed)).collect();
    assert_eq!(latest, [("claude/b".to_owned(), 7), ("meta/a".to_owned(), 6)]);
}

#[test]
fn status_history_starts_with_the_status_held_then() {
    let db = db_with("draw", "draw.example.com");
    db.set_status("draw", Status::Live, None).unwrap();
    // An event written before statuses carried data reads from its message.
    db.record(Some("draw"), "app.status", "degraded: slow", &serde_json::Value::Null).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let since = now_ms();
    db.set_status("draw", Status::Down, Some("exited")).unwrap();
    let history: Vec<Status> = db.status_history("draw", since).unwrap().into_iter().map(|(_, s)| s).collect();
    assert_eq!(history, [Status::Degraded, Status::Down]);
    assert_eq!(db.status_history("draw", 0).unwrap().len(), 3);
    assert!(db.status_history("other", 0).unwrap().is_empty());
}

#[test]
fn events_after_returns_only_what_is_new() {
    let db = db_with("draw", "draw.example.com");
    let seen = db.events(None, 1).unwrap()[0].id;
    db.record(Some("draw"), "x", "one", &serde_json::Value::Null).unwrap();
    db.record(None, "y", "two", &serde_json::Value::Null).unwrap();
    let all: Vec<String> = db.events_after(seen, None, 10).unwrap().into_iter().map(|e| e.message).collect();
    assert_eq!(all, ["one", "two"]);
    assert_eq!(db.events_after(seen, Some("draw"), 10).unwrap().len(), 1);
}

#[test]
fn an_adoption_remembers_the_record_its_cutover_replaced() {
    let db = Canon::open_in_memory().unwrap();
    assert!(db.adoption("odyssey").unwrap().is_none());
    db.put_adoption("odyssey", Some("odyssey-data"), Some("odyssey-data")).unwrap();
    db.set_seeded("odyssey", 42).unwrap();
    let old = serde_json::json!({ "type": "CNAME", "content": "old.cfargotunnel.com" });
    db.set_cutover("odyssey", Some(&old)).unwrap();
    db.put_adoption("odyssey", Some("odyssey-data"), Some("odyssey-data")).unwrap();
    let a = db.adoption("odyssey").unwrap().unwrap();
    assert!(a.seeded_at.is_some() && a.cutover_at.is_some(), "adopting again keeps the cutover");
    assert_eq!(a.dns, Some(old));
    db.clear_cutover("odyssey").unwrap();
    let a = db.adoption("odyssey").unwrap().unwrap();
    assert_eq!((a.cutover_at, a.dns), (None, None));
    assert_eq!(db.events(Some("odyssey"), 1).unwrap()[0].kind, "adopt.back");
}

#[test]
fn a_workers_bundle_rides_with_its_release_and_its_deploy_outlives_the_app() {
    let db = db_with("cairn", "cairn.example.com");
    let r = db.create_release("cairn", "abc", "lease").unwrap();
    let bundle = serde_json::json!({ "main": "worker.js", "modules": { "worker.js": "export default {}" } });
    db.put_worker_bundle(r.id, "sync", "h1", &bundle).unwrap();
    let next = db.create_release("cairn", "abc", "lease").unwrap();
    db.copy_worker_bundles(r.id, next.id).unwrap();
    assert_eq!(db.worker_bundles(next.id).unwrap(), db.worker_bundles(r.id).unwrap());
    assert_eq!(db.worker_bundles(next.id).unwrap()[0].bundle, bundle);

    db.claim_worker("cairn", "sync", "henry-cairn-sync").unwrap();
    db.set_worker_failed("cairn", "sync", "403").unwrap();
    db.set_worker_failed("cairn", "sync", "403").unwrap();
    let failures = db.events(Some("cairn"), 50).unwrap().iter().filter(|e| e.kind == "worker.failed").count();
    assert_eq!(failures, 1, "the same failure is recorded once");
    db.set_worker_deployed("cairn", "sync", "https://henry-cairn-sync.me.workers.dev", "h1", next.id).unwrap();
    let d = &db.worker_deploys().unwrap()[0];
    assert_eq!((d.error.as_deref(), d.deployed.as_deref(), d.release), (None, Some("h1"), Some(next.id)));

    db.remove_app("cairn").unwrap();
    assert!(db.worker_bundles(next.id).unwrap().is_empty(), "bundles go with the app's releases");
    assert_eq!(db.worker_deploys().unwrap().len(), 1, "the script is still on Cloudflare until Hinge removes it");
    db.forget_worker("cairn", "sync").unwrap();
    assert!(db.worker_deploys().unwrap().is_empty());
}
