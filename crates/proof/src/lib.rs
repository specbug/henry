//! Proof: the Live gate.
//!
//! A release becomes live by enduring, not by argument. Proof takes each
//! release on trial through three stages, saving its progress in Canon so a
//! reboot resumes the trial rather than restarting it:
//!
//! 1. **Waiting**: every container of the release is running.
//! 2. **Soak**: for the soak time, nothing restarts, nothing logs a crash, and
//!    the health probes pass enough times in a row.
//! 3. **Settle**: the release's own containers are restarted, and must come
//!    back healthy.
//!
//! Then the release is live. Proof keeps probing live releases: a service
//! that keeps failing makes its app Degraded, and one that fails for long
//! enough is restarted, at most once per cooldown.

pub mod probe;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use canon::{App, Canon, Desired, Release, ReleaseState, Status, now_ms, serde_json};
use hold::{Container, Runtime, State, label};
use lease::{Lease, Service};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Canon(#[from] canon::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone)]
pub struct Settings {
    /// How long a release must run cleanly before its restart test.
    pub soak: Duration,
    /// Passing probes in a row, per service, the soak must end on.
    pub streak: u32,
    /// Give up on a soak that has not held after this long.
    pub soak_limit: Duration,
    /// Give up on a release whose containers never all come up.
    pub start_timeout: Duration,
    /// Passing probes in a row after the restart test.
    pub settle_streak: u32,
    pub settle_timeout: Duration,
    pub probe_timeout: Duration,
    /// Log lines that mean a crash, whatever the language.
    pub signatures: Vec<String>,
    /// Caddy's listen address, to check public routes end to end.
    pub proxy: Option<String>,
    /// Failing probes in a row before a live app is Degraded.
    pub degraded_after: u32,
    /// Failing probes in a row before the container is restarted.
    pub restart_after: u32,
    pub restart_cooldown: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            soak: Duration::from_secs(600),
            streak: 30,
            soak_limit: Duration::from_secs(1800),
            start_timeout: Duration::from_secs(600),
            settle_streak: 3,
            settle_timeout: Duration::from_secs(180),
            probe_timeout: Duration::from_secs(5),
            signatures: [
                "panicked at",
                "panic: ",
                "Traceback (most recent call last)",
                "Segmentation fault",
                "core dumped",
                "FATAL ERROR:",
                "OutOfMemoryError",
                "Exception in thread \"main\"",
                "UnhandledPromiseRejection",
            ]
            .map(String::from)
            .to_vec(),
            proxy: None,
            degraded_after: 3,
            restart_after: 6,
            restart_cooldown: Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Waiting,
    Soak,
    Settle,
}

/// A trial's progress, saved in Canon after every pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trial {
    pub stage: Stage,
    pub started_at: i64,
    pub stage_at: i64,
    pub streak: u32,
    pub probes: u32,
    pub failed_probes: u32,
    pub last_failure: Option<String>,
    /// Restart counts when the stage began, by container name.
    pub baseline: BTreeMap<String, i64>,
    /// Logs are scanned from here on, in Unix seconds.
    pub log_since: i64,
}

impl Trial {
    fn new(now: i64) -> Trial {
        Trial {
            stage: Stage::Waiting,
            started_at: now,
            stage_at: now,
            streak: 0,
            probes: 0,
            failed_probes: 0,
            last_failure: None,
            baseline: BTreeMap::new(),
            log_since: now / 1000,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub promoted: Vec<(String, i64)>,
    pub failed: Vec<(String, i64, String)>,
    pub degraded: Vec<String>,
    pub recovered: Vec<String>,
    pub restarted: Vec<String>,
    pub skipped: Option<String>,
}

#[derive(Default)]
struct Watch {
    failures: u32,
    last_failure: String,
    last_restart: Option<Instant>,
    restarts_seen: Option<i64>,
}

pub struct Proof<R> {
    canon: Arc<Canon>,
    runtime: R,
    settings: Settings,
    watch: Mutex<HashMap<(String, String), Watch>>,
}

/// One service of a release and the container serving it.
struct Member<'a> {
    service: &'a Service,
    container: Option<&'a Container>,
    port: Option<u16>,
    /// Restarting it would interrupt the live release too.
    shared: bool,
}

fn now_secs() -> i64 {
    now_ms() / 1000
}

impl<R: Runtime> Proof<R> {
    pub fn new(canon: Arc<Canon>, runtime: R, settings: Settings) -> Self {
        Proof { canon, runtime, settings, watch: Mutex::new(HashMap::new()) }
    }

