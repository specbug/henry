use canon::{ReleaseState, Source};

use super::*;

const LEASE: &str = r#"
name = "draw"

[[service]]
name = "web"
port = 80
build = { image = "web:1" }
health = { http = "/" }

[[service.volume]]
name = "uploads"
mount = "/uploads"

[[service]]
name = "db"
port = 5432
build = { image = "postgres:17-alpine" }
health = { postgres = true }
env = { POSTGRES_USER = "draw" }

[[service.volume]]
name = "pg"
mount = "/var/lib/postgresql/data"
backup = "postgres"
"#;

fn canon(live: bool) -> Canon {
    let canon = Canon::open_in_memory().unwrap();
    canon.insert_app(&Lease::parse(LEASE).unwrap(), LEASE, "repo", Source::Operator).unwrap();
    if live {
        let r = canon.create_release("draw", "sha", LEASE).unwrap();
        canon
            .set_release_images(r.id, &serde_json::json!({ "web": "henry/draw-web:abc", "db": "postgres:17-alpine" }))
            .unwrap();
        canon.set_release_state(r.id, ReleaseState::Live, None).unwrap();
    }
    canon
}

#[test]
fn every_volume_of_the_live_release_is_a_target() {
    assert!(targets(&canon(false), "draw").unwrap().is_empty(), "nothing live, nothing to keep");
    let t = targets(&canon(true), "draw").unwrap();
    assert_eq!(t.len(), 2);
    let (web, db) = (&t[0], &t[1]);
    assert_eq!((web.name(), web.kind.clone()), ("web/uploads".to_owned(), Backup::Files), "no backup = files");
    assert_eq!(web.engine_volume, "henry-draw-uploads");
    assert_eq!(web.filename(), "draw/web/uploads.tar");
    assert_eq!(db.container, "draw-db");
    assert_eq!(db.image, "postgres:17-alpine");
    assert_eq!(db.pg_user(), "draw");
    assert_eq!(db.filename(), "draw/db/pg.sql");
    assert_eq!(db.tags(), ["app=draw", "target=db/pg"]);
}

#[test]
fn schedule_queues_what_is_due_and_backs_off_after_a_failure() {
    let c = canon(true);
    let s = Settings::default();
    assert_eq!(schedule(&c, &s).unwrap(), ["draw", HENRY]);
    let claimed: Vec<_> = std::iter::from_fn(|| c.claim().unwrap()).collect();
    for j in &claimed {
        c.finish(j.id).unwrap();
    }

    for target in ["web/uploads", "db/pg"] {
        c.record_backup("draw", target, Some("snap"), Some(1), None).unwrap();
    }
    c.record_backup(HENRY, CANON, None, None, Some("network down")).unwrap();
    assert!(schedule(&c, &s).unwrap().is_empty(), "fresh backups, and a failure waits for its retry");

    let soon = Settings { retry: Duration::ZERO, ..Settings::default() };
    assert_eq!(schedule(&c, &soon).unwrap(), [HENRY], "once the retry interval passes, it is tried again");
}

#[test]
fn a_stopped_app_is_not_scheduled() {
    let c = canon(true);
    c.set_desired("draw", Desired::Stopped).unwrap();
    assert_eq!(schedule(&c, &Settings::default()).unwrap(), [HENRY]);
}

#[test]
fn only_plain_relative_paths_reach_a_script() {
    for good in ["app.db", "db/app.db", "data/my-app_1.sqlite"] {
        assert!(safe_path(good).is_ok(), "{good}");
    }
    for bad in ["/etc/passwd", "../x.db", "a//b", "x'; rm -rf /; '", "a b.db", "$(id).db"] {
        assert!(safe_path(bad).is_err(), "{bad}");
    }
}

#[test]
fn a_backup_reports_its_snapshot_or_why_not() {
    use std::os::unix::process::ExitStatusExt;
    let out = |code: i32, stdout: &str| std::process::Output {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout: stdout.as_bytes().to_vec(),
        stderr: vec![],
    };
    let ok = out(
        0,
        "{\"message_type\":\"status\"}\n{\"message_type\":\"summary\",\"total_bytes_processed\":6,\"snapshot_id\":\"b4a7\"}\n",
    );
    assert_eq!(restic::parse_backup(&ok).unwrap(), Snapshot { id: "b4a7".into(), bytes: 6 });
    let failed = out(
        1,
        "{\"message_type\":\"error\",\"error\":{\"message\":\"failed to save /f: command failed\"}}\n{\"message_type\":\"exit_error\",\"code\":1,\"message\":\"Fatal: unable to save snapshot: command failed: exit status 1\"}\n",
    );
    let e = restic::parse_backup(&failed).unwrap_err().to_string();
    assert!(e.contains("unable to save snapshot"), "{e}");
}
