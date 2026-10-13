//! Disk upkeep against a real engine: Henry's stale images go, the ones a
//! release could run stay, and nobody else's images are touched.
//!
//! Runs when `HENRY_ENGINE_TESTS=1`.

use std::time::Duration;

use canon::{Canon, ReleaseState, Source};
use mend::disk::Disk;

fn engine() -> Option<&'static str> {
    if std::env::var("HENRY_ENGINE_TESTS").ok().as_deref() != Some("1") {
        return None;
    }
    let docker = std::process::Command::new("docker").arg("info").output().is_ok_and(|o| o.status.success());
    Some(if docker { "docker" } else { "podman" })
}

fn sh(engine: &str, args: &[&str]) -> String {
    let out = std::process::Command::new(engine).args(args).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[tokio::test]
async fn stale_images_logs_and_checkouts_go_and_the_rest_stay() {
    let Some(engine) = engine() else { return };
    let base = "alpine:3.20";
    if sh(engine, &["image", "inspect", "-f", "{{.Id}}", base]).is_empty() {
        sh(engine, &["pull", "-q", base]);
    }
    let tags: Vec<String> = (1..=4).map(|n| format!("henry/mendt-web:{n}")).collect();
    for t in &tags {
        sh(engine, &["tag", base, t]);
    }

    let canon = Canon::open_in_memory().unwrap();
    let lease = "name = \"mendt\"\n[[service]]\nname = \"web\"\nport = 80\nbuild = { dockerfile = \"Dockerfile\" }\nhealth = { http = \"/\" }\n";
    canon.insert_app(&lease::Lease::parse(lease).unwrap(), lease, "repo", Source::Operator).unwrap();
    let mut ids = Vec::new();
    for t in &tags {
        let r = canon.create_release("mendt", t, lease).unwrap();
        canon.set_release_images(r.id, &serde_json::json!({ "web": t })).unwrap();
        ids.push(r.id);
    }
    canon.set_release_state(ids[0], ReleaseState::Live, None).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let (logs, checkouts) = (dir.path().join("logs"), dir.path().join("checkouts"));
    std::fs::create_dir_all(checkouts.join("mendt")).unwrap();
    std::fs::create_dir_all(checkouts.join("long-gone")).unwrap();
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(logs.join("old.log"), "x").unwrap();

    let disk =
        Disk { engine: engine.into(), logs, checkouts: checkouts.clone(), keep_releases: 1, log_age: Duration::ZERO };
    let pruned = disk.prune(&canon).await.unwrap();

    assert!(pruned.images.contains(&tags[1]) && pruned.images.contains(&tags[2]), "{pruned:?}");
    for kept in [&tags[0], &tags[3]] {
        assert!(!sh(engine, &["image", "inspect", "-f", "{{.Id}}", kept]).is_empty(), "{kept} is live or newest");
    }
    assert!(!sh(engine, &["image", "inspect", "-f", "{{.Id}}", base]).is_empty(), "not Henry's, not touched");
    assert_eq!(pruned.logs, 1);
    assert_eq!(pruned.checkouts, ["long-gone"]);
    assert!(checkouts.join("mendt").exists());

    for t in &tags {
        let _ = std::process::Command::new(engine).args(["rmi", t]).output();
    }
}