    pub async fn pass(&self) -> Result<Report> {
        let mut report = Report::default();
        let containers = match self.runtime.list().await {
            Ok(c) => c,
            Err(e) => {
                report.skipped = Some(format!("cannot list containers: {e}"));
                return Ok(report);
            }
        };
        for app in self.canon.apps()? {
            if app.desired == Desired::Stopped {
                continue;
            }
            let wanted = self.canon.wanted_releases(&app.name)?;
            if let Some(candidate) = &wanted.candidate {
                self.trial(&app, candidate, &containers, &mut report).await?;
            }
            if let Some(live) = &wanted.live {
                self.watch(&app, live, &containers, &mut report).await?;
            }
        }
        Ok(report)
    }

    fn members<'a>(
        &self,
        app: &App,
        release: &Release,
        lease: &'a Lease,
        containers: &'a [Container],
    ) -> Result<Vec<Member<'a>>> {
        let mut out = Vec::new();
        for service in &lease.services {
            let stateful = !service.volumes.is_empty();
            let id = release.id.to_string();
            let container = containers.iter().find(|c| {
                c.label(label::APP) == Some(&app.name)
                    && c.label(label::SERVICE) == Some(&service.name)
                    && if stateful { c.label(label::RELEASE).is_none() } else { c.label(label::RELEASE) == Some(&id) }
            });
            let slot = if stateful { 0 } else { release.id };
            let port = self.canon.existing_port(&app.name, &service.name, slot)?;
            out.push(Member { service, container, port, shared: stateful && app.live_release.is_some() });
        }
        Ok(out)
    }

    // --- the gate ----------------------------------------------------------

    async fn trial(&self, app: &App, release: &Release, containers: &[Container], report: &mut Report) -> Result<()> {
        let lease = match Lease::parse(&release.lease) {
            Ok(l) => l,
            Err(e) => return self.fail(app, release, format!("its lease no longer passes the checks: {e}"), report),
        };
        let mut t: Trial = self
            .canon
            .proof(release.id)?
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_else(|| Trial::new(now_ms()));
        let members = self.members(app, release, &lease, containers)?;
        let now = now_ms();

        match t.stage {
            Stage::Waiting => {
                let missing: Vec<&str> = members
                    .iter()
                    .filter(|m| !m.container.is_some_and(|c| c.state == State::Running))
                    .map(|m| m.service.name.as_str())
                    .collect();
                if missing.is_empty() {
                    t.baseline = self.restart_counts(&members).await;
                    t.stage = Stage::Soak;
                    t.stage_at = now;
                    t.log_since = now / 1000;
                    self.canon.set_release_state(release.id, ReleaseState::Proof, Some("soak started"))?;
                } else if app.status == Status::NeedsYou {
                    // Waiting on you for a secret is not the release's fault:
                    // the clock starts once Plumb can start it.
                    t.started_at = now;
                } else if now - t.started_at > self.settings.start_timeout.as_millis() as i64 {
                    let why = format!("never came up: {} not running", missing.join(", "));
                    return self.fail(app, release, why, report);
                }
            }
            Stage::Soak => {
                if let Err(why) = self.check_containers(&members, &t.baseline).await {
                    return self.fail(app, release, why, report);
                }
                if let Err(why) = self.check_logs(&members, t.log_since).await {
                    return self.fail(app, release, why, report);
                }
                t.log_since = now_secs();
                self.probe_all(&members, &mut t).await;
                let soaked = now - t.stage_at >= self.settings.soak.as_millis() as i64;
                if soaked && t.streak >= self.settings.streak {
                    self.restart_test(app, &members).await;
                    t.baseline = self.restart_counts(&members).await;
                    t.stage = Stage::Settle;
                    t.stage_at = now_ms();
                    t.streak = 0;
                } else if now - t.stage_at > self.settings.soak_limit.as_millis() as i64 {
                    let last = t.last_failure.clone().unwrap_or_default();
                    let why = format!(
                        "health probes never held {} times in a row; last failure: {last}",
                        self.settings.streak
                    );
                    return self.fail(app, release, why, report);
                }
            }
            Stage::Settle => {
                if let Err(why) = self.check_containers(&members, &t.baseline).await {
                    return self.fail(app, release, format!("after the restart test, {why}"), report);
                }
                self.probe_all(&members, &mut t).await;
                if t.streak >= self.settings.settle_streak {
                    return self.promote(app, release, &t, report);
                }
                if now - t.stage_at > self.settings.settle_timeout.as_millis() as i64 {
                    let last = t.last_failure.clone().unwrap_or_default();
                    return self.fail(
                        app,
                        release,
                        format!("did not come back healthy after a restart: {last}"),
                        report,
                    );
                }
            }
        }
        self.canon.put_proof(release.id, &app.name, &serde_json::to_value(&t).expect("a trial always serializes"))?;
        Ok(())
    }

    async fn restart_counts(&self, members: &[Member<'_>]) -> BTreeMap<String, i64> {
        let mut out = BTreeMap::new();
        for m in members {
            if let Some(c) = m.container
                && let Ok(i) = self.runtime.inspect(&c.id).await
            {
                out.insert(c.name.clone(), i.restart_count);
            }
        }
        out
    }

    /// Every container still running, and none restarted by the engine.
    async fn check_containers(&self, members: &[Member<'_>], baseline: &BTreeMap<String, i64>) -> Result<(), String> {
        for m in members {
            let name = &m.service.name;
            let Some(c) = m.container else { return Err(format!("`{name}` is gone")) };
            let seen = self.runtime.inspect(&c.id).await.map_err(|e| format!("cannot inspect `{name}`: {e}"))?;
            if seen.oom_killed {
                return Err(format!("`{name}` was killed for using too much memory"));
            }
            if seen.state != State::Running {
                let code = seen.exit_code.map(|c| format!(" (exit {c})")).unwrap_or_default();
                return Err(format!("`{name}` stopped{code}"));
            }
            let before = baseline.get(&c.name).copied().unwrap_or(0);
            if seen.restart_count > before {
                return Err(format!("`{name}` crashed and was restarted {} time(s)", seen.restart_count - before));
            }
        }
        Ok(())
    }

    /// No crash signature in anything logged since `since`.
    async fn check_logs(&self, members: &[Member<'_>], since: i64) -> Result<(), String> {
        for m in members {
            let Some(c) = m.container else { continue };
            let Ok(text) = self.runtime.logs(&c.id, 2000, Some(since)).await else { continue };
            for line in text.lines() {
                if self.settings.signatures.iter().any(|s| line.contains(s.as_str())) {
                    let line: String = line.trim().chars().take(200).collect();
                    return Err(format!("`{}` logged a crash: {line}", m.service.name));
                }
            }
        }
        Ok(())
    }

    /// Probe every service once. All passing extends the streak; any
    /// failing ends it.
    async fn probe_all(&self, members: &[Member<'_>], t: &mut Trial) {
        let mut failure = None;
        for m in members {
            let Some(port) = m.port else { continue };
            if let Err(e) = probe::probe(&m.service.health, port, self.settings.probe_timeout).await {
                failure = Some(format!("`{}`: {e}", m.service.name));
                break;
            }
        }
        t.probes += 1;
        match failure {
            None => t.streak += 1,
            Some(f) => {
                t.streak = 0;
                t.failed_probes += 1;
                t.last_failure = Some(f);
            }
        }
    }

    /// Stop and start the release's own containers. A stateful one shared
    /// with the live release is left alone: restarting it would interrupt
    /// the live app to test a candidate.
    async fn restart_test(&self, app: &App, members: &[Member<'_>]) {
        for m in members.iter().filter(|m| !m.shared) {
            let Some(c) = m.container else { continue };
            let restarted = async {
                self.runtime.stop(&c.id, Duration::from_secs(20)).await?;
                self.runtime.start(&c.id).await
            };
            if let Err(e) = restarted.await {
                warn!(app = %app.name, container = %c.name, "restart test could not restart it: {e}");
            }
        }
    }

    fn promote(&self, app: &App, release: &Release, t: &Trial, report: &mut Report) -> Result<()> {
        let soaked = (t.stage_at - t.started_at).max(0) / 60_000;
        let summary = format!(
            "proven: soaked {soaked} min, {} probes ({} failed), survived a restart",
            t.probes, t.failed_probes
        );
        self.canon.set_release_state(release.id, ReleaseState::Live, Some(&summary))?;
        self.canon.set_status(&app.name, Status::Live, None)?;
        self.canon.put_proof(release.id, &app.name, &serde_json::to_value(t).expect("a trial always serializes"))?;
        self.watch.lock().unwrap().retain(|(a, _), _| *a != app.name);
        info!(app = %app.name, release = release.id, "{summary}");
        report.promoted.push((app.name.clone(), release.id));
        Ok(())
    }

    fn fail(&self, app: &App, release: &Release, why: String, report: &mut Report) -> Result<()> {
        warn!(app = %app.name, release = release.id, "failed proof: {why}");
        self.canon.set_release_state(release.id, ReleaseState::Failed, Some(&format!("failed proof: {why}")))?;
        if app.live_release.is_none() {
            self.canon.set_status(&app.name, Status::Stuck, Some(&why))?;
        }
        report.failed.push((app.name.clone(), release.id, why));
        Ok(())
    }

    // --- after the gate ----------------------------------------------------

    async fn watch(&self, app: &App, live: &Release, containers: &[Container], report: &mut Report) -> Result<()> {
        let Ok(lease) = Lease::parse(&live.lease) else { return Ok(()) };
        let members = self.members(app, live, &lease, containers)?;
        let mut failing: Vec<String> = Vec::new();
        for m in &members {
            let Some(c) = m.container.filter(|c| c.state == State::Running) else { continue };
            let Some(port) = m.port else { continue };
            let mut result = probe::probe(&m.service.health, port, self.settings.probe_timeout).await;
            if result.is_ok()
                && let (Some(proxy), Some(public)) = (&self.settings.proxy, &m.service.public)
            {
                result = through_edge(proxy, &public.hostname, &public.path, self.settings.probe_timeout).await;
            }
            let restarts = self.runtime.inspect(&c.id).await.map(|i| i.restart_count).ok();

            let key = (app.name.clone(), m.service.name.clone());
            let due = {
                let mut watch = self.watch.lock().unwrap();
                let w = watch.entry(key).or_default();
                if let (Some(now), Some(before)) = (restarts, w.restarts_seen)
                    && now > before
                {
                    let _ = self.canon.record(
                        Some(&app.name),
                        "proof.crash",
                        &format!("`{}` crashed and the engine restarted it", m.service.name),
                        &serde_json::Value::Null,
                    );
                }
                w.restarts_seen = restarts.or(w.restarts_seen);
                match result {
                    Ok(()) => w.failures = 0,
                    Err(e) => {
                        w.failures += 1;
                        w.last_failure = e;
                    }
                }
                if w.failures >= self.settings.degraded_after {
                    failing.push(format!("`{}` failing its health check: {}", m.service.name, w.last_failure));
                }
                let due = w.failures >= self.settings.restart_after
                    && w.last_restart.is_none_or(|t| t.elapsed() >= self.settings.restart_cooldown);
                if due {
                    w.last_restart = Some(Instant::now());
                }
                due
            };
            if due {
                let restarted = async {
                    self.runtime.stop(&c.id, Duration::from_secs(20)).await?;
                    self.runtime.start(&c.id).await
                };
                let message = match restarted.await {
                    Ok(()) => format!("restarted `{}` after it kept failing its health check", m.service.name),
                    Err(e) => format!("could not restart `{}`: {e}", m.service.name),
                };
                self.canon.record(Some(&app.name), "proof.restart", &message, &serde_json::Value::Null)?;
                report.restarted.push(c.name.clone());
            }
        }

        let status = self.canon.app(&app.name)?.status;
        if !failing.is_empty() {
            // Down (containers not running) is Plumb's word and outranks this.
            if matches!(status, Status::Live | Status::Degraded) {
                self.canon.set_status(&app.name, Status::Degraded, Some(&failing.join("; ")))?;
                report.degraded.push(app.name.clone());
            }
        } else if status == Status::Degraded {
            self.canon.set_status(&app.name, Status::Live, None)?;
            report.recovered.push(app.name.clone());
        }
        Ok(())
    }
}

/// A public route, end to end through Caddy: anything but a gateway error
/// means the edge reached the app.
async fn through_edge(proxy: &str, host: &str, path: &str, timeout: Duration) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let res = client
        .get(format!("http://{proxy}{path}"))
        .header("host", host)
        .send()
        .await
        .map_err(|e| format!("Caddy did not answer for {host}: {e}"))?;
    match res.status().as_u16() {
        502..=504 => Err(format!("Caddy could not reach the app for {host}{path} ({})", res.status())),
        404 if res.headers().contains_key("x-henry-route") => Err(format!("Caddy has no route for {host}{path}")),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests;
