//! Draft: onboarding, from repo URL to Live.
//!
//! A draft is a first version you expect to revise, and the pull of a ship's
//! hull through the water: how deep she sits. Draft takes a repo through
//! rounds. Each round it surveys the repo (Sound), picks a plan (the repo's
//! own lease, Sound's skeleton, or Ghost's proposal), checks it, builds it
//! (Press) and puts it on trial (Proof). A plan that fails sends its failure
//! back to Ghost, which revises it, up to the repair budget.
//!
//! The job ([`Draft::run`]) does the slow part: planning and building. The
//! tick ([`pass`]) watches each trial and starts the next round when one
//! fails. Every step is written to Canon before it is taken, so a crash
//! resumes the onboarding rather than restarting it.

use std::path::PathBuf;
use std::sync::Arc;

use canon::{Canon, DraftStage, Job, NewProposal, ReleaseState, Source, Status, serde_json};
use ghost::{Attempt, Brief, Workspace};
use hush::{Hush, Secret};
use press::build::Builder;
use press::git::Git;
use press::{Outcome as Built, Press};
use sound::{Ask, Generate, Plan, SkeletonSource, Survey};
use tracing::{info, warn};

/// The Canon job kind Draft runs.
pub const JOB: &str = "draft";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Worth retrying later: the network, a provider's rate limit.
    #[error("{0}")]
    Transient(String),
    #[error("{0}")]
    Io(String),
    #[error(transparent)]
    Canon(#[from] canon::Error),
    #[error(transparent)]
    Hush(#[from] hush::Error),
}

impl Error {
    pub fn is_transient(&self) -> bool {
        matches!(self, Error::Transient(_))
    }
}

impl From<press::Error> for Error {
    fn from(e: press::Error) -> Error {
        match e {
            press::Error::Transient(m) => Error::Transient(m),
            press::Error::Canon(c) => Error::Canon(c),
            other => Error::Io(other.to_string()),
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone)]
pub struct Settings {
    /// Henry's own clones; Draft shares Press's checkout of each app.
    pub checkouts: PathBuf,
    pub limits: lease::Limits,
    /// Plans tried before an onboarding is stuck: the repair budget.
    pub max_rounds: i64,
    /// Model tokens, in and out, one onboarding may spend.
    pub max_tokens: u64,
    pub ghost: ghost::Settings,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            checkouts: PathBuf::from("checkouts"),
            limits: lease::Limits::default(),
            max_rounds: 5,
            max_tokens: 2_000_000,
            ghost: ghost::Settings::default(),
        }
    }
}

pub use ghost::Planner;

/// Begin onboarding a repo as `app`.
/// The app name a repo URL suggests: its last path segment, made lease-safe.
pub fn name_for(repo: &str) -> Option<String> {
    let last = repo.trim_end_matches('/').trim_end_matches(".git").rsplit(['/', ':']).next()?;
    let name: String =
        last.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let name = name.trim_matches('-');
    (!name.is_empty()).then(|| name.chars().take(32).collect::<String>().trim_end_matches('-').to_owned())
}

pub fn start(
    canon: &Canon,
    app: &str,
    repo: &str,
    branch: Option<&str>,
    hostname: Option<&str>,
) -> Result<canon::Draft> {
    let draft = canon.start_draft(app, repo, branch, hostname)?;
    canon.enqueue(JOB, Some(app), &serde_json::json!({}))?;
    Ok(draft)
}

pub struct Draft<B, P> {
    canon: Arc<Canon>,
    hush: Arc<Hush>,
    press: Press<B>,
    git: Git,
    planner: Option<P>,
    settings: Settings,
}

/// Where a round's plan came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Repo,
    Sound,
    Ghost,
}

impl Origin {
    fn as_str(self) -> &'static str {
        match self {
            Origin::Repo => "repo",
            Origin::Sound => "sound",
            Origin::Ghost => "ghost",
        }
    }

    fn source(self) -> Source {
        match self {
            Origin::Repo => Source::Repo,
            Origin::Sound => Source::Sound,
            Origin::Ghost => Source::Ghost,
        }
    }
}

