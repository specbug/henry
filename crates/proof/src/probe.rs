//! Health probes, one per kind a lease can ask for. Each answers one
//! question: is something on this loopback port doing its job right now?

use std::time::Duration;

use lease::Health;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Postgres's SSLRequest: any Postgres server answers it with one byte, `S`
/// or `N`, before authentication. A handshake, not a login.
const PG_SSL_REQUEST: [u8; 8] = [0, 0, 0, 8, 4, 210, 22, 47];

pub async fn probe(health: &Health, port: u16, timeout: Duration) -> Result<(), String> {
    match tokio::time::timeout(timeout, run(health, port, timeout)).await {
        Ok(result) => result,
        Err(_) => Err(format!("no answer within {}s", timeout.as_secs_f32())),
    }
}

async fn run(health: &Health, port: u16, timeout: Duration) -> Result<(), String> {
    match health {
        Health::Http { path } => http(&format!("http://127.0.0.1:{port}{path}"), None, timeout).await,
        Health::Tcp => connect(port).await.map(drop),
        Health::Postgres => {
            let mut s = connect(port).await?;
            s.write_all(&PG_SSL_REQUEST).await.map_err(|e| e.to_string())?;
            let mut reply = [0u8; 1];
            s.read_exact(&mut reply).await.map_err(|e| format!("no Postgres handshake: {e}"))?;
            match reply[0] {
                b'S' | b'N' => Ok(()),
                other => Err(format!("not a Postgres handshake (got byte {other})")),
            }
        }
        Health::Redis => {
            let mut s = connect(port).await?;
            s.write_all(b"PING\r\n").await.map_err(|e| e.to_string())?;
            let mut buf = [0u8; 64];
            let n = s.read(&mut buf).await.map_err(|e| e.to_string())?;
            let reply = String::from_utf8_lossy(&buf[..n]);
            // NOAUTH still proves a live Redis; it just wants a password.
            if reply.starts_with("+PONG") || reply.starts_with("-NOAUTH") {
                Ok(())
            } else {
                Err(format!("Redis answered `{}`", reply.trim()))
            }
        }
    }
}

/// An HTTP GET that passes on any status below 400: the app answered, and
/// did not say it was broken. Redirects are not followed.
pub async fn http(url: &str, host: Option<&str>, timeout: Duration) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = client.get(url);
    if let Some(host) = host {
        req = req.header("host", host);
    }
    let res = req.send().await.map_err(|e| format!("GET {url}: {}", short(&e)))?;
    let status = res.status();
    if status.as_u16() < 400 { Ok(()) } else { Err(format!("GET {url} answered {status}")) }
}

async fn connect(port: u16) -> Result<TcpStream, String> {
    TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| format!("nothing accepts connections on :{port} ({e})"))
}

fn short(e: &reqwest::Error) -> String {
    if e.is_connect() {
        "connection refused".into()
    } else if e.is_timeout() {
        "timed out".into()
    } else {
        e.to_string()
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::TcpListener;

    use super::*;

    /// Answers every connection with `reply` after reading what it was sent.
    async fn server(reply: &'static [u8]) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = s.read(&mut buf).await;
                    let _ = s.write_all(reply).await;
                });
            }
        });
        port
    }

    fn closed_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    const T: Duration = Duration::from_secs(2);

    #[tokio::test]
    async fn http_passes_below_400_and_fails_above() {
        let ok = server(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok").await;
        let moved =
            server(b"HTTP/1.1 302 Found\r\nlocation: https://elsewhere.example\r\ncontent-length: 0\r\n\r\n").await;
        let broken = server(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n").await;
        let h = Health::Http { path: "/health".into() };
        assert_eq!(probe(&h, ok, T).await, Ok(()));
        assert_eq!(probe(&h, moved, T).await, Ok(()), "a redirect is an answer, and is not followed");
        assert!(probe(&h, broken, T).await.unwrap_err().contains("503"));
        assert!(probe(&h, closed_port(), T).await.unwrap_err().contains("connection refused"));
    }

    #[tokio::test]
    async fn postgres_needs_a_real_handshake() {
        assert_eq!(probe(&Health::Postgres, server(b"N").await, T).await, Ok(()));
        assert_eq!(probe(&Health::Postgres, server(b"S").await, T).await, Ok(()));
        assert!(probe(&Health::Postgres, server(b"HTTP/1.1 400").await, T).await.is_err());
    }

    #[tokio::test]
    async fn redis_must_pong_or_ask_for_a_password() {
        assert_eq!(probe(&Health::Redis, server(b"+PONG\r\n").await, T).await, Ok(()));
        assert_eq!(probe(&Health::Redis, server(b"-NOAUTH Authentication required.\r\n").await, T).await, Ok(()));
        assert!(probe(&Health::Redis, server(b"-LOADING Redis is loading\r\n").await, T).await.is_err());
    }

    #[tokio::test]
    async fn tcp_needs_a_listener_and_silence_times_out() {
        assert_eq!(probe(&Health::Tcp, server(b"").await, T).await, Ok(()));
        assert!(probe(&Health::Tcp, closed_port(), T).await.is_err());

        // Accepts, never answers: a Postgres probe must not hang.
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _keep = l.accept().await;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let err = probe(&Health::Postgres, port, Duration::from_millis(300)).await.unwrap_err();
        assert!(err.contains("no answer"), "{err}");
    }
}
