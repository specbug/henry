//! Ark, built from `[ark]` and Henry's own secrets.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use ark::engine::Engine;
use ark::{Ark, Restic, Retention, Settings};
use canon::Canon;
use hush::{HOST, Hush, Secret};

use crate::home::{ArkConfig, Config, Home};

pub const RESTIC_PASSWORD: &str = "RESTIC_PASSWORD";
pub const R2_KEY_ID: &str = "R2_ACCESS_KEY_ID";
pub const R2_SECRET: &str = "R2_SECRET_ACCESS_KEY";
/// Bump when the helper's Dockerfile changes, so it is built again.
const HELPER: &str = "henry/ark-helper:1";

pub fn settings(home: &Home, ac: &ArkConfig) -> Settings {
    Settings {
        every: Duration::from_secs(ac.every_hours.max(1) * 3600),
        drill_every: Duration::from_secs(ac.drill_every_days.max(1) * 24 * 3600),
        keep: Retention { daily: ac.keep_daily, weekly: ac.keep_weekly, monthly: ac.keep_monthly },
        scratch: home.root.join("scratch"),
        config: Some(home.config()),
        ..Settings::default()
    }
}

/// Whether backups can run: `[ark]` is set and so is the repository password.
pub fn ready(config: &Config, hush: &Hush) -> Result<bool> {
    Ok(config.ark.is_some() && hush.get(HOST, RESTIC_PASSWORD)?.is_some())
}

pub fn engine(config: &Config) -> Engine {
    Engine {
        program: crate::daemon::builder_program(config),
        helper: HELPER.into(),
        timeout: Duration::from_secs(3600),
    }
}

fn restic(home: &Home, ac: &ArkConfig, hush: &Hush, repository: &str) -> Result<Restic> {
    let program = crate::binaries::restic(home, Some(ac));
    let password = hush
        .get(HOST, RESTIC_PASSWORD)?
        .with_context(|| format!("set henry/{RESTIC_PASSWORD} first, and keep a copy somewhere safe"))?;
    let mut env = Vec::new();
    if repository.starts_with("s3:") {
        for (name, var) in [(R2_KEY_ID, "AWS_ACCESS_KEY_ID"), (R2_SECRET, "AWS_SECRET_ACCESS_KEY")] {
            let value = hush.get(HOST, name)?.with_context(|| format!("set henry/{name} for {repository}"))?;
            env.push((var.to_owned(), value));
        }
        if repository.contains(".r2.cloudflarestorage.com") {
            env.push(("AWS_DEFAULT_REGION".to_owned(), Secret::new("auto")));
        }
    }
    Ok(Restic {
        program,
        repository: repository.to_owned(),
        password,
        env,
        cache: Some(home.root.join("cache").join("restic")),
        timeout: Duration::from_secs(6 * 3600),
    })
}

pub fn ark(home: &Home, config: &Config, canon: Arc<Canon>, hush: &Hush) -> Result<Ark> {
    let ac = config.ark.as_ref().context("backups are not set up: add [ark] to henry.toml")?;
    std::fs::create_dir_all(home.root.join("scratch"))?;
    let primary = restic(home, ac, hush, &ac.repository)?;
    let mirrors = ac.mirrors.iter().map(|m| restic(home, ac, hush, m)).collect::<Result<Vec<_>>>()?;
    Ok(Ark::new(canon, primary, mirrors, engine(config), settings(home, ac)))
}
