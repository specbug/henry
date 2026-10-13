//! Ark against a real engine and a real restic, with a local repository:
//! back up each kind of volume, drill each snapshot, and restore two of them
//! over their live volumes.
//!
//! Runs when `HENRY_ENGINE_TESTS=1` and `HENRY_RESTIC` names a restic binary.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ark::engine::Engine;
use ark::{Ark, HENRY, Restic, Settings};
use canon::{Canon, ReleaseState, Source};
use hush::Secret;
use lease::Lease;

const APP: &str = "arkt";

const LEASE: &str = r#"
name = "arkt"

[[service]]
name = "files"
port = 80
build = { image = "alpine:3.20" }
health = { tcp = true }

[[service.volume]]
name = "data"
mount = "/data"

[[service]]
name = "lite"
port = 80
build = { image = "alpine:3.20" }
health = { tcp = true }

[[service.volume]]
name = "lite"
mount = "/data"
backup = "sqlite:db/app.db"

[[service]]
name = "db"
port = 5432
build = { image = "postgres:17-alpine" }
health = { postgres = true }
env = { POSTGRES_USER = "app" }

[[service.volume]]
name = "pg"
mount = "/var/lib/postgresql/data"
backup = "postgres"

[[service]]
name = "cache"
port = 6379
build = { image = "redis:7-alpine" }
health = { redis = true }

[[service.volume]]
name = "rd"
mount = "/data"
backup = "redis"
"#;

fn program() -> Option<(String, PathBuf)> {
    if std::env::var("HENRY_ENGINE_TESTS").ok().as_deref() != Some("1") {
        return None;
    }
    let restic = PathBuf::from(std::env::var_os("HENRY_RESTIC")?);
    let engine = if std::process::Command::new("docker").arg("info").output().is_ok_and(|o| o.status.success()) {
        "docker"
    } else {
        "podman"
    };
    Some((engine.to_owned(), restic))
}

