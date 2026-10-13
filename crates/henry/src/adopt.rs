//! Moving an app onto Henry from hostd, one at a time, with hostd serving it
//! until Henry has proven it.
//!
//! `henry adopt` imports the app's `.env` into Hush, copies hostd's volume
//! into the one Henry's lease will mount, and starts an ordinary onboarding.
//! Henry builds, runs and proves the app privately: its DNS record was made
//! by hand for hostd's tunnel, so Hinge leaves it alone and the public keeps
//! reaching hostd.
//!
//! `henry cutover` is the switch, once the app is live under Henry and
//! hostd has let it go: a final copy of the data, then the DNS record moved
//! to Henry's tunnel. The old record is kept, and `henry cutover --back`
//! puts it back.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use canon::{Canon, Desired, Status};
use hush::{Hush, Secret};

use crate::home::Config;

/// What the old stack used to reach the edge itself. Henry's tunnel takes
/// its place, so these are not imported.
const EDGE: &[&str] = &["CLOUDFLARE_TUNNEL_TOKEN", "TUNNEL_TOKEN"];

/// `KEY=VALUE` lines as compose reads a `.env`: `#` comments, an optional
/// `export `, quotes stripped, and ` #` comments after an unquoted value.
pub fn parse_env(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim();
        let value = match value.chars().next() {
            Some(q @ ('"' | '\'')) => value[1..].split(q).next().unwrap_or_default().to_owned(),
            _ => value.split(" #").next().unwrap_or_default().trim_end().to_owned(),
        };
        out.push((key.trim().to_owned(), value));
    }
    out
}

/// A volume name as a lease writes it, the way Sound turns compose's.
pub fn lease_volume(name: &str) -> String {
    let n: String =
        name.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    n.trim_matches('-').to_owned()
}

pub struct Adopt<'a> {
    pub repo: &'a str,
    pub name: &'a str,
    pub hostname: Option<&'a str>,
    pub branch: Option<&'a str>,
    /// The app's `.env`, as hostd ran it.
    pub env: Option<&'a Path>,
    /// hostd's volume, e.g. `odyssey-data`.
    pub volume: Option<&'a str>,
}

/// Import, copy and start onboarding. Safe to run again until the
/// onboarding has started.
pub async fn adopt(config: &Config, canon: &Canon, hush: &Hush, a: &Adopt<'_>) -> Result<Vec<String>> {
    let name = a.name;
    let mut said = Vec::new();
    if canon.app(name).is_ok() || canon.draft(name)?.is_some_and(|d| d.stage != canon::DraftStage::Stuck) {
        bail!("`{name}` is already on Henry");
    }

    if let Some(path) = a.env {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let (mut imported, mut skipped) = (Vec::new(), Vec::new());
        for (key, value) in parse_env(&text) {
            if EDGE.contains(&key.as_str()) {
                skipped.push(key);
                continue;
            }
            hush.set(name, &key, &Secret::new(value)).with_context(|| format!("importing {key}"))?;
            imported.push(key);
        }
        said.push(format!("sealed {} variable(s) from {}: {}", imported.len(), path.display(), imported.join(", ")));
        if !skipped.is_empty() {
            said.push(format!("left out {}: Henry's own tunnel takes its place", skipped.join(", ")));
        }
    }

    match a.volume {
        Some(from) => {
            let volume = lease_volume(from);
            let to = plumb::plan::volume_name(name, &volume);
            canon.put_adoption(name, Some(from), Some(&volume))?;
            let files = ark::seed(&crate::backup::engine(config), from, &to, name).await?;
            canon.set_seeded(name, files)?;
            said.push(format!("copied {files} file(s) from hostd's {from} into {to}, with hostd still serving"));
        }
        None => canon.put_adoption(name, None, None)?,
    }

    draft::start(canon, name, a.repo, a.branch, a.hostname)?;
    said.push(format!(
        "onboarding `{name}`. Henry proves it privately; DNS still sends the public to hostd. \
         Once `henry show {name}` says live, stop it under hostd and run `henry cutover {name}`"
    ));
    Ok(said)
}

