//! Press with a real `git` and a real image build. Runs only with
//! HENRY_ENGINE_TESTS=1 and `docker` (or `podman`) on PATH.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use canon::{Canon, ReleaseState, Source};
use lease::Lease;
use press::build::{Builder, CliBuilder};
use press::git::Git;
use press::{Outcome, Press, Settings};

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

#[tokio::test]
async fn a_push_is_built_into_an_image() {
    if std::env::var("HENRY_ENGINE_TESTS").ok().as_deref() != Some("1") {
        eprintln!("skipped: set HENRY_ENGINE_TESTS=1 to build with a real engine");
        return;
    }
    let program = if Command::new("podman").arg("--version").output().is_ok_and(|o| o.status.success()) {
        "podman"
    } else {
        "docker"
    };
    let dir = tempfile::tempdir().unwrap();
    let (bare, work) = (dir.path().join("origin.git"), dir.path().join("work"));
    std::fs::create_dir_all(&bare).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    git(&bare, &["init", "-q", "--bare", "--initial-branch=main"]);
    git(&work, &["init", "-q", "--initial-branch=main"]);
    let origin = format!("file://{}", bare.display());
    git(&work, &["remote", "add", "origin", &origin]);
    let app = format!("pressit{}", std::process::id());
    std::fs::write(work.join("Dockerfile"), format!("FROM alpine:3.20\nRUN echo {app} > /built\n")).unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-qm", "one"]);
    git(&work, &["push", "-q", "origin", "main"]);

    let text = format!(
        "name = \"{app}\"\n[[service]]\nname = \"job\"\nport = 80\nbuild = {{ dockerfile = \"Dockerfile\" }}\nhealth = {{ tcp = true }}\n"
    );
    let canon = Arc::new(Canon::open_in_memory().unwrap());
    canon.insert_app(&Lease::parse(&text).unwrap(), &text, &origin, Source::Operator).unwrap();
    let builder = CliBuilder { program: program.into(), timeout: Duration::from_secs(600), railpack: None };
    let settings = Settings {
        checkouts: dir.path().join("checkouts"),
        logs: dir.path().join("logs"),
        limits: lease::Limits::default(),
        build_retries: 1,
        retry_pause: Duration::from_secs(1),
    };
    let press = Press::new(canon.clone(), Git::default(), builder, settings);

    let job = press::request(&canon, &app, None, false).unwrap();
    canon.claim().unwrap();
    let Outcome::Released(id) = press.run(&job).await.unwrap() else { panic!("expected a release") };
    let release = canon.release(id).unwrap();
    assert_eq!(release.state, ReleaseState::Trial);
    let tag = release.images["job"].as_str().unwrap().to_owned();
    let check = CliBuilder { program: program.into(), timeout: Duration::from_secs(60), railpack: None };
    assert!(check.has(&tag).await.unwrap(), "{tag} exists after the build");
    let _ = Command::new(program).args(["image", "rm", "-f", &tag]).output();
}
