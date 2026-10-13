//! `henry run`: the long-lived process launchd keeps alive.
//!
//! For now it holds the single-instance lock, recovers interrupted jobs,
//! proves it can open Hush, and waits on its signals. Plumb's reconcile pass
//! slots into the tick in milestone 2.

use std::fs::{File, TryLockError};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use canon::{Canon, serde_json};
use tokio::signal::unix::{SignalKind, signal};
use tracing::{info, warn};

use crate::home::{Config, Home};

const TICK: Duration = Duration::from_secs(30);

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
    let hush = crate::cli::open_hush(home, canon.clone())?;
    let host_secrets = hush.names(hush::HOST)?.len();

    let version = env!("CARGO_PKG_VERSION");
    canon.record(None, "henry.started", &format!("Henry {version} started"), &serde_json::Value::Null)?;
    info!(version, home = %home.root.display(), host_secrets, default_model = ?config.ai.default_model, "Henry is up");

    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async {
        let mut term = signal(SignalKind::terminate())?;
        let mut int = signal(SignalKind::interrupt())?;
        let mut hup = signal(SignalKind::hangup())?;
        let mut tick = tokio::time::interval(TICK);
        loop {
            tokio::select! {
                _ = term.recv() => break,
                _ = int.recv() => break,
                _ = hup.recv() => match Config::load(&home.config()).and_then(|c| c.limits().map(|_| c)) {
                    Ok(c) => {
                        config = c;
                        info!(default_model = ?config.ai.default_model, "configuration reloaded");
                    }
                    Err(e) => warn!("configuration not reloaded, keeping the old one: {e:#}"),
                },
                _ = tick.tick() => {
                    tracing::debug!(apps = canon.apps()?.len(), "tick");
                }
            }
        }
        anyhow::Ok(())
    })?;

    canon.record(None, "henry.stopped", "Henry stopped", &serde_json::Value::Null)?;
    info!("Henry stopped");
    Ok(())
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
