//! Mend's tasks, run by the job worker, and the gate a tool upgrade passes.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use canon::Canon;
use hush::{HOST, Hush};
use mend::Task;
use mend::tools::{Gate, Tools, Upgrade};
use tracing::{info, warn};

use crate::home::{Config, Home};

/// Run one Mend task and record how it went. The result is the summary.
pub async fn run(home: &Home, config: &Config, canon: Arc<Canon>, hush: Arc<Hush>, task: Task) -> Result<String> {
    let result = match task {
        Task::Disk => disk(home, config, &canon).await,
        Task::Refresh => refresh(home, config, canon.clone(), &hush).await,
        Task::Host => host().await,
        Task::Tools => tools(home, config, &canon, &hush).await,
        Task::Bench => bench(config, &canon, &hush).await.map(|(summary, _)| summary),
    };
    let (ok, detail) = match &result {
        Ok(s) => (true, s.clone()),
        Err(e) => (false, format!("{e:#}")),
    };
    canon.record_upkeep(task.as_str(), ok, &detail)?;
    result
}

async fn disk(home: &Home, config: &Config, canon: &Canon) -> Result<String> {
    let d = mend::disk::Disk {
        engine: crate::daemon::builder_program(config),
        logs: home.logs().join("builds"),
        checkouts: home.checkouts(),
        keep_releases: config.mend.keep_releases.max(1),
        log_age: Duration::from_secs(config.mend.log_days.max(1) * 24 * 3600),
    };
    Ok(d.prune(canon).await?.to_string())
}

async fn refresh(home: &Home, config: &Config, canon: Arc<Canon>, hush: &Hush) -> Result<String> {
    let press = crate::daemon::press(home, config, hush, canon.clone())?;
    let mut lines = Vec::new();
    let mut failures = 0;
    for app in canon.apps()? {
        if app.live_release.is_none() || app.desired == canon::Desired::Stopped {
            continue;
        }
        let line = match press.refresh(&app.name).await {
            Ok(press::Refresh::Released(id, changed)) => format!("release {id} on trial ({})", changed.join(", ")),
            Ok(press::Refresh::Failed(why)) => {
                failures += 1;
                canon.record(Some(&app.name), "mend.refresh_failed", &why, &canon::serde_json::Value::Null)?;
                format!("could not rebuild, live release untouched: {}", why.lines().next().unwrap_or_default())
            }
            Ok(other) => format!("{other:?}").to_lowercase(),
            Err(e) => {
                failures += 1;
                format!("failed: {e}")
            }
        };
        lines.push(format!("{}: {line}", app.name));
    }
    let summary = if lines.is_empty() { "no live apps".to_owned() } else { lines.join("; ") };
    if failures > 0 && failures == lines.len() { bail!(summary) } else { Ok(summary) }
}

async fn host() -> Result<String> {
    let checks = mend::host::checks().await;
    if checks.is_empty() {
        return Ok("not a Mac; nothing to check".into());
    }
    let drifted: Vec<String> = checks
        .iter()
        .filter(|c| c.ok == Some(false))
        .map(|c| format!("{}: want {}, found {}", c.name, c.want, c.got))
        .collect();
    if drifted.is_empty() {
        Ok(format!("{} settings as Henry needs them", checks.iter().filter(|c| c.ok == Some(true)).count()))
    } else {
        bail!(drifted.join("; "))
    }
}

fn tool_box(home: &Home, config: &Config, hush: &Hush) -> Result<Tools> {
    let token = hush.get(HOST, "GITHUB_TOKEN")?.map(|t| t.expose().to_owned());
    Ok(Tools::new(home.tools(), &config.mend.github_api, token)?)
}

async fn tools(home: &Home, config: &Config, canon: &Canon, hush: &Hush) -> Result<String> {
    let tools = tool_box(home, config, hush)?;
    tools.recover(canon)?;
    let managed = canon.tools()?;
    if managed.is_empty() {
        return Ok("no tools are managed by Henry; `henry tools adopt <name>`".into());
    }
    let gate = HenryGate { home: home.clone(), config: config.clone(), canon };
    let mut lines = Vec::new();
    for t in managed {
        let line = match tools.upgrade(canon, &t.name, &gate).await {
            Ok(u) => {
                if let Upgrade::Upgraded { .. } = &u {
                    info!(tool = %t.name, "{u}");
                }
                u.to_string()
            }
            Err(e) => format!("failed: {e}"),
        };
        lines.push(format!("{}: {line}", t.name));
    }
    Ok(lines.join("; "))
}

