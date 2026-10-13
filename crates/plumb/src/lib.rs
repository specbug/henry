//! Plumb: brings what is running back to true.
//!
//! Each pass reads Canon, works out what should run ([`plan`]), looks at what
//! the engine actually has, and closes the gap: create what is missing,
//! recreate what drifted, start what stopped, remove what nobody wants.
//! A reboot, a crash and a half-finished deploy are the same event to Plumb:
//! a large gap.
//!
//! Plumb never trusts a failed observation. If the engine cannot list its
//! containers, the pass stops before it acts, because "I could not see
//! anything" is not "nothing is running".

pub mod plan;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use canon::{Canon, Desired, Status, serde_json};
use hold::{Container, Labels, Machine, MachineState, Runtime, State, label};
use hush::Hush;
use tracing::{info, warn};

use crate::plan::{AppPlan, Planned, plan_app};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Canon(#[from] canon::Error),
    #[error(transparent)]
    Hush(#[from] hush::Error),
    #[error("release {0} carries a lease Henry can no longer accept: {1}")]
    Lease(i64, String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone)]
pub struct Settings {
    /// Host ports handed out to published services.
    pub ports: Range<u16>,
    /// Where published ports listen. Loopback: only Hinge reaches them.
    pub publish_ip: String,
    /// How long a container gets to stop before it is killed.
    pub grace: Duration,
    /// The first retry after a failed start; it doubles up to `max_backoff`.
    pub backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            ports: 20000..30000,
            publish_ip: "127.0.0.1".into(),
            grace: Duration::from_secs(20),
            backoff: Duration::from_secs(15),
            max_backoff: Duration::from_secs(600),
        }
    }
}

/// What one pass did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub created: Vec<String>,
    pub recreated: Vec<String>,
    pub started: Vec<String>,
    pub removed: Vec<String>,
    pub failed: Vec<(String, String)>,
    /// Containers not started because something they depend on is not up.
    pub waiting: Vec<(String, Vec<String>)>,
    /// Why the pass did nothing at all, if it did nothing at all.
    pub skipped: Option<String>,
}

impl Report {
    pub fn is_quiet(&self) -> bool {
        self.created.is_empty()
            && self.recreated.is_empty()
            && self.started.is_empty()
            && self.removed.is_empty()
            && self.failed.is_empty()
            && self.waiting.is_empty()
            && self.skipped.is_none()
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(why) = &self.skipped {
            return write!(f, "skipped: {why}");
        }
        write!(
            f,
            "{} created, {} recreated, {} started, {} removed, {} failed, {} waiting",
            self.created.len(),
            self.recreated.len(),
            self.started.len(),
            self.removed.len(),
            self.failed.len(),
            self.waiting.len()
        )
    }
}

struct Backoff {
    failures: u32,
    retry_at: Instant,
    last_error: String,
}

pub struct Plumb<R, M> {
    canon: Arc<Canon>,
    hush: Arc<Hush>,
    runtime: R,
    machine: M,
    settings: Settings,
    backoff: Mutex<HashMap<String, Backoff>>,
    host_problem: Mutex<Option<String>>,
}

impl<R: Runtime, M: Machine> Plumb<R, M> {
    pub fn new(canon: Arc<Canon>, hush: Arc<Hush>, runtime: R, machine: M, settings: Settings) -> Self {
        Plumb {
            canon,
            hush,
            runtime,
            machine,
            settings,
            backoff: Mutex::new(HashMap::new()),
            host_problem: Mutex::new(None),
        }
    }

    pub fn runtime(&self) -> &R {
        &self.runtime
    }