fn sh(engine: &str, args: &[&str]) -> String {
    let out = std::process::Command::new(engine).args(args).output().unwrap();
    assert!(out.status.success(), "{engine} {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn cleanup(engine: &str) {
    for c in ["files", "lite", "db", "cache"] {
        let _ = std::process::Command::new(engine).args(["rm", "-f", &format!("{APP}-{c}")]).output();
    }
    for v in ["data", "lite", "pg", "rd"] {
        let _ = std::process::Command::new(engine).args(["volume", "rm", "-f", &format!("henry-{APP}-{v}")]).output();
    }
}

fn wait(engine: &str, args: &[&str]) {
    for _ in 0..120 {
        if std::process::Command::new(engine).args(args).output().is_ok_and(|o| o.status.success()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("{args:?} never succeeded");
}

fn start_postgres(engine: &str) {
    sh(
        engine,
        &[
            "run",
            "-d",
            "--name",
            "arkt-db",
            "-e",
            "POSTGRES_PASSWORD=pw",
            "-e",
            "POSTGRES_USER=app",
            "-v",
            "henry-arkt-pg:/var/lib/postgresql/data",
            "postgres:17-alpine",
        ],
    );
    wait(engine, &["exec", "arkt-db", "pg_isready", "-h", "127.0.0.1", "-U", "app"]);
}

#[tokio::test]
async fn every_kind_of_volume_is_backed_up_drilled_and_restored() {
    let Some((engine, restic)) = program() else { return };
    cleanup(&engine);
    let dir = tempfile::tempdir().unwrap();

    let canon = Arc::new(Canon::open_in_memory().unwrap());
    let lease = Lease::parse(LEASE).unwrap();
    canon.insert_app(&lease, LEASE, "repo", Source::Operator).unwrap();
    let release = canon.create_release(APP, "sha", LEASE).unwrap();
    let images = serde_json::json!({
        "files": "alpine:3.20", "lite": "alpine:3.20", "db": "postgres:17-alpine", "cache": "redis:7-alpine"
    });
    canon.set_release_images(release.id, &images).unwrap();
    canon.set_release_state(release.id, ReleaseState::Live, None).unwrap();

    let restic = Restic {
        program: restic,
        repository: dir.path().join("repo").to_string_lossy().into_owned(),
        password: Secret::new("correct horse"),
        env: vec![],
        cache: Some(dir.path().join("cache")),
        timeout: Duration::from_secs(300),
    };
    let tool = Engine {
        program: engine.clone().into(),
        helper: "henry/ark-helper:test".into(),
        timeout: Duration::from_secs(300),
    };
    tool.ensure_helper().await.unwrap();
    let settings = Settings {
        scratch: dir.path().to_path_buf(),
        config: None,
        patience: Duration::from_secs(90),
        ..Settings::default()
    };
    let ark = Ark::new(canon.clone(), restic, vec![], tool, settings);

    // The volumes, as the apps would have left them.
    sh(&engine, &["run", "-d", "--name", "arkt-files", "-v", "henry-arkt-data:/data", "alpine:3.20", "sleep", "600"]);
    sh(
        &engine,
        &["exec", "arkt-files", "sh", "-c", "mkdir -p /data/up && echo one > /data/a && echo two > /data/up/b"],
    );
    sh(
        &engine,
        &[
            "run",
            "--rm",
            "-v",
            "henry-arkt-lite:/v",
            "henry/ark-helper:test",
            "sh",
            "-c",
            "mkdir -p /v/db && sqlite3 /v/db/app.db 'pragma journal_mode=wal; create table t(x); insert into t values (1),(2),(3);' && echo note > /v/notes.txt",
        ],
    );
    start_postgres(&engine);
    sh(
        &engine,
        &["exec", "arkt-db", "psql", "-U", "app", "-c", "create table t(x int); insert into t values (1),(2),(3);"],
    );
    sh(&engine, &["run", "-d", "--name", "arkt-cache", "-v", "henry-arkt-rd:/data", "redis:7-alpine"]);
    wait(&engine, &["exec", "arkt-cache", "redis-cli", "PING"]);
    sh(&engine, &["exec", "arkt-cache", "redis-cli", "MSET", "a", "1", "b", "2"]);

    // One job backs up every volume and drills each snapshot.
    let job = canon.enqueue(ark::JOB, Some(APP), &serde_json::json!({})).unwrap();
    let report = ark.run(&job).await.unwrap();
    for target in ["files/data", "lite/lite", "db/pg", "cache/rd"] {
        let b = canon.last_backup(APP, target).unwrap().unwrap();
        assert!(b.ok, "{target}: {:?} ({report})", b.detail);
        let d = canon.last_drill(APP, target).unwrap().unwrap();
        assert!(d.ok, "{target} drill: {:?}", d.detail);
    }
    let detail = |t| canon.last_drill(APP, t).unwrap().unwrap().detail.unwrap();
    assert_eq!(detail("files/data"), "restored 2 files");
    assert!(detail("lite/lite").contains("passes its integrity check"));
    assert!(
        detail("db/pg").starts_with('2') || detail("db/pg").starts_with('3'),
        "app and postgres: {}",
        detail("db/pg")
    );
    assert_eq!(detail("cache/rd"), "2 key(s) restored and answering");
    assert_eq!(
        sh(&engine, &["inspect", "-f", "{{.State.Status}}", "arkt-files"]),
        "running",
        "unpaused after the copy"
    );

    // Nothing more is due on a second run.
    assert!(ark.run(&job).await.unwrap().contains("nothing due"));

    // Restore files over a damaged volume.
    let targets = ark::targets(&canon, APP).unwrap();
    let files = targets.iter().find(|t| t.service == "files").unwrap();
    let snap = canon.last_good_backup(APP, "files/data").unwrap().unwrap().snapshot.unwrap();
    sh(&engine, &["exec", "arkt-files", "sh", "-c", "rm -rf /data/* && echo wrong > /data/a"]);
    let refused = ark.restore(files, &snap, None).await.unwrap_err().to_string();
    assert!(refused.contains("in use"), "a running app's volume is never overwritten: {refused}");
    sh(&engine, &["rm", "-f", "arkt-files"]);
    ark.restore(files, &snap, None).await.unwrap();
    let back = sh(
        &engine,
        &["run", "--rm", "-v", "henry-arkt-data:/data", "alpine:3.20", "sh", "-c", "cat /data/a /data/up/b"],
    );
    assert_eq!(back, "one\ntwo");

    // Restore Postgres over a lost database, with the app's own password.
    let db = targets.iter().find(|t| t.service == "db").unwrap();
    let snap = canon.last_good_backup(APP, "db/pg").unwrap().unwrap().snapshot.unwrap();
    sh(&engine, &["exec", "arkt-db", "psql", "-U", "app", "-c", "drop table t;"]);
    sh(&engine, &["rm", "-f", "arkt-db"]);
    ark.restore(db, &snap, Some(&Secret::new("pw"))).await.unwrap();
    start_postgres(&engine);
    let rows = sh(
        &engine,
        &[
            "exec",
            "-e",
            "PGPASSWORD=pw",
            "arkt-db",
            "psql",
            "-h",
            "127.0.0.1",
            "-U",
            "app",
            "-tAc",
            "select sum(x) from t",
        ],
    );
    assert_eq!(rows, "6", "the rows and the password both came back");

    // Henry's own state.
    let job = canon.enqueue(ark::JOB, Some(HENRY), &serde_json::json!({})).unwrap();
    let report = ark.run(&job).await.unwrap();
    assert!(report.contains("canon drill passed"), "{report}");
    let snap = ark.latest(HENRY, "canon").await.unwrap().unwrap();
    let dest = dir.path().join("restored.db");
    ark.restore_file(&snap.id, "henry/canon.db", &dest).await.unwrap();
    assert_eq!(Canon::open(&dest).unwrap().app(APP).unwrap().name, APP);

    cleanup(&engine);
}
