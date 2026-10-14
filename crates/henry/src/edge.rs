//! Setting up Hinge from config and Hush: Caddy's boot file, the tunnel
//! token file cloudflared reads, and Henry's own public routes.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use canon::Canon;
use hinge::caddy::Caddy;
use hinge::cloudflare::Cloudflare;
use hinge::workers::Workers;
use hinge::{Hinge, Route, Settings};
use hush::{HOST, Hush};
use tracing::warn;

use crate::home::{Config, Home};

pub const API_TOKEN: &str = "CLOUDFLARE_API_TOKEN";
pub const TUNNEL_TOKEN: &str = "CLOUDFLARE_TUNNEL_TOKEN";

/// Write the files the edge agents read. Safe to run on every start.
pub fn prepare(home: &Home, config: &Config, hush: &Hush) -> Result<()> {
    home.ensure()?;
    let admin = config.hinge.admin.trim_start_matches("http://").trim_start_matches("https://");
    let boot = format!("{{\"admin\":{{\"listen\":\"{admin}\"}}}}\n");
    if std::fs::read_to_string(home.caddy_boot()).ok().as_deref() != Some(boot.as_str()) {
        std::fs::write(home.caddy_boot(), boot).context("writing Caddy's boot config")?;
    }
    if let Some(token) = hush.get(HOST, TUNNEL_TOKEN)? {
        write_private(&home.tunnel_token(), token.expose().as_bytes())?;
    }
    Ok(())
}

/// Replace a file atomically, readable only by you from its first byte.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Henry's own public routes: the webhook path, when a hostname is set, and
/// Glass, when it is behind Cloudflare Access. Caddy marks every request on
/// them as come from the edge.
pub fn own_routes(config: &Config) -> Result<Vec<Route>> {
    let port = || -> Result<u16> {
        config.http.listen.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).context("[http] listen needs a port")
    };
    let mut routes = Vec::new();
    if let Some(host) = &config.hinge.hooks_host {
        routes.push(Route { host: host.clone(), path: "/hooks".into(), upstream: port()?, owner: hinge::HENRY.into() });
    }
    if let Some((host, _, _)) = config.glass.access()? {
        routes.push(Route { host: host.to_owned(), path: "/".into(), upstream: port()?, owner: hinge::HENRY.into() });
    }
    Ok(routes)
}

/// The Cloudflare API client, when `[cloudflare]` and its token are set.
pub fn cloudflare(config: &Config, hush: &Hush) -> Result<Option<Cloudflare>> {
    Ok(match &config.cloudflare {
        None => None,
        Some(cf) => match hush.get(HOST, API_TOKEN)? {
            Some(token) => Some(Cloudflare::new(&cf.api, &cf.account, &cf.tunnel, cf.zones.clone(), token.expose())),
            None => {
                warn!("[cloudflare] is configured but henry/{API_TOKEN} is not set; routing locally only");
                None
            }
        },
    })
}

/// Henry's R2 bucket: the one `[ark]` backs up to, when that is on R2.
pub fn r2_bucket(config: &Config) -> Option<String> {
    let repo = config.ark.as_ref()?.repository.strip_prefix("s3:")?;
    let (host, path) = repo.strip_prefix("https://")?.split_once('/')?;
    if !host.ends_with(".r2.cloudflarestorage.com") {
        return None;
    }
    path.split('/').next().filter(|b| !b.is_empty()).map(str::to_owned)
}

/// The Workers deployer, when `[cloudflare]` and its token are set.
pub fn workers(config: &Config, canon: Arc<Canon>, hush: Arc<Hush>) -> Result<Option<Workers<Cloudflare>>> {
    let edge = cloudflare(config, &hush)?;
    Ok(edge.map(|cf| Workers::new(canon, hush, cf, r2_bucket(config), Duration::from_secs(300))))
}

pub fn hinge(config: &Config, canon: Arc<Canon>, hush: &Hush) -> Result<Hinge<Caddy, Cloudflare>> {
    let caddy = Caddy::new(config.hinge.admin.clone(), config.hinge.listen.clone());
    let tunnel = cloudflare(config, hush)?;
    let settings = Settings {
        origin: format!("http://{}", config.hinge.listen),
        own: own_routes(config)?,
        gate: Duration::from_secs(2),
    };
    Ok(Hinge::new(canon, caddy, tunnel, settings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workers_store_in_the_r2_bucket_backups_go_to() {
        let mut config = Config::default();
        assert_eq!(r2_bucket(&config), None);
        let ark = |repo: &str| {
            Some(crate::home::ArkConfig { repository: repo.into(), ..toml::from_str("repository = \"x\"").unwrap() })
        };
        config.ark = ark("s3:https://acct.r2.cloudflarestorage.com/henry/henry");
        assert_eq!(r2_bucket(&config).as_deref(), Some("henry"));
        config.ark = ark("s3:https://s3.amazonaws.com/bucket/henry");
        assert_eq!(r2_bucket(&config), None, "not R2: Workers cannot bind it");
        config.ark = ark("rclone:gdrive:henry");
        assert_eq!(r2_bucket(&config), None);
    }

    #[test]
    fn the_webhook_route_follows_the_http_port() {
        let mut config = Config::default();
        assert!(own_routes(&config).unwrap().is_empty());
        config.hinge.hooks_host = Some("hooks.example.com".into());
        config.http.listen = "127.0.0.1:7788".into();
        let r = own_routes(&config).unwrap();
        assert_eq!((r[0].host.as_str(), r[0].path.as_str(), r[0].upstream), ("hooks.example.com", "/hooks", 7788));
    }

    #[test]
    fn glass_is_routed_only_behind_access() {
        let mut config = Config::default();
        config.glass.hostname = Some("henry.example.com".into());
        let err = own_routes(&config).unwrap_err().to_string();
        assert!(err.contains("Cloudflare Access"), "{err}");
        config.glass.access_team = Some("acme".into());
        config.glass.access_aud = Some("aud-tag".into());
        let r = own_routes(&config).unwrap();
        assert_eq!((r[0].host.as_str(), r[0].path.as_str(), r[0].owner.as_str()), ("henry.example.com", "/", "henry"));
    }

    #[test]
    fn prepare_writes_caddys_boot_and_a_private_token() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home { root: dir.path().join("h") };
        let canon = Arc::new(Canon::open_in_memory().unwrap());
        let hush = Hush::new(canon, &hush::Key::generate().unwrap());
        hush.set(HOST, TUNNEL_TOKEN, &hush::Secret::new("eyJ-token")).unwrap();
        prepare(&home, &Config::default(), &hush).unwrap();
        prepare(&home, &Config::default(), &hush).unwrap();

        assert_eq!(
            std::fs::read_to_string(home.caddy_boot()).unwrap(),
            "{\"admin\":{\"listen\":\"127.0.0.1:2019\"}}\n"
        );
        assert_eq!(std::fs::read_to_string(home.tunnel_token()).unwrap(), "eyJ-token");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(home.tunnel_token()).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
