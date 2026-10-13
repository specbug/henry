//! What Glass shows, read from Canon. Names of secrets, never values.

use std::collections::BTreeSet;

use canon::serde_json::{Value, json};
use canon::{App, Canon, DraftStage, Event, Release, now_ms};

use crate::strip;

type Result<T> = std::result::Result<T, canon::Error>;

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or_default()
}

/// The secrets an app's lease asks for, and which of them are set.
pub fn secrets(canon: &Canon, app: &App) -> Result<(Vec<String>, Vec<String>)> {
    let wanted: BTreeSet<String> = match app.lease() {
        Ok(l) => l.services.iter().flat_map(|s| s.secrets.iter().cloned()).collect(),
        Err(_) => BTreeSet::new(),
    };
    let held: BTreeSet<String> = canon.sealed_names(&app.name)?.into_iter().collect();
    let (set, missing) = wanted.into_iter().partition(|n| held.contains(n));
    Ok((set, missing))
}

fn health(canon: &Canon, app: &App, now: i64) -> Result<(String, Option<f64>)> {
    let since = now - strip::HOURS as i64 * strip::HOUR_MS;
    let history = canon.status_history(&app.name, since)?;
    let marks = strip::marks(now, app.created_at, &history);
    Ok((strip::encode(&marks), strip::live_share(now, app.created_at, &history)))
}

fn release(r: &Release) -> Value {
    json!({
        "id": r.id,
        "commit": r.commit_sha,
        "short": short(&r.commit_sha),
        "state": r.state.as_str(),
        "summary": r.reason.as_deref().map(first_line),
        "reason": r.reason,
        "created_at": r.created_at,
        "finished_at": r.finished_at,
    })
}

fn draft(d: &canon::Draft) -> Value {
    json!({
        "stage": d.stage.as_str(),
        "detail": d.detail,
        "summary": d.detail.as_deref().map(first_line),
        "round": d.round,
        "spent": d.spent,
        "repo": d.repo,
        "branch": d.branch,
        "hostname": d.hostname,
        "release": d.release,
        "updated_at": d.updated_at,
    })
}

pub fn event(e: &Event) -> Value {
    json!({ "id": e.id, "at": e.at, "app": e.app, "kind": e.kind, "message": e.message })
}

/// One row of the list of apps.
fn row(canon: &Canon, app: &App, now: i64) -> Result<Value> {
    let (strip, share) = health(canon, app, now)?;
    let live = match app.live_release {
        Some(id) => canon.release(id).ok().map(|r| json!({ "id": r.id, "short": short(&r.commit_sha) })),
        None => None,
    };
    let (_, missing) = secrets(canon, app)?;
    let onboarding = canon.draft(&app.name)?.filter(|d| d.stage != DraftStage::Live).map(|d| draft(&d));
    Ok(json!({
        "name": app.name,
        "repo": app.repo,
        "status": app.status.as_str(),
        "detail": app.status_detail,
        "desired": app.desired.as_str(),
        "live": live,
        "missing": missing,
        "onboarding": onboarding,
        "strip": strip,
        "share": share,
        "updated_at": app.updated_at,
    }))
}

/// Every app, and every onboarding that has not made one yet.
pub fn apps(canon: &Canon) -> Result<Value> {
    let now = now_ms();
    let apps = canon.apps()?;
    let mut rows = Vec::with_capacity(apps.len());
    for a in &apps {
        rows.push(row(canon, a, now)?);
    }
    let drafts: Vec<Value> = canon
        .drafts()?
        .iter()
        .filter(|d| !apps.iter().any(|a| a.name == d.app))
        .map(|d| json!({ "name": d.app, "onboarding": draft(d) }))
        .collect();
    Ok(json!({ "now": now, "apps": rows, "drafts": drafts }))
}

