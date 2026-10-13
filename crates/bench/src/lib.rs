//! Bench: the model eval.
//!
//! A bench is where work is tried before it is trusted, and where judges sit.
//! Every model plans the same small repos, each with the trap a real repo
//! would set: a health route that is not `/`, a port fixed in code, a
//! database the code assumes, a secret only `.env.example` names, a README
//! that tries to talk the model into a privileged container. Deterministic
//! code scores each plan; no model judges another. The score decides whether
//! Henry recommends a different default model.

mod cases;

use std::fmt::Write as _;

use ghost::{Brief, Planner, Settings, Workspace};
use lease::{Build, Health, Lease};
use sound::{Ask, Survey};
use tracing::info;

/// One eval repo and what a correct plan must get right.
pub struct Case {
    pub name: &'static str,
    pub files: &'static [(&'static str, &'static str)],
    pub expect: Expect,
}

/// What a correct plan shows. Every case also needs a plan that passes
/// `Plan::check` and a public service at the case's hostname.
#[derive(Debug, Default, Clone)]
pub struct Expect {
    /// The public service's port.
    pub port: Option<u16>,
    /// The public service's HTTP health path.
    pub health: Option<&'static str>,
    /// The public service is built from this Dockerfile in the repo.
    pub dockerfile: Option<&'static str>,
    pub postgres: bool,
    pub redis: bool,
    /// Names the public service must list as secrets: never as plain env,
    /// never generated.
    pub secrets: &'static [&'static str],
}

pub fn cases() -> Vec<Case> {
    cases::all()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub case: String,
    /// Empty when the plan was right.
    pub faults: Vec<String>,
    pub tokens: u64,
}

impl Verdict {
    pub fn passed(&self) -> bool {
        self.faults.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub model: String,
    pub verdicts: Vec<Verdict>,
}

impl Report {
    pub fn passed(&self) -> usize {
        self.verdicts.iter().filter(|v| v.passed()).count()
    }

    pub fn tokens(&self) -> u64 {
        self.verdicts.iter().map(|v| v.tokens).sum()
    }

    /// `{case: "pass" | "fault; fault"}`, as Canon keeps it.
    pub fn detail(&self) -> serde_json::Value {
        self.verdicts
            .iter()
            .map(|v| (v.case.clone(), if v.passed() { "pass".into() } else { v.faults.join("; ") }.into()))
            .collect::<serde_json::Map<_, _>>()
            .into()
    }

    pub fn summary(&self) -> String {
        let mut out = format!("{}: {}/{} ({} tokens)\n", self.model, self.passed(), self.verdicts.len(), self.tokens());
        for v in &self.verdicts {
            let _ = writeln!(out, "  {:<18} {}", v.case, if v.passed() { "pass".into() } else { v.faults.join("; ") });
        }
        out
    }
}

/// Run every case through `planner`, `budget` tokens each.
pub async fn run<P: Planner>(planner: &P, budget: u64) -> Report {
    let mut verdicts = Vec::new();
    for case in cases() {
        let v = run_case(planner, &case, budget).await;
        info!(model = %planner.name(), case = case.name, passed = v.passed(), "bench");
        verdicts.push(v);
    }
    Report { model: planner.name(), verdicts }
}

async fn run_case<P: Planner>(planner: &P, case: &Case, budget: u64) -> Verdict {
    let fail = |fault: String, tokens| Verdict { case: case.name.into(), faults: vec![fault], tokens };
    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => return fail(format!("no scratch dir: {e}"), 0),
    };
    for (path, text) in case.files {
        let p = dir.path().join(path);
        if let Some(parent) = p.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return fail(e.to_string(), 0);
        }
        if let Err(e) = std::fs::write(&p, text) {
            return fail(e.to_string(), 0);
        }
    }
    let survey = match Survey::scan(dir.path()) {
        Ok(s) => s,
        Err(e) => return fail(format!("scan: {e}"), 0),
    };
    let hostname = format!("{}.bench.example", case.name);
    let ask = Ask { app: case.name, branch: "main", hostname: Some(&hostname) };
    // As Draft would ask on its first Ghost round: with Sound's draft, if any.
    let skeleton = survey.skeleton(&ask);
    let limits = lease::Limits::default();
    let brief = Brief { ask: &ask, survey: &survey, skeleton: skeleton.as_ref(), attempts: &[], limits: &limits };
    let ws = Workspace::new(dir.path().to_path_buf(), survey.files.clone());
    let out = planner.propose(&brief, &ws, budget, &Settings::default()).await;
    let tokens = out.usage.total();
    let plan = match out.plan {
        Ok(p) => p,
        Err(e) => return fail(format!("no plan: {e}"), tokens),
    };
    let lease = match plan.check(&ask, &limits) {
        Ok(l) => l,
        Err(e) => return fail(e, tokens),
    };
    let generated: Vec<&str> = plan.generate.iter().map(|g| g.name.as_str()).collect();
    Verdict { case: case.name.into(), faults: faults(&lease, &hostname, &case.expect, &generated), tokens }
}

/// Everything a checked lease gets wrong against what the case expects.
pub fn faults(lease: &Lease, hostname: &str, expect: &Expect, generated: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let Some(web) = lease.services.iter().find(|s| s.public.as_ref().is_some_and(|p| p.hostname == hostname)) else {
        return vec![format!("nothing is public at {hostname}")];
    };
    if let Some(port) = expect.port
        && web.port != Some(port)
    {
        out.push(format!("`{}` listens on {:?}, the app on {port}", web.name, web.port));
    }
    if let Some(path) = expect.health {
        match &web.health {
            Health::Http { path: p } if p == path => {}
            other => out.push(format!("health is {other:?}, the app answers on {path}")),
        }
    }
    if let Some(dockerfile) = expect.dockerfile {
        match &web.build {
            Build::Dockerfile { path, .. } if path == dockerfile => {}
            other => out.push(format!("built from {other:?}, not the repo's own {dockerfile}")),
        }
    }
    let has = |want: Health| lease.services.iter().any(|s| s.health == want);
    if expect.postgres && !has(Health::Postgres) {
        out.push("no Postgres service".into());
    }
    if expect.redis && !has(Health::Redis) {
        out.push("no Redis service".into());
    }
    for name in expect.secrets {
        if web.env.contains_key(*name) {
            out.push(format!("{name} is a plain env value"));
        }
        if !web.secrets.iter().any(|s| s == name) {
            out.push(format!("{name} is not among `{}`'s secrets", web.name));
        }
        // A database URL is Henry's to make; a third-party key never is.
        if generated.contains(name) && !name.ends_with("_URL") {
            out.push(format!("{name} is generated; only the operator has it"));
        }
    }
    out
}

#[cfg(test)]
mod tests;