/// Poll `check` every two seconds until it holds or `patience` runs out.
async fn until(patience: Duration, mut check: impl AsyncFnMut() -> Result<bool>) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        if check().await? {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Move the public to Henry: hostd must have let go, the app must be live
/// under Henry; then a final copy of the data and the DNS record. Safe to run
/// again: once the switch has begun, the data is never copied over again.
pub async fn cutover(config: &Config, canon: Arc<Canon>, hush: &Hush, app: &str) -> Result<Vec<String>> {
    let Some(adoption) = canon.adoption(app)? else {
        bail!("`{app}` was not adopted from hostd; there is nothing to cut over");
    };
    let a = canon.app(app)?;
    if a.live_release.is_none() || a.status != Status::Live {
        bail!("`{app}` is not live under Henry yet ({}); `henry show {app}` says why", a.status);
    }
    let host = canon.release(a.live_release.expect("checked above"))?;
    let host = lease::Lease::parse(&host.lease)?.hostnames().next().map(str::to_owned);
    let cf = match &host {
        Some(_) => Some(crate::edge::cloudflare(config, hush)?.context("set [cloudflare] and its API token first")?),
        None => None,
    };
    let mut said = Vec::new();

    if adoption.cutover_at.is_none()
        && let (Some(from), Some(volume)) = (&adoption.source_volume, &adoption.volume)
    {
        let engine = crate::backup::engine(config);
        let users = engine.users(from).await?;
        if !users.is_empty() {
            bail!(
                "hostd still runs `{app}` on {from}. Let it go first:\n  \
                 hostd rm {app} && hostd install\n  \
                 podman compose -f ~/Documents/<repo>/compose.yml down   # in the app's working copy"
            );
        }
        let to = plumb::plan::volume_name(app, volume);
        canon.set_desired(app, Desired::Stopped)?;
        let stopped = until(Duration::from_secs(180), async || Ok(engine.users(&to).await?.is_empty())).await;
        let copied = match stopped {
            Ok(true) => ark::seed(&engine, from, &to, app).await.map_err(anyhow::Error::from),
            Ok(false) => Err(anyhow::anyhow!("Henry did not stop `{app}` within 3 minutes; is the daemon running?")),
            Err(e) => Err(e),
        };
        // Whatever happened, the app is wanted running again.
        canon.set_desired(app, Desired::Running)?;
        let files = copied?;
        canon.set_seeded(app, files)?;
        said.push(format!("copied hostd's final {from} ({files} file(s)) into {to}"));
        let back = until(Duration::from_secs(300), async || Ok(canon.app(app)?.status == Status::Live)).await?;
        if !back {
            bail!("`{app}` did not come back live after the copy; DNS was not touched. `henry show {app}`");
        }
    }

    match (&host, &cf) {
        (Some(host), Some(cf)) => {
            let now = cf.record(host).await?;
            if now.as_ref().is_some_and(|r| cf.is_ours(r)) {
                if adoption.cutover_at.is_none() {
                    canon.set_cutover(app, None)?;
                }
                said.push(format!("{host} already points at Henry"));
            } else {
                // Written before the switch, so a crash cannot lose the old record.
                let old = match adoption.cutover_at {
                    Some(_) => adoption.dns.clone().filter(|d| !d.is_null()),
                    None => now,
                };
                canon.set_cutover(app, old.as_ref())?;
                cf.take_over(host).await?;
                said.push(format!("{host} now points at Henry's tunnel; `henry cutover {app} --back` undoes it"));
            }
        }
        _ => {
            if adoption.cutover_at.is_none() {
                canon.set_cutover(app, None)?;
            }
            said.push(format!("`{app}` has no public hostname; nothing to point"));
        }
    }
    Ok(said)
}

/// Hand the public back to hostd: the old DNS record returns. Henry keeps
/// running the app privately.
pub async fn back(config: &Config, canon: &Canon, hush: &Hush, app: &str) -> Result<Vec<String>> {
    let Some(adoption) = canon.adoption(app)?.filter(|a| a.cutover_at.is_some()) else {
        bail!("`{app}` has not been cut over");
    };
    let a = canon.app(app)?;
    let lease = a.lease()?;
    let mut said = Vec::new();
    if let Some(host) = lease.hostnames().next() {
        let cf = crate::edge::cloudflare(config, hush)?.context("set [cloudflare] and its API token first")?;
        cf.give_back(host, adoption.dns.as_ref().filter(|d| !d.is_null())).await?;
        said.push(format!("{host} points where it did before the cutover"));
    }
    canon.clear_cutover(app)?;
    if let Some(volume) = &adoption.volume {
        said.push(format!(
            "start `{app}` under hostd again. What it wrote since the cutover is in {}; \
             a later cutover copies hostd's data over it",
            plumb::plan::volume_name(app, volume)
        ));
    }
    Ok(said)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dotenv_reads_as_compose_reads_it() {
        let text = "# Odyssey\nMETA_API_KEY=sk-abc=def\nexport CORS_ORIGINS=\"https://a.example.com\"\n\nQUOTED='it''s'\nEMPTY=\nNOTE=value # a comment\n  SPACED = x \nnot a line\n";
        let got = parse_env(text);
        let want = [
            ("META_API_KEY", "sk-abc=def"),
            ("CORS_ORIGINS", "https://a.example.com"),
            ("QUOTED", "it"),
            ("EMPTY", ""),
            ("NOTE", "value"),
            ("SPACED", "x"),
        ];
        assert_eq!(got, want.map(|(k, v)| (k.to_owned(), v.to_owned())));
    }

    #[test]
    fn a_hostd_volume_keeps_its_name_in_the_lease() {
        assert_eq!(lease_volume("odyssey-data"), "odyssey-data");
        assert_eq!(lease_volume("Draw_Data"), "draw-data");
        assert_eq!(plumb::plan::volume_name("draw", &lease_volume("draw-data")), "henry-draw-draw-data");
    }

    #[tokio::test]
    async fn adopting_imports_the_env_and_starts_onboarding() {
        let dir = tempfile::tempdir().unwrap();
        let env = dir.path().join(".env");
        std::fs::write(&env, "META_API_KEY=sk-1\nCLOUDFLARE_TUNNEL_TOKEN=eyJ\n").unwrap();
        let canon = Arc::new(Canon::open_in_memory().unwrap());
        let hush = Hush::new(canon.clone(), &hush::Key::generate().unwrap());
        let a = Adopt {
            repo: "https://github.com/specbug/odyssey",
            name: "odyssey",
            hostname: Some("odyssey.example.com"),
            branch: Some("main"),
            env: Some(&env),
            volume: None,
        };
        let said = adopt(&Config::default(), &canon, &hush, &a).await.unwrap();
        assert!(said[0].contains("META_API_KEY") && !said.join(" ").contains("sk-1"), "{said:?}");
        assert_eq!(hush.get("odyssey", "META_API_KEY").unwrap().unwrap().expose(), "sk-1");
        assert!(hush.get("odyssey", "CLOUDFLARE_TUNNEL_TOKEN").unwrap().is_none());
        assert_eq!(canon.draft("odyssey").unwrap().unwrap().hostname.as_deref(), Some("odyssey.example.com"));
        assert!(canon.adoption("odyssey").unwrap().is_some());
        assert!(adopt(&Config::default(), &canon, &hush, &a).await.is_err(), "once onboarding, not again");
        let err = cutover(&Config::default(), canon.clone(), &hush, "odyssey").await.unwrap_err().to_string();
        assert!(err.contains("no app") || err.contains("not live"), "{err}");
    }
}