    /// One reconcile pass. Safe to run at any time, any number of times.
    pub async fn pass(&self) -> Result<Report> {
        let mut report = Report::default();

        match self.machine.ensure_up().await {
            Ok(MachineState::Up) => self.host_ok()?,
            Ok(MachineState::Started) => {
                self.host_ok()?;
                self.canon.record(None, "host.machine", "started the Podman machine", &serde_json::Value::Null)?;
            }
            Err(e) => return self.skip(report, format!("the engine's VM is not up: {e}")),
        }

        let actual = match self.list_twice().await {
            Ok(list) => list,
            Err(e) => return self.skip(report, format!("could not list containers, so changing nothing: {e}")),
        };
        let by_name: HashMap<&str, &Container> = actual.iter().map(|c| (c.name.as_str(), c)).collect();

        let mut keep: BTreeSet<String> = BTreeSet::new();
        let mut planned_apps: BTreeSet<String> = BTreeSet::new();
        for app in self.canon.apps()? {
            planned_apps.insert(app.name.clone());
            let plan = match plan_app(&self.canon, &self.hush, &app, &self.settings) {
                Ok(p) => p,
                Err(e) => {
                    // One unreadable app must not stop the others, and its
                    // containers stay as they are until it is readable again.
                    warn!(app = %app.name, "cannot plan: {e}");
                    keep.extend(
                        actual.iter().filter(|c| c.label(label::APP) == Some(&app.name)).map(|c| c.name.clone()),
                    );
                    report.failed.push((app.name.clone(), e.to_string()));
                    continue;
                }
            };
            if plan.frozen() {
                keep.extend(actual.iter().filter(|c| c.label(label::APP) == Some(&app.name)).map(|c| c.name.clone()));
                let detail = format!("missing secrets: {}", plan.missing_secrets.join(", "));
                self.canon.set_status(&app.name, Status::NeedsYou, Some(&detail))?;
                continue;
            }
            for problem in &plan.problems {
                warn!(app = %app.name, "{problem}");
            }
            keep.extend(plan.containers.iter().map(|p| p.spec.name.clone()));
            let down = self.converge_app(&plan, &by_name, &mut report).await?;
            self.settle_status(&plan, &down)?;
            let slots: Vec<(String, i64)> = plan.containers.iter().map(|p| (p.service.clone(), p.slot)).collect();
            self.canon.free_ports(&app.name, &slots)?;
        }

        // Whatever Henry manages and nobody wants any more: old releases,
        // stopped apps, removed apps.
        for c in &actual {
            if keep.contains(&c.name) {
                continue;
            }
            match self.retire(c).await {
                Ok(()) => {
                    let app = c.label(label::APP).filter(|a| planned_apps.contains(*a));
                    self.canon.record(
                        app,
                        "container.removed",
                        &format!("removed {}", c.name),
                        &serde_json::Value::Null,
                    )?;
                    report.removed.push(c.name.clone());
                }
                Err(e) => report.failed.push((c.name.clone(), e.to_string())),
            }
        }

        // An app asked to stop is Stopped once nothing of it remains.
        let failed: BTreeSet<&str> = report.failed.iter().map(|(n, _)| n.as_str()).collect();
        for app in self.canon.apps()? {
            let lingering = actual.iter().any(|c| {
                c.label(label::APP) == Some(&app.name) && (keep.contains(&c.name) || failed.contains(c.name.as_str()))
            });
            if app.desired == Desired::Stopped && !lingering {
                self.canon.set_status(&app.name, Status::Stopped, None)?;
            }
        }

        if !report.is_quiet() {
            info!(%report, "plumb pass");
        }
        Ok(report)
    }

    /// Bring one app's containers in line with its plan. Returns the services
    /// serving its live release that are not running at the end of the pass.
    async fn converge_app(
        &self,
        plan: &AppPlan,
        actual: &HashMap<&str, &Container>,
        report: &mut Report,
    ) -> Result<Vec<String>> {
        let everything = || plan.containers.iter().filter(|p| p.serves_live).map(|p| p.service.clone()).collect();
        let labels =
            Labels::from([(label::MANAGED.to_owned(), "1".to_owned()), (label::APP.to_owned(), plan.app.clone())]);
        if !plan.containers.is_empty()
            && let Err(e) = self.runtime.ensure_network(&plan.network, &labels).await
        {
            report.failed.push((plan.network.clone(), e.to_string()));
            return Ok(everything());
        }
        for volume in &plan.volumes {
            if let Err(e) = self.runtime.ensure_volume(volume, &labels).await {
                report.failed.push((volume.clone(), e.to_string()));
                return Ok(everything());
            }
        }

        let mut down = Vec::new();
        let mut up: HashSet<(i64, &str)> = HashSet::new();
        for planned in &plan.containers {
            let found = actual.get(planned.spec.name.as_str()).copied();
            // Only gate *starting*: a dependent already running is left alone
            // when its dependency dies, since most reconnect on their own.
            let blocked: Vec<String> = planned
                .depends_on
                .iter()
                .filter(|d| !up.contains(&(planned.release, d.as_str())) && !up.contains(&(0, d.as_str())))
                .cloned()
                .collect();
            let ok = if !blocked.is_empty() && !is_current(planned, found) {
                report.waiting.push((planned.spec.name.clone(), blocked));
                false
            } else {
                self.converge_one(&plan.app, planned, found, report).await?
            };
            if ok {
                up.insert(planned.key());
            } else if planned.serves_live {
                down.push(planned.service.clone());
            }
        }
        Ok(down)
    }