enum Choice {
    Plan(Plan, Origin, Option<String>),
    /// Ghost tried and produced nothing usable; the round is spent.
    Spent(String),
    Stuck(String),
}

impl<B: Builder, P: Planner> Draft<B, P> {
    /// `planner` is unset when no AI provider has a key: Draft then gets as
    /// far as the repo and Sound can take it.
    pub fn new(
        canon: Arc<Canon>,
        hush: Arc<Hush>,
        press: Press<B>,
        git: Git,
        planner: Option<P>,
        settings: Settings,
    ) -> Self {
        Draft { canon, hush, press, git, planner, settings }
    }

    /// Run one draft job: plan and build until a release is on trial, or
    /// the onboarding is stuck.
    pub async fn run(&self, job: &Job) -> Result<String> {
        let app = job.app.as_deref().ok_or_else(|| Error::Io("a draft job names no app".into()))?;
        match self.onboard(app).await {
            // Anything that will not pass with time ends the onboarding,
            // saying why, rather than leaving it waiting on nothing.
            Err(e) if !e.is_transient() => self.stuck(app, &e.to_string()),
            other => other,
        }
    }

    async fn onboard(&self, app: &str) -> Result<String> {
        let Some(d) = self.canon.draft(app)? else { return Ok(format!("no onboarding for `{app}`")) };
        if matches!(d.stage, DraftStage::Trial | DraftStage::Live | DraftStage::Stuck) {
            return Ok(format!("`{app}` is {}; nothing to do", d.stage));
        }

        let branch = match &d.branch {
            Some(b) => b.clone(),
            None => self.git.default_branch(&d.repo).await?.unwrap_or_else(|| "main".into()),
        };
        let Some(commit) = self.git.head(&d.repo, &branch).await? else {
            return self.stuck(app, &format!("{} has no branch `{branch}`", d.repo));
        };
        let dir = self.settings.checkouts.join(app);
        self.git.checkout(&dir, &d.repo, &branch, &commit).await?;
        let survey = Survey::scan(&dir).map_err(|e| Error::Io(format!("scanning {}: {e}", dir.display())))?;
        let ask = Ask { app, branch: &branch, hostname: d.hostname.as_deref() };

        // A crash mid-build resumes the build, not the planning.
        let mut resume = d.stage == DraftStage::Build && self.canon.app(app).is_ok();
        // Why the last round produced nothing, when it did not get as far as a plan.
        let mut spent: Option<String> = None;
        loop {
            let (proposal, from) = if resume {
                resume = false;
                let Some(p) = self.canon.proposals(app, 1)?.into_iter().next() else { continue };
                let from = match p.source.as_str() {
                    "repo" => Origin::Repo,
                    "sound" => Origin::Sound,
                    _ => Origin::Ghost,
                };
                (p.id, from)
            } else {
                let round = self.canon.draft(app)?.map_or(0, |d| d.round);
                if round >= self.settings.max_rounds {
                    let last = match spent.take() {
                        Some(why) => why,
                        None => self.attempts(app)?.pop().map(|a| a.failure).unwrap_or_default(),
                    };
                    return self.stuck(app, &format!("out of repair rounds after {round}; the last: {last}"));
                }
                self.canon.set_draft_stage(app, DraftStage::Plan, Some(&format!("round {}", round + 1)))?;
                let (plan, from, model) = match self.choose(app, &survey, &ask, &dir).await? {
                    Choice::Plan(plan, from, model) => (plan, from, model),
                    Choice::Spent(why) => {
                        let round = self.canon.next_draft_round(app)?;
                        spent = Some(why.clone());
                        warn!(app, round, "no plan this round: {why}");
                        self.canon.record(
                            Some(app),
                            "draft.round",
                            &format!("round {round}: {why}"),
                            &serde_json::Value::Null,
                        )?;
                        continue;
                    }
                    Choice::Stuck(why) => return self.stuck(app, &why),
                };
                spent = None;
                let round = self.canon.next_draft_round(app)?;
                let id = self.record(app, round, from, model.as_deref(), &plan)?;
                let lease = match plan.check(&ask, &self.settings.limits) {
                    Ok(l) => l,
                    Err(why) => {
                        self.canon.set_proposal_verdict(id, &why)?;
                        if from == Origin::Repo {
                            return self.stuck(app, &format!("the repo's lease.toml does not pass: {why}"));
                        }
                        continue;
                    }
                };
                if let Err(e) = self.accept(app, &d.repo, &lease, &plan, from) {
                    let why = e.to_string();
                    self.canon.set_proposal_verdict(id, &why)?;
                    return self.stuck(app, &why);
                }
                info!(app, round, from = from.as_str(), "plan accepted; building");
                self.canon.set_draft_stage(app, DraftStage::Build, Some(&format!("round {round}: building")))?;
                (id, from)
            };

            match self.press.release(app, Some(&commit), true).await? {
                Built::Released(release) | Built::Already(release) => {
                    self.canon.set_draft_release(app, Some(release))?;
                    self.canon.set_draft_stage(app, DraftStage::Trial, Some(&format!("release {release} on trial")))?;
                    return Ok(format!("release {release} of `{app}` is on trial"));
                }
                Built::Failed(_, why) => {
                    self.canon.set_proposal_verdict(proposal, &why)?;
                    if from == Origin::Repo {
                        return self.stuck(app, &format!("the repo's own plan did not build: {why}"));
                    }
                }
            }
        }
    }

