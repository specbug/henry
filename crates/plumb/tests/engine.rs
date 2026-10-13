//! Plumb against a real engine, end to end. Runs only with
//! HENRY_ENGINE_TESTS=1 and a Docker or Podman socket.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use canon::{Canon, Desired, ReleaseState, Source, Status};
use hold::{Engine, Runtime, Socket};
use hush::{Hush, Key, Secret};
use lease::Lease;
use plumb::{Plumb, Settings};

fn answers(port: u16) -> bool {
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let mut out = String::new();
    s.write_all(b"GET / HTTP/1.0\r\n\r\n").is_ok()
        && s.read_to_string(&mut out).is_ok()
        && out.starts_with("HTTP/1.1 200")
}

#[tokio::test]
async fn plumb_runs_an_app_on_a_real_engine() {
    if std::env::var("HENRY_ENGINE_TESTS").ok().as_deref() != Some("1") {
        eprintln!("skipped: set HENRY_ENGINE_TESTS=1 to run against a real engine");
        return;
    }
    let app = format!("it{}", std::process::id());
    let text = format!(
        "name = \"{app}\"\n[[service]]\nname = \"web\"\nport = 80\nbuild = {{ image = \"nginx:alpine\" }}\nhealth = {{ http = \"/\" }}\npublic = {{ hostname = \"{app}.example.com\" }}\nsecrets = [\"TOKEN\"]\n"
    );
    let canon = Arc::new(Canon::open_in_memory().unwrap());
    let hush = Arc::new(Hush::new(canon.clone(), &Key::generate().unwrap()));
    canon.insert_app(&Lease::parse(&text).unwrap(), &text, "repo", Source::Operator).unwrap();
    hush.set(&app, "TOKEN", &Secret::new("t")).unwrap();
    let r = canon.create_release(&app, "images", &text).unwrap();
    canon.set_release_state(r.id, ReleaseState::Trial, None).unwrap();

    let socket = Socket::discover().expect("no engine socket");
    let engine = Engine::connect(&socket.path).unwrap();
    // A port range of its own, so a concurrent run cannot collide with it.
    let base = 31000 + (std::process::id() % 1000) as u16;
    let settings = Settings { ports: base..base + 10, ..Settings::default() };
    let plumb = Plumb::new(canon.clone(), hush, engine.clone(), Socket { path: socket.path.clone() }, settings);

    let report = plumb.pass().await.unwrap();
    assert_eq!(report.created, [format!("{app}-web-r{}", r.id)], "{report}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !answers(base) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(answers(base), "the app answers on its published port");

    let again = plumb.pass().await.unwrap();
    assert!(again.is_quiet(), "a converged pass changes nothing: {again}");

    canon.set_desired(&app, Desired::Stopped).unwrap();
    let report = plumb.pass().await.unwrap();
    assert_eq!(report.removed.len(), 1);
    assert_eq!(canon.app(&app).unwrap().status, Status::Stopped);
    assert!(!engine.list().await.unwrap().iter().any(|c| c.name.starts_with(&app)));
    let _ = std::process::Command::new("docker").args(["network", "rm", &format!("henry-{app}")]).output();
}