    /// Returns whether the container ends the pass running.
    async fn converge_one(
        &self,
        app: &str,
        planned: &Planned,
        found: Option<&Container>,
        report: &mut Report,
    ) -> Result<bool> {
        let name = &planned.spec.name;
        let hash = planned.spec.hash();
        let current = found.filter(|c| c.spec_hash() == Some(hash.as_str()));
        if is_current(planned, found) {
            self.clear_backoff(name);
            return Ok(true);
        }
        if !self.may_try(name) {
            return Ok(false);
        }

        let attempt = async {
            let id = match (found, current) {
                (_, Some(c)) => c.id.clone(),
                (Some(stale), None) => {
                    self.retire(stale).await?;
                    self.create(planned).await?
                }
                (None, None) => self.create(planned).await?,
            };
            self.runtime.start(&id).await
        };
        match attempt.await {
            Ok(()) => {
                self.clear_backoff(name);
                let (what, list) = match (found, current) {
                    (Some(_), None) => ("recreated", &mut report.recreated),
                    (None, _) => ("created", &mut report.created),
                    (Some(_), Some(_)) => ("started", &mut report.started),
                };
                list.push(name.clone());
                self.canon.record(
                    Some(app),
                    &format!("container.{what}"),
                    &format!("{what} {name}"),
                    &serde_json::Value::Null,
                )?;
                Ok(true)
            }
            Err(e) => {
                let message = e.to_string();
                if self.note_failure(name, &message) {
                    self.canon.record(
                        Some(app),
                        "container.failed",
                        &format!("{name} would not start: {message}"),
                        &serde_json::Value::Null,
                    )?;
                }
                report.failed.push((name.clone(), message));
                Ok(false)
            }
        }
    }

    async fn create(&self, planned: &Planned) -> hold::Result<String> {
        self.runtime.ensure_image(&planned.spec.image).await?;
        self.runtime.create(&planned.spec).await
    }

    async fn retire(&self, c: &Container) -> hold::Result<()> {
        if c.state == State::Running {
            self.runtime.stop(&c.id, self.settings.grace).await?;
        }
        self.runtime.remove(&c.id).await
    }

    fn settle_status(&self, plan: &AppPlan, down: &[String]) -> Result<()> {
        let app = self.canon.app(&plan.app)?;
        if plan.has_live {
            if down.is_empty() {
                self.canon.set_status(&plan.app, Status::Live, None)?;
            } else {
                self.canon.set_status(&plan.app, Status::Down, Some(&format!("{} not running", down.join(", "))))?;
            }
        } else if matches!(app.status, Status::NeedsYou | Status::Stopped) {
            // Unblocked or restarted before it ever went live: back to onboarding.
            self.canon.set_status(&plan.app, Status::Onboarding, None)?;
        }
        Ok(())
    }

    async fn list_twice(&self) -> hold::Result<Vec<Container>> {
        match self.runtime.list().await {
            Ok(list) => Ok(list),
            Err(first) => {
                warn!("listing containers failed, retrying once: {first}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                self.runtime.list().await
            }
        }
    }

    fn skip(&self, mut report: Report, why: String) -> Result<Report> {
        let mut last = self.host_problem.lock().unwrap();
        if last.as_deref() != Some(why.as_str()) {
            warn!("{why}");
            self.canon.record(None, "host.problem", &why, &serde_json::Value::Null)?;
            *last = Some(why.clone());
        }
        report.skipped = Some(why);
        Ok(report)
    }

    fn host_ok(&self) -> Result<()> {
        if self.host_problem.lock().unwrap().take().is_some() {
            self.canon.record(None, "host.recovered", "the engine is reachable again", &serde_json::Value::Null)?;
        }
        Ok(())
    }

    fn may_try(&self, name: &str) -> bool {
        self.backoff.lock().unwrap().get(name).is_none_or(|b| Instant::now() >= b.retry_at)
    }

    /// Returns true when this failure is news: the first of a streak, or a
    /// different error from the last one. Repeats are not logged as events.
    fn note_failure(&self, name: &str, error: &str) -> bool {
        let mut map = self.backoff.lock().unwrap();
        let entry = map.entry(name.to_owned()).or_insert(Backoff {
            failures: 0,
            retry_at: Instant::now(),
            last_error: String::new(),
        });
        entry.failures += 1;
        let delay =
            self.settings.backoff.saturating_mul(1 << (entry.failures - 1).min(16)).min(self.settings.max_backoff);
        entry.retry_at = Instant::now() + delay;
        let news = entry.failures == 1 || entry.last_error != error;
        entry.last_error = error.to_owned();
        news
    }

    fn clear_backoff(&self, name: &str) {
        self.backoff.lock().unwrap().remove(name);
    }
}

/// Running, and built from exactly this spec.
fn is_current(planned: &Planned, found: Option<&Container>) -> bool {
    found.is_some_and(|c| c.state == State::Running && c.spec_hash() == Some(planned.spec.hash().as_str()))
}

/// Group containers by app, for `henry ps`.
pub fn by_app(containers: &[Container]) -> BTreeMap<String, Vec<&Container>> {
    let mut out: BTreeMap<String, Vec<&Container>> = BTreeMap::new();
    for c in containers {
        out.entry(c.label(label::APP).unwrap_or("?").to_owned()).or_default().push(c);
    }
    out
}

#[cfg(test)]
mod tests;