    /// The most deterministic plan not yet tried.
    async fn choose(&self, app: &str, survey: &Survey, ask: &Ask<'_>, dir: &std::path::Path) -> Result<Choice> {
        let skeleton = survey.skeleton(ask);
        let attempts = self.attempts(app)?;
        let tried =
            |source: &str| -> Result<bool> { Ok(self.canon.proposals(app, 100)?.iter().any(|p| p.source == source)) };
        if let Some(s) = skeleton.as_ref().filter(|s| s.source == SkeletonSource::Repo) {
            return Ok(if tried("repo")? {
                let last = attempts.last().map(|a| a.failure.as_str()).unwrap_or_default();
                Choice::Stuck(format!("the repo's lease.toml failed: {last}. Fix it and push, then onboard again"))
            } else {
                Choice::Plan(s.plan.clone(), Origin::Repo, None)
            });
        }
        // The most deterministic source goes first, guesses and all: the
        // gate, not a model, says whether they were right.
        if let Some(s) = &skeleton
            && !tried("sound")?
        {
            return Ok(Choice::Plan(s.plan.clone(), Origin::Sound, None));
        }
        let Some(planner) = &self.planner else {
            let why = match attempts.last() {
                Some(a) => format!("{}; no AI provider is set up to repair it", a.failure),
                None => "Sound found nothing to build from, and no AI provider is set up".into(),
            };
            return Ok(Choice::Stuck(format!(
                "{why}. Add a key (`henry secret set henry/META_API_KEY`) or a lease.toml to the repo"
            )));
        };
        let spent = self.canon.draft(app)?.map_or(0, |d| d.spent).max(0) as u64;
        let Some(budget) = self.settings.max_tokens.checked_sub(spent).filter(|b| *b > 0) else {
            return Ok(Choice::Stuck(format!("the AI spend cap of {} tokens is used up", self.settings.max_tokens)));
        };
        let ws = Workspace::new(dir.to_path_buf(), survey.files.clone());
        let brief =
            Brief { ask, survey, skeleton: skeleton.as_ref(), attempts: &attempts, limits: &self.settings.limits };
        let out = planner.propose(&brief, &ws, budget, &self.settings.ghost).await;
        self.canon.add_draft_spend(app, out.usage.total() as i64)?;
        Ok(match out.plan {
            Ok(plan) => Choice::Plan(plan, Origin::Ghost, Some(planner.name())),
            Err(e) if e.is_transient() => return Err(Error::Transient(e.to_string())),
            Err(ghost::Error::Provider(why)) => Choice::Stuck(format!("the AI provider refused: {why}")),
            Err(e) => Choice::Spent(e.to_string()),
        })
    }

