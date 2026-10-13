//! Hold against a real engine. Runs only with HENRY_ENGINE_TESTS=1 and a
//! Docker or Podman socket, so `cargo test` stays hermetic by default.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use hold::{ContainerSpec, Engine, EnvVar, Labels, Machine, Mount, Publish, Runtime, Socket, State, label};

fn enabled() -> bool {
    std::env::var("HENRY_ENGINE_TESTS").is_ok_and(|v| v == "1")
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn get(port: u16) -> Option<String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    s.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n").ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    Some(out)
}

#[tokio::test]
async fn a_container_round_trip_on_a_real_engine() {
    if !enabled() {
        eprintln!("skipped: set HENRY_ENGINE_TESTS=1 to run against a real engine");
        return;
    }
    let socket = Socket::discover().expect("no Docker or Podman socket found");
    assert_eq!(socket.ensure_up().await.unwrap(), hold::MachineState::Up);
    let engine = Engine::connect(&socket.path).unwrap();
    engine.ping().await.unwrap();

    let tag = format!("henry-it-{}", std::process::id());
    let labels = Labels::from([(label::APP.to_owned(), tag.clone())]);
    engine.ensure_network(&tag, &labels).await.unwrap();
    engine.ensure_network(&tag, &labels).await.unwrap(); // idempotent
    engine.ensure_volume(&tag, &labels).await.unwrap();
    engine.ensure_volume(&tag, &labels).await.unwrap();
    engine.ensure_image("nginx:alpine").await.unwrap();
    assert!(engine.ensure_image("henry.invalid/no-such-image:0").await.is_err());

    let port = free_port();
    let spec = ContainerSpec {
        name: tag.clone(),
        image: "nginx:alpine".into(),
        command: None,
        env: vec![
            EnvVar { name: "PLAIN".into(), value: "1".into(), secret: false },
            EnvVar { name: "TOKEN".into(), value: "s3cret".into(), secret: true },
        ],
        secret_stamp: "1".into(),
        labels: labels.clone(),
        network: tag.clone(),
        aliases: vec!["web".into()],
        mounts: vec![Mount { volume: tag.clone(), target: "/data".into() }],
        publish: Some(Publish { container_port: 80, host_ip: "127.0.0.1".into(), host_port: port }),
        memory_bytes: 64 << 20,
        cpus: 0.5,
    };
    let id = engine.create(&spec).await.unwrap();
    assert!(matches!(engine.create(&spec).await, Err(hold::Error::Conflict(_))));
    engine.start(&id).await.unwrap();
    engine.start(&id).await.unwrap(); // already running is fine

    let listed = engine.list().await.unwrap();
    let mine = listed.iter().find(|c| c.name == tag).expect("the container is listed");
    assert_eq!(mine.state, State::Running);
    assert_eq!(mine.spec_hash(), Some(spec.hash().as_str()));
    assert_eq!(mine.label(label::APP), Some(tag.as_str()));
    assert!(mine.networks[&tag].contains(&"web".to_owned()), "aliases are reported: {:?}", mine.networks);

    // Aliases move in place: same container, still running, new name.
    engine.set_aliases(&id, &tag, &["api".into()]).await.unwrap();
    let moved = engine.list().await.unwrap().into_iter().find(|c| c.name == tag).unwrap();
    assert_eq!(moved.id, id);
    assert_eq!(moved.state, State::Running);
    assert!(moved.networks[&tag].contains(&"api".to_owned()), "{:?}", moved.networks);
    assert!(!moved.networks[&tag].contains(&"web".to_owned()));

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut answered = None;
    while Instant::now() < deadline && answered.is_none() {
        answered = get(port).filter(|r| r.starts_with("HTTP/1.1 200"));
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(answered.is_some(), "nginx answered on the published port");

    let seen = engine.inspect(&id).await.unwrap();
    assert_eq!(seen.state, State::Running);
    assert_eq!(seen.restart_count, 0);
    assert!(engine.logs(&id, 50, None).await.is_ok());

    engine.stop(&id, Duration::from_secs(5)).await.unwrap();
    assert_eq!(engine.inspect(&id).await.unwrap().state, State::Exited);
    engine.remove(&id).await.unwrap();
    engine.remove(&id).await.unwrap(); // already gone is fine
    assert!(!engine.list().await.unwrap().iter().any(|c| c.name == tag));

    let _ = std::process::Command::new("docker").args(["network", "rm", &tag]).output();
    let _ = std::process::Command::new("docker").args(["volume", "rm", &tag]).output();
}