/// One app in full. `None` when there is neither an app nor an onboarding
/// of that name.
pub fn app(canon: &Canon, name: &str, backups: bool) -> Result<Option<Value>> {
    let now = now_ms();
    let onboarding = canon.draft(name)?;
    let events: Vec<Value> = canon.events(Some(name), 40)?.iter().map(event).collect();
    let proposals: Vec<Value> = canon
        .proposals(name, 10)?
        .iter()
        .map(|p| {
            json!({
                "id": p.id,
                "round": p.round,
                "source": p.source,
                "model": p.model,
                "verdict": p.verdict,
                "summary": p.verdict.as_deref().map(first_line),
                "notes": p.notes,
                "lease": p.lease,
                "files": p.files,
                "generate": p.generate.as_array().map(|g| g.iter().filter_map(|x| x["name"].as_str()).collect::<Vec<_>>()),
                "created_at": p.created_at,
            })
        })
        .collect();
    let app = match canon.app(name) {
        Ok(a) => a,
        Err(canon::Error::NoSuchApp(_)) => {
            return Ok(onboarding.map(|d| {
                json!({ "now": now, "name": name, "app": null, "onboarding": draft(&d), "plans": proposals, "events": events })
            }));
        }
        Err(e) => return Err(e),
    };
    let mut summary = row(canon, &app, now)?;
    let (set, missing) = secrets(canon, &app)?;
    let lease = app.lease().ok();
    let services: Vec<Value> = lease
        .iter()
        .flat_map(|l| &l.services)
        .map(|s| {
            json!({
                "name": s.name,
                "public": s.public.as_ref().map(|p| format!("https://{}{}", p.hostname, if p.path == "/" { "" } else { &p.path })),
                "volumes": s.volumes.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
                "secrets": s.secrets,
            })
        })
        .collect();
    let wanted = canon.wanted_releases(name)?;
    let trial = match &wanted.candidate {
        Some(c) => Some(json!({ "release": release(c), "proof": canon.proof(c.id)? })),
        None => None,
    };
    let releases: Vec<Value> = canon.releases(name, 12)?.iter().map(release).collect();
    let mut volumes = Vec::new();
    for t in ark::targets(canon, name).unwrap_or_default() {
        let target = t.name();
        let last = canon.last_backup(name, &target)?;
        let drill = canon.last_drill(name, &target)?;
        let good_drill = canon.last_good_drill(name, &target)?;
        volumes.push(json!({
            "target": target,
            "kind": format!("{:?}", t.kind).to_lowercase(),
            "backup": last.map(|b| json!({ "at": b.at, "ok": b.ok, "bytes": b.bytes, "detail": b.detail })),
            "drill": drill.map(|d| json!({ "at": d.at, "ok": d.ok, "detail": d.detail })),
            "proven": good_drill.is_some(),
        }));
    }
    let model = lease.as_ref().and_then(|l| l.ai.as_ref()).map(|a| format!("{}/{}", a.provider.as_str(), a.model));
    let extra = json!({
        "now": now,
        "app": true,
        "branch": lease.as_ref().map(|l| l.branch.clone()),
        "auto_deploy": lease.as_ref().map(|l| l.auto_deploy),
        "source": app.source.as_str(),
        "model": model,
        "lease": app.lease,
        "services": services,
        "secrets": { "set": set, "missing": missing },
        "trial": trial,
        "releases": releases,
        "volumes": volumes,
        "backups": backups,
        "onboarding_record": onboarding.map(|d| draft(&d)),
        "plans": proposals,
        "events": events,
    });
    if let (Some(s), Some(e)) = (summary.as_object_mut(), extra.as_object()) {
        s.extend(e.clone());
    }
    Ok(Some(summary))
}

/// Henry's own upkeep: each task's last run, the tools it owns, and Bench.
pub fn house(canon: &Canon, enabled: &[mend::Task]) -> Result<Value> {
    let mut tasks = Vec::new();
    for t in mend::Task::ALL {
        let u = canon.upkeep(t.as_str())?;
        tasks.push(json!({
            "task": t.as_str(),
            "on": enabled.contains(&t),
            "every_hours": t.every().as_secs() / 3600,
            "last": u.map(|u| json!({ "at": u.last_at, "ok": u.ok, "detail": u.detail })),
        }));
    }
    let tools: Vec<Value> = canon
        .tools()?
        .iter()
        .map(|t| json!({ "name": t.name, "current": t.current, "previous": t.previous, "pending": t.pending, "since": t.updated_at }))
        .collect();
    let bench: Vec<Value> = canon
        .bench_latest()?
        .iter()
        .map(|b| json!({ "model": b.model, "passed": b.passed, "total": b.total, "tokens": b.tokens, "at": b.at }))
        .collect();
    let henry: Vec<Value> = canon.events(None, 200)?.iter().filter(|e| e.app.is_none()).take(30).map(event).collect();
    Ok(json!({ "now": now_ms(), "upkeep": tasks, "tools": tools, "bench": bench, "events": henry }))
}