/// Adopt a tool: install its newest release and run it from now on.
pub async fn adopt(home: &Home, config: &Config, canon: &Canon, hush: &Hush, name: &str) -> Result<String> {
    Ok(tool_box(home, config, hush)?.adopt(canon, name).await?)
}

/// A tool upgrade is kept only if, after switching, the tool runs and what
/// depends on it still works.
pub struct HenryGate<'a> {
    pub home: Home,
    pub config: Config,
    pub canon: &'a Canon,
}

impl Gate for HenryGate<'_> {
    async fn check(&self, tool: &str) -> Result<(), String> {
        let bin = mend::tools::Tools::current(&self.home.tools(), tool).ok_or("not installed")?;
        let version_arg = if tool == "restic" { "version" } else { "--version" };
        let out = tokio::process::Command::new(&bin).arg(version_arg).output().await.map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!("`{tool} {version_arg}` exited with {}", out.status));
        }
        match tool {
            "caddy" => {
                restart(crate::launchd::CADDY).await?;
                self.routes_answer().await
            }
            "cloudflared" => {
                restart(crate::launchd::CLOUDFLARED).await?;
                tokio::time::sleep(Duration::from_secs(10)).await;
                running(crate::launchd::CLOUDFLARED).await
            }
            _ => Ok(()),
        }
    }
}

impl HenryGate<'_> {
    /// Every public route answers through Caddy, within a minute.
    async fn routes_answer(&self) -> Result<(), String> {
        let routes = hinge::desired(self.canon).map_err(|e| e.to_string())?;
        let listen = &self.config.hinge.listen;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            let mut failing = None;
            for r in &routes {
                let url = format!("http://{listen}{}", r.path);
                if let Err(e) = proof::probe::http(&url, Some(&r.host), Duration::from_secs(5)).await {
                    failing = Some(format!("{}{}: {e}", r.host, r.path));
                    break;
                }
            }
            match failing {
                None => return Ok(()),
                Some(why) if tokio::time::Instant::now() >= deadline => return Err(why),
                Some(_) => tokio::time::sleep(Duration::from_secs(3)).await,
            }
        }
    }
}

/// Restart a LaunchAgent so it runs the binary `current` now points at.
async fn restart(label: &str) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let target = format!("gui/{uid}/{label}");
    let out = tokio::process::Command::new("launchctl")
        .args(["kickstart", "-k", &target])
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if out.status.success() { Ok(()) } else { Err(format!("launchctl kickstart {target} failed")) }
}

async fn running(label: &str) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let out = tokio::process::Command::new("launchctl")
        .args(["print", &format!("gui/{uid}/{label}")])
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if String::from_utf8_lossy(&out.stdout).contains("state = running") {
        Ok(())
    } else {
        Err(format!("{label} is not running after the switch"))
    }
}

/// Run Bench on the default model and every candidate, record each, and
/// say when a candidate beats the default. Returns the summary and the
/// reports.
pub async fn bench(config: &Config, canon: &Canon, hush: &Hush) -> Result<(String, Vec<bench::Report>)> {
    let Some(default) = crate::ai::planner(config, hush)? else {
        return Ok(("no AI provider has a key; nothing to bench".into(), vec![]));
    };
    let mut models = vec![default];
    for c in &config.ai.candidates {
        let (provider, model) = crate::ai::parse(c)?;
        match hush.get(HOST, ghost::key_name(provider))? {
            Some(key) => {
                let base = config.ai.endpoints.get(provider.as_str()).map(String::as_str);
                models.push(ghost::Model::connect(provider, model, key, base)?);
            }
            None => warn!(candidate = %c, "skipped: henry/{} is not set", ghost::key_name(provider)),
        }
    }
    let mut reports = Vec::new();
    for m in &models {
        let report = bench::run(m, config.mend.bench_tokens).await;
        canon.record_bench(
            &report.model,
            report.passed() as i64,
            report.verdicts.len() as i64,
            report.tokens() as i64,
            &report.detail(),
        )?;
        reports.push(report);
    }
    let current = &reports[0];
    let best = reports.iter().max_by_key(|r| (r.passed(), std::cmp::Reverse(r.tokens()))).context("no reports")?;
    let mut summary: Vec<String> =
        reports.iter().map(|r| format!("{} {}/{}", r.model, r.passed(), r.verdicts.len())).collect();
    if best.passed() > current.passed() {
        let advice = format!(
            "{} onboards more of the eval repos than {} ({} vs {}); set [ai] default_model = \"{}\"",
            best.model,
            current.model,
            best.passed(),
            current.passed(),
            best.model
        );
        canon.record(None, "bench.recommend", &advice, &canon::serde_json::Value::Null)?;
        summary.push(advice);
    }
    Ok((summary.join("; "), reports))
}