    /// Earlier plans that failed, oldest first, the last three.
    fn attempts(&self, app: &str) -> Result<Vec<Attempt>> {
        let mut out: Vec<Attempt> = self
            .canon
            .proposals(app, 3)?
            .into_iter()
            .filter_map(|p| {
                let failure = p.verdict.filter(|v| v != ACCEPTED)?;
                let files = p
                    .files
                    .as_object()
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_owned())).collect())
                    .unwrap_or_default();
                let generate = serde_json::from_value(p.generate).unwrap_or_default();
                Some(Attempt { plan: Plan { lease: p.lease, files, generate, notes: p.notes }, failure })
            })
            .collect();
        out.reverse();
        Ok(out)
    }

    fn record(&self, app: &str, round: i64, from: Origin, model: Option<&str>, plan: &Plan) -> Result<i64> {
        let files: serde_json::Map<String, serde_json::Value> =
            plan.files.iter().map(|(p, c)| (p.clone(), c.clone().into())).collect();
        let generate = serde_json::to_value(&plan.generate).unwrap_or_default();
        Ok(self.canon.add_proposal(&NewProposal {
            app,
            round,
            source: from.as_str(),
            model,
            lease: &plan.lease,
            files: &serde_json::Value::Object(files),
            generate: &generate,
            notes: &plan.notes,
        })?)
    }

    /// Make the plan the app's: its lease, its files, its generated secrets.
    fn accept(&self, app: &str, repo: &str, lease: &lease::Lease, plan: &Plan, from: Origin) -> Result<()> {
        match self.canon.app(app) {
            Ok(_) => {
                self.canon.update_lease(lease, &plan.lease, from.source())?;
            }
            Err(canon::Error::NoSuchApp(_)) => {
                self.canon.insert_app(lease, &plan.lease, repo, from.source())?;
            }
            Err(e) => return Err(e.into()),
        }
        self.canon.set_overlays(app, &plan.files)?;
        generate(&self.hush, app, &plan.generate)
    }

    fn stuck(&self, app: &str, why: &str) -> Result<String> {
        stuck(&self.canon, app, why)?;
        Ok(format!("`{app}` is stuck: {why}"))
    }
}

/// The verdict on a plan that went live.
pub const ACCEPTED: &str = "accepted";

/// End an onboarding whose job could not be finished, e.g. after its
/// retries ran out.
pub fn give_up(canon: &Canon, app: &str, why: &str) -> Result<()> {
    match canon.draft(app)? {
        Some(d) if !matches!(d.stage, DraftStage::Live | DraftStage::Stuck) => stuck(canon, app, why),
        _ => Ok(()),
    }
}

fn stuck(canon: &Canon, app: &str, why: &str) -> Result<()> {
    warn!(app, "onboarding stuck: {why}");
    canon.set_draft_stage(app, DraftStage::Stuck, Some(why))?;
    if let Ok(a) = canon.app(app)
        && a.live_release.is_none()
    {
        canon.set_status(app, Status::Stuck, Some(why))?;
    }
    Ok(())
}

