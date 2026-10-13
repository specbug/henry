//! `henry run`: the long-lived process launchd keeps alive.
//!
//! It holds the single-instance lock, recovers interrupted jobs, and runs a
//! Plumb pass every tick. Plumb connects lazily: if the VM is down at boot,
//! the first passes bring it up rather than Henry refusing to start.

use std::fs::{File, TryLockError};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use canon::{Canon, serde_json};
use hold::{Engine, Machine};
use hush::Hush;
use plumb::{Plumb, Settings};
use tokio::signal::unix::{SignalKind, signal};
use tracing::{info, warn};

use crate::home::{Config, Home};
use crate::host::Host;

/// How often Plumb looks. Recovery after a crash takes at most this long.
const TICK: Duration = Duration::from_secs(15);

pub fn run(home: &Home) -> Result<()> {
    home.ensure()?;
    let _lock = lock(home)?;

    let mut config = Config::load(&home.config())?;
    config.limits()?;
    let canon = Arc::new(Canon::open(&home.canon()).context("opening Canon")?);

    let requeued = canon.requeue_interrupted()?;
    if requeued > 0 {
        warn!(requeued, "resuming jobs interrupted by the last shutdown");
    }
    let hush = Arc::new(crate::cli::open_hush(home, canon.clone())?);
    let host_secrets = hush.names(hush::HOST)?.len();

    let version = env!("CARGO_PKG_VERSION");
    canon.record(None, "henry.started", &format!("Henry {version} started"), &serde_json::Value::Null)?;
    info!(version, home = %home.root.display(), host_secrets, default_model = ?config.ai.default_model, "Henry is up");

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async {
        let mut term = signal(SignalKind::terminate())?;
        let mut int = signal(SignalKind::interrupt())?;
        let mut hup = signal(SignalKind::hangup())?;
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut plumb: Option<Plumb<Engine, Host>> = None;
        let mut last_connect_error: Option<String> = None;
        loop {
            tokio::select! {
                _ = term.recv() => break,
                _ = int.recv() => break,
                _ = hup.recv() => match Config::load(&home.config()).and_then(|c| c.limits().map(|_| c)) {
                    Ok(c) => {
                        config = c;
                        // Engine settings take effect on the next connect.
                        plumb = None;
                        info!(default_model = ?config.ai.default_model, "configuration reloaded");
                    }
                    Err(e) => warn!("configuration not reloaded, keeping the old one: {e:#}"),
                },
                _ = tick.tick() => {
                    if plumb.is_none() {
                        match connect(&config, canon.clone(), hush.clone()).await {
                            Ok(p) => {
                                info!("connected to the container engine");
                                last_connect_error = None;
                                plumb = Some(p);
                            }
                            Err(e) => {
                                let e = format!("{e:#}");
                                if last_connect_error.as_deref() != Some(e.as_str()) {
                                    warn!("cannot reach the container engine yet: {e}");
                                    canon.record(None, "host.problem", &e, &serde_json::Value::Null)?;
                                    last_connect_error = Some(e);
                                }
                            }
                        }
                    }
                    if let Some(p) = &plumb
                        && let Err(e) = p.pass().await
                    {
                        warn!("plumb pass failed: {e}");
                    }
                }
            }
        }
        anyhow::Ok(())
    })?;

    canon.record(None, "henry.stopped", "Henry stopped", &serde_json::Value::Null)?;
    info!("Henry stopped");
    Ok(())
}

/// Bring the engine up and connect to it. The daemon is the only process
/// that ever starts the Podman machine.
async fn connect(config: &Config, canon: Arc<Canon>, hush: Arc<Hush>) -> Result<Plumb<Engine, Host>> {
    let host = Host::from_config(&config.engine)?;
    host.ensure_up().await?;
    let engine = host.connect().await?;
    Ok(Plumb::new(canon, hush, engine, host, Settings::default()))
}

/// Hold `henry.lock` for the life of the process. The OS drops the lock when
/// the process dies, however it dies, so a stale lock cannot exist. hostd
/// needed a PID re-check dance to approximate this.
fn lock(home: &Home) -> Result<File> {
    let mut file = File::options().create(true).truncate(false).write(true).open(home.lock())?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => bail!("another Henry is already running from {}", home.root.display()),
        Err(TryLockError::Error(e)) => return Err(e).context("locking henry.lock"),
    }
    file.set_len(0)?;
    writeln!(file, "{}", std::process::id())?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_henry_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home { root: dir.path().to_path_buf() };
        let first = lock(&home).unwrap();
        let err = lock(&home).unwrap_err().to_string();
        assert!(err.contains("already running"), "{err}");
        drop(first);
        lock(&home).unwrap();
    }
}
