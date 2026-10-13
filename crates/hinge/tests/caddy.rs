//! Hinge's Caddy driver against a real Caddy. Runs only when HENRY_CADDY
//! names a `caddy` binary; the test starts and stops its own instance.

use std::net::TcpListener;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use hinge::caddy::Caddy;
use hinge::{Proxy, Route};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// A one-line HTTP upstream that answers every request with `body`.
async fn upstream(body: &'static str) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf).await;
                let reply =
                    format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = s.write_all(reply.as_bytes()).await;
            });
        }
    });
    port
}

/// An upstream that answers with every `X-Henry-Via` it was sent.
async fn echo_via() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let via: Vec<&str> = head
                    .lines()
                    .filter_map(|l| l.split_once(':'))
                    .filter(|(k, _)| k.eq_ignore_ascii_case("x-henry-via"))
                    .map(|(_, v)| v.trim())
                    .collect();
                let body = via.join(",");
                let reply =
                    format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = s.write_all(reply.as_bytes()).await;
            });
        }
    });
    port
}

async fn get(port: u16, host: &str, path: &str) -> (u16, String) {
    get_with(port, host, path, &[]).await
}

async fn get_with(port: u16, host: &str, path: &str, headers: &[(&str, &str)]) -> (u16, String) {
    let mut req = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://127.0.0.1:{port}{path}"))
        .header("host", host);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = req.send().await.unwrap();
    (res.status().as_u16(), res.text().await.unwrap())
}

#[tokio::test]
async fn caddy_routes_what_hinge_applies() {
    let Ok(bin) = std::env::var("HENRY_CADDY") else {
        eprintln!("skipped: set HENRY_CADDY to a caddy binary");
        return;
    };
    let (admin_port, listen_port) = (free_port(), free_port());
    let dir = std::env::temp_dir().join(format!("henry-caddy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let boot = dir.join("boot.json");
    std::fs::write(&boot, format!("{{\"admin\":{{\"listen\":\"127.0.0.1:{admin_port}\"}}}}")).unwrap();
    let _caddy = Running(
        Command::new(bin)
            .args(["run", "--config"])
            .arg(&boot)
            .env("XDG_CONFIG_HOME", &dir)
            .env("XDG_DATA_HOME", &dir)
            .spawn()
            .unwrap(),
    );
    let caddy = Caddy::new(format!("http://127.0.0.1:{admin_port}"), format!("127.0.0.1:{listen_port}"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while caddy.apply(&[]).await.is_err() {
        assert!(Instant::now() < deadline, "caddy never came up");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let (web, api) = (upstream("web").await, upstream("api").await);
    let routes = vec![
        Route { host: "site.example.com".into(), path: "/".into(), upstream: web, owner: "site".into() },
        Route { host: "site.example.com".into(), path: "/api".into(), upstream: api, owner: "site".into() },
    ];
    assert!(caddy.apply(&routes).await.unwrap());
    assert!(!caddy.apply(&routes).await.unwrap(), "Caddy hands back exactly what it was given");

    assert_eq!(get(listen_port, "site.example.com", "/").await, (200, "web".into()));
    assert_eq!(get(listen_port, "site.example.com", "/api/users").await, (200, "api".into()));
    assert_eq!(
        get(listen_port, "site.example.com", "/apiary").await,
        (200, "web".into()),
        "a prefix is a path segment"
    );
    assert_eq!(get(listen_port, "other.example.com", "/").await.0, 404);

    // Swinging the route is one atomic load.
    let next = upstream("web v2").await;
    let swung = vec![Route { upstream: next, ..routes[0].clone() }, routes[1].clone()];
    assert!(caddy.apply(&swung).await.unwrap());
    assert_eq!(get(listen_port, "site.example.com", "/").await, (200, "web v2".into()));

    // Henry's own routes are marked as come from the edge, whatever the
    // client claims; an app's are not.
    let (henry, app) = (echo_via().await, echo_via().await);
    let own = vec![
        Route { host: "henry.example.com".into(), path: "/".into(), upstream: henry, owner: hinge::HENRY.into() },
        Route { host: "app.example.com".into(), path: "/".into(), upstream: app, owner: "app".into() },
    ];
    assert!(caddy.apply(&own).await.unwrap());
    assert_eq!(get(listen_port, "henry.example.com", "/api/apps").await, (200, "edge".into()));
    let forged = get_with(listen_port, "henry.example.com", "/", &[("x-henry-via", "local"), ("X-Henry-Via", "x")]);
    assert_eq!(forged.await, (200, "edge".into()));
    assert_eq!(get(listen_port, "app.example.com", "/").await, (200, "".into()));
    let _ = std::fs::remove_dir_all(&dir);
}
