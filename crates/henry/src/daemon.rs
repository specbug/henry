//! `henry run`: the long-lived process launchd keeps alive.
//!
//! It holds the single-instance lock, recovers interrupted jobs, and then runs
//! four things side by side:
//!
//! - Plumb, Proof, then Hinge, every tick. Plumb connects lazily: if the VM is
//!   down at boot, the first passes bring it up rather than Henry refusing to
//!   start. Proof judges what Plumb runs, and Hinge routes what Proof promoted
//!   in the same tick.
//! - The job worker, one job at a time (builds are heavy).
//! - Press's poll of every auto-deploying branch.
//! - The loopback HTTP endpoint, for GitHub's push webhook.
//!
//! On shutdown the side tasks are dropped mid-flight. That is safe: a job cut
//! off here is still marked running in Canon and is requeued at next start.

use std::fs::{File, TryLockError};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use canon::{Canon, serde_json};
use hold::{Engine, Machine};
use hush::{HOST, Hush};
use plumb::{Plumb, Settings};
use press::Press;
use press::build::CliBuilder;
use press::git::Git;
use proof::Proof;
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

    crate::edge::prepare(home, &config, &hush)?;
    let mut hinge = crate::edge::hinge(&config, canon.clone(), &hush)?;
    let mut last_hinge_error: Option<String> = None;

    let version = env!("CARGO_PKG_VERSION");
    canon.record(None, "henry.started", &format!("Henry {version} started"), &serde_json::Value::Null)?;
    info!(version, home = %home.root.display(), host_secrets, default_model = ?config.ai.default_model, "Henry is up");

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async {
        let side = [
            tokio::spawn(http(config.http.listen.clone(), canon.clone(), hush.clone())),
            tokio::spawn(work(home.clone(), config.clone(), canon.clone(), hush.clone())),
            tokio::spawn(poll(home.clone(), config.clone(), canon.clone(), hush.clone())),
        ];
        let mut term = signal(SignalKind::terminate())?;
        let mut int = signal(SignalKind::interrupt())?;
        let mut hup = signal(SignalKind::hangup())?;
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut engine: Option<(Plumb<Engine, Host>, Proof<Engine>)> = None;
        let mut last_connect_error: Option<String> = None;
        loop {
            tokio::select! {
                _ = term.recv() => break,
                _ = int.recv() => break,
                _ = hup.recv() => match Config::load(&home.config()).and_then(|c| c.limits().map(|_| c)) {
                    Ok(c) => {
                        config = c;
                        // Engine and Proof settings take effect on the next connect.
                        engine = None;
                        // Edge settings and Henry's own secrets, now.
                        if let Err(e) = crate::edge::prepare(home, &config, &hush) {
                            warn!("edge files not rewritten: {e:#}");
                        }
                        match crate::edge::hinge(&config, canon.clone(), &hush) {
                            Ok(h) => hinge = h,
                            Err(e) => warn!("keeping the old routing setup: {e:#}"),
                        }
                        info!(default_model = ?config.ai.default_model, "configuration reloaded");
                    }
                    Err(e) => warn!("configuration not reloaded, keeping the old one: {e:#}"),
                },
                _ = tick.tick() => {
                    if engine.is_none() {
                        match connect(&config, canon.clone(), hush.clone()).await {
                            Ok(e) => {
                                info!("connected to the container engine");
                                last_connect_error = None;
                                engine = Some(e);
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
                    if let Some((plumb, proof)) = &engine {
                        if let Err(e) = plumb.pass().await {
                            warn!("plumb pass failed: {e}");
                        }
                        match proof.pass().await {
                            Ok(r) => {
                                if let Some(why) = r.skipped {
                                    warn!("proof skipped a pass: {why}");
                                }
                            }
                            Err(e) => warn!("proof pass failed: {e}"),
                        }
                    }
                    match hinge.pass().await {
                        Ok(_) => last_hinge_error = None,
                        Err(e) => {
                            let e = e.to_string();
                            if last_hinge_error.as_deref() != Some(e.as_str()) {
                                warn!("routing not updated: {e}");
                                canon.record(None, "hinge.problem", &e, &serde_json::Value::Null)?;
                                last_hinge_error = Some(e);
                            }
                        }
                    }
                }
            }
        }
        for task in side {
            task.abort();
        }
        anyhow::Ok(())
    })?;

    canon.record(None, "henry.stopped", "Henry stopped", &serde_json::Value::Null)?;
    info!("Henry stopped");
    Ok(())
}

async fn http(listen: String, canon: Arc<Canon>, hush: Arc<Hush>) {
    if let Err(e) = crate::http::serve(&listen, canon, hush).await {
        // Webhooks are a speed-up, not a dependency: polling still deploys.
        warn!("the http endpoint is down, deploys fall back to polling: {e:#}");
    }
}

/// Build a Press for one use, so a GitHub token set with `henry secret set`
/// applies without a restart.
fn press(home: &Home, config: &Config, hush: &Hush, canon: Arc<Canon>) -> Result<Press<CliBuilder>> {
    let token = hush.get(HOST, "GITHUB_TOKEN")?.map(|t| t.expose().to_owned());
    let builder = CliBuilder {
        program: builder_program(config),
        timeout: Duration::from_secs(config.press.build_timeout_minutes * 60),
    };
    let settings = press::Settings {
        checkouts: home.checkouts(),
        logs: home.logs().join("builds"),
        limits: config.limits()?,
        build_retries: 1,
        retry_pause: Duration::from_secs(3),
    };
    Ok(Press::new(canon, Git { program: "git".into(), token }, builder, settings))
}

fn builder_program(config: &Config) -> std::path::PathBuf {
    if let Some(p) = &config.press.builder {
        return p.clone();
    }
    if cfg!(target_os = "macos") {
        return config.engine.podman.clone().unwrap_or_else(|| "/opt/homebrew/bin/podman".into());
    }
    let on_path = |name: &str| {
        std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
    };
    if on_path("podman") { "podman".into() } else { "docker".into() }
}

/// Run queued jobs, one at a time.
async fn work(home: Home, config: Config, canon: Arc<Canon>, hush: Arc<Hush>) {
    loop {
        let job = match canon.claim() {
            Ok(Some(job)) => job,
            Ok(None) => {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            Err(e) => {
                warn!("cannot read the job queue: {e}");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        let outcome = match job.kind.as_str() {
            press::JOB => match press(&home, &config, &hush, canon.clone()) {
                Ok(p) => p.run(&job).await.map(|o| o.to_string()).map_err(|e| (e.is_transient(), e.to_string())),
                Err(e) => Err((false, format!("{e:#}"))),
            },
            other => Err((false, format!("no handler for jobs of kind `{other}`"))),
        };
        let settled = match outcome {
            Ok(done) => {
                info!(job = job.id, kind = %job.kind, "{done}");
                canon.finish(job.id)
            }
            // Network trouble: try again later, backing off, up to six times.
            Err((true, e)) if job.attempts < 6 => {
                let wait = 30_000i64 << (job.attempts - 1).clamp(0, 6);
                warn!(job = job.id, "retrying in {}s: {e}", wait / 1000);
                canon.fail(job.id, &e, Some(canon::now_ms() + wait))
            }
            Err((_, e)) => {
                warn!(job = job.id, kind = %job.kind, "failed: {e}");
                let _ = canon.record(
                    job.app.as_deref(),
                    "job.failed",
                    &format!("{} failed: {e}", job.kind),
                    &serde_json::Value::Null,
                );
                canon.fail(job.id, &e, None)
            }
        };
        if let Err(e) = settled {
            warn!(job = job.id, "cannot record the job's outcome: {e}");
        }
    }
}

/// Check every auto-deploying branch for new commits.
async fn poll(home: Home, config: Config, canon: Arc<Canon>, hush: Arc<Hush>) {
    let mut every = tokio::time::interval(Duration::from_secs(config.press.poll_seconds.max(10)));
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        match press(&home, &config, &hush, canon.clone()) {
            Ok(p) => match p.poll().await {
                Ok(queued) if !queued.is_empty() => info!(?queued, "new commits, deploys queued"),
                Ok(_) => {}
                Err(e) => warn!("polling failed: {e}"),
            },
            Err(e) => warn!("cannot poll: {e:#}"),
        }
    }
}

/// Bring the engine up and connect to it. The daemon is the only process
/// that ever starts the Podman machine.
async fn connect(config: &Config, canon: Arc<Canon>, hush: Arc<Hush>) -> Result<(Plumb<Engine, Host>, Proof<Engine>)> {
    let host = Host::from_config(&config.engine)?;
    host.ensure_up().await?;
    let engine = host.connect().await?;
    let proof = Proof::new(canon.clone(), engine.clone(), config.proof.settings(&config.hinge.listen));
    Ok((Plumb::new(canon, hush, engine, host, Settings::default()), proof))
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