/// Fill in the secrets a plan asks Henry to make. A random one, once made,
/// is kept: a database initialised with a password must keep getting it.
/// A templated one is made again every time, from its parts.
fn generate(hush: &Hush, app: &str, wanted: &[Generate]) -> Result<()> {
    let mut made: Vec<(&str, Secret)> = Vec::new();
    for g in wanted.iter().filter(|g| g.template.is_none()) {
        let value = match hush.get(app, &g.name)? {
            Some(v) => v,
            None => {
                let mut bytes = [0u8; 32];
                getrandom::fill(&mut bytes).map_err(|e| hush::Error::Random(e.to_string()))?;
                let v = Secret::new(hex::encode(bytes));
                hush.set(app, &g.name, &v)?;
                v
            }
        };
        made.push((&g.name, value));
    }
    // Templates may build on each other: fill each once its parts exist.
    let mut pending: Vec<&Generate> = wanted.iter().filter(|g| g.template.is_some()).collect();
    while !pending.is_empty() {
        let (ready, waiting): (Vec<&Generate>, Vec<&Generate>) = pending.into_iter().partition(|g| {
            sound::placeholders(g.template.as_deref().unwrap_or_default())
                .iter()
                .all(|n| made.iter().any(|(m, _)| m == n))
        });
        if ready.is_empty() {
            let names: Vec<&str> = waiting.iter().map(|g| g.name.as_str()).collect();
            return Err(Error::Io(format!("cannot fill the templates for {}", names.join(", "))));
        }
        for g in ready {
            let mut value = g.template.clone().unwrap_or_default();
            for (name, part) in &made {
                value = value.replace(&format!("{{{name}}}"), part.expose());
            }
            let value = Secret::new(value);
            hush.set(app, &g.name, &value)?;
            made.push((&g.name, value));
        }
        pending = waiting;
    }
    Ok(())
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub live: Vec<String>,
    pub retried: Vec<String>,
    pub stuck: Vec<String>,
}

/// Watch every onboarding on trial: done when its release goes live,
/// another round when it fails, stuck when the budget is spent. Cheap and
/// Canon-only, so it runs on every tick.
pub fn pass(canon: &Canon, max_rounds: i64) -> Result<Report> {
    let mut report = Report::default();
    for d in canon.drafts()?.into_iter().filter(|d| d.stage == DraftStage::Trial) {
        let Some(id) = d.release else { continue };
        let Ok(app) = canon.app(&d.app) else { continue };
        let release = canon.release(id)?;
        let latest = canon.proposals(&d.app, 1)?.into_iter().next();
        match release.state {
            ReleaseState::Live => {
                if let Some(p) = &latest {
                    canon.set_proposal_verdict(p.id, ACCEPTED)?;
                }
                canon.set_draft_stage(&d.app, DraftStage::Live, Some(&format!("release {id} is live")))?;
                info!(app = %d.app, release = id, "onboarded");
                report.live.push(d.app);
            }
            ReleaseState::Failed => {
                let why = release.reason.unwrap_or_else(|| "the release failed".into());
                if let Some(p) = &latest {
                    canon.set_proposal_verdict(p.id, &why)?;
                }
                if latest.as_ref().is_some_and(|p| p.source == "repo") {
                    stuck(canon, &d.app, &format!("the repo's own plan failed: {why}"))?;
                    report.stuck.push(d.app);
                } else if d.round >= max_rounds {
                    stuck(canon, &d.app, &format!("out of repair rounds after {}; the last: {why}", d.round))?;
                    report.stuck.push(d.app);
                } else {
                    canon.set_draft_stage(&d.app, DraftStage::Plan, Some(&format!("release {id} failed; revising")))?;
                    canon.enqueue(JOB, Some(&d.app), &serde_json::json!({}))?;
                    report.retried.push(d.app);
                }
            }
            // A push during onboarding replaced the trial: follow the newest.
            ReleaseState::Superseded => {
                if let Some(newer) = canon.releases(&d.app, 1)?.into_iter().next().filter(|r| r.id > id) {
                    canon.set_draft_release(&d.app, Some(newer.id))?;
                }
            }
            _ => {
                let detail = if app.status == Status::NeedsYou {
                    app.status_detail.unwrap_or_else(|| "waiting for you".into())
                } else {
                    format!("release {id} on trial")
                };
                canon.set_draft_stage(&d.app, DraftStage::Trial, Some(&detail))?;
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests;
