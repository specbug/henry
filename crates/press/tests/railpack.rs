//! A Railpack build for real: Railpack, the `henry-buildkit` container Press
//! starts, and the image loaded into the engine, then run and asked for a
//! page. Runs only with HENRY_ENGINE_TESTS=1 and HENRY_RAILPACK naming a
//! railpack binary.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::time::{Duration, Instant};

use press::build::{BUILDKIT, BuildRequest, Builder, CliBuilder, Railpack, Recipe};

fn engine(program: &str, args: &[&str]) -> (bool, String) {
    let out = Command::new(program).args(args).output().unwrap();
    (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

fn get(port: u16) -> Option<String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(s, "GET / HTTP/1.0\r\nHost: localhost\r\n\r\n").ok()?;
    let mut text = String::new();
    s.read_to_string(&mut text).ok()?;
    Some(text)
}

#[tokio::test]
async fn a_repo_without_a_dockerfile_is_built_by_railpack_and_runs() {
    let (Some("1"), Ok(railpack)) =
        (std::env::var("HENRY_ENGINE_TESTS").ok().as_deref(), std::env::var("HENRY_RAILPACK"))
    else {
        eprintln!("skipped: set HENRY_ENGINE_TESTS=1 and HENRY_RAILPACK to build with a real Railpack");
        return;
    };
    // As the daemon does: Podman where it is, with a `docker` that is Podman,
    // since Railpack calls `docker` by name.
    let podman = Command::new("podman").arg("--version").output().is_ok_and(|o| o.status.success());
    let program = if podman { "podman" } else { "docker" };
    let dir = tempfile::tempdir().unwrap();
    let shims = if podman {
        let shims = dir.path().join("shims");
        std::fs::create_dir_all(&shims).unwrap();
        let target = String::from_utf8(Command::new("which").arg("podman").output().unwrap().stdout).unwrap();
        std::os::unix::fs::symlink(target.trim(), shims.join("docker")).unwrap();
        Some(shims)
    } else {
        None
    };

    // A BuildKit left from an older Henry: it must be replaced, not used.
    let _ = engine(program, &["rm", "-f", BUILDKIT]);
    let (ok, out) = engine(program, &["run", "-d", "--name", BUILDKIT, "alpine:3.22", "sleep", "600"]);
    assert!(ok, "{out}");

    let app = dir.path().join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("package.json"),
        r#"{"name": "hello", "version": "1.0.0", "scripts": {"start": "node index.js"}}"#,
    )
    .unwrap();
    std::fs::write(
        app.join("index.js"),
        "require('http').createServer((q, s) => s.end('hello from railpack')).listen(process.env.PORT || 3000);\n",
    )
    .unwrap();

    let builder = CliBuilder {
        program: program.into(),
        timeout: Duration::from_secs(1200),
        railpack: Some(Railpack { program: railpack.into(), buildkit_image: "moby/buildkit:v0.34.0".into(), shims }),
    };
    let tag = format!("henry/railpack-test:{}", std::process::id());
    let log = dir.path().join("build.log");
    let req = BuildRequest { recipe: Recipe::Railpack { dir: &app }, tag: &tag, pull: false, log: &log, labels: &[] };
    let built = builder.build(&req).await;
    let log_text = std::fs::read_to_string(&log).unwrap_or_default();
    let tail: String =
        log_text.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
    let label = engine(program, &["inspect", "-f", "{{index .Config.Labels \"dev.henry.image\"}}", BUILDKIT]).1;
    let cleanup = |name: &str| {
        let _ = engine(program, &["rm", "-f", name]);
    };
    if let Err(e) = built {
        cleanup(BUILDKIT);
        panic!("the Railpack build failed: {e}\n{tail}");
    }
    assert_eq!(label.trim(), "moby/buildkit:v0.34.0", "the old henry-buildkit was replaced by the configured one");
    assert!(builder.has(&tag).await.unwrap(), "the image was loaded into {program}");

    let name = format!("henry-railpack-test-{}", std::process::id());
    let (ok, out) = engine(program, &["run", "-d", "--name", &name, "-p", "127.0.0.1::8080", "-e", "PORT=8080", &tag]);
    assert!(ok, "{out}");
    let port: u16 = engine(program, &["port", &name, "8080"])
        .1
        .lines()
        .next()
        .and_then(|l| l.rsplit(':').next())
        .and_then(|p| p.trim().parse().ok())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let answer = loop {
        match get(port) {
            Some(t) if t.contains("hello from railpack") => break Some(t),
            _ if Instant::now() > deadline => break None,
            _ => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    };
    let logs = engine(program, &["logs", &name]).1;
    cleanup(&name);
    let _ = engine(program, &["rmi", "-f", &tag]);
    cleanup(BUILDKIT);
    assert!(answer.is_some(), "the app never answered on $PORT: {logs}");
}
