//! The `henry` command line.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use canon::{Canon, Desired, ReleaseState, Source, serde_json};
use clap::{Parser, Subcommand};
use hold::Runtime;
use hush::{HOST, Hush, Secret};
use lease::{Lease, Refusal};

use crate::home::{Config, Home};
use crate::host::Host;
use crate::time::utc;
use crate::{daemon, edge, launchd};

#[derive(Parser)]
#[command(name = "henry", version, about = "Rules the home: hosts your GitHub repos on this Mac")]
pub struct Args {
    /// Log at debug level (or set HENRY_LOG).
    #[arg(short, long, global = true)]
    pub verbose: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon. After `henry install`, launchd does this for you.
    Run,
    /// Check a lease.toml without registering anything.
    Check { lease: PathBuf },
    /// Onboard a GitHub repo: Henry surveys it, plans how to run it (with
    /// AI when a provider key is set), builds it and proves it. Follow it
    /// with `henry show <app>`.
    Onboard {
        /// The repo, e.g. https://github.com/you/draw.
        repo: String,
        /// The app's name. Default: the repo's name.
        #[arg(long)]
        name: Option<String>,
        /// The public hostname, e.g. draw.example.com. Omit to keep it private.
        #[arg(long)]
        hostname: Option<String>,
        /// The branch to deploy. Default: the repo's default branch.
        #[arg(long)]
        branch: Option<String>,
    },
    /// Every plan onboarding weighed for an app, newest first, in full.
    Plans {
        app: String,
        #[arg(short = 'n', long, default_value_t = 3)]
        limit: usize,
    },
    /// Register an app from a lease you wrote.
    Add {
        lease: PathBuf,
        /// The GitHub repo the app is built from.
        #[arg(long)]
        repo: String,
    },
    /// List every app and its state.
    Apps,
    /// One app in detail: routes, secrets, releases and recent events.
    Show { app: String },
    /// What the container engine is running for each app.
    Ps,
    /// Build and try the app's branch now, or a given commit. The daemon does
    /// the work; follow it with `henry show`.
    Deploy {
        app: String,
        /// A commit to release instead of the branch head.
        #[arg(long)]
        commit: Option<String>,
    },
    /// Try a release made of images that already exist. Plumb runs it beside
    /// the live release; only Proof can make it live.
    Trial {
        app: String,
        /// A prebuilt image for a service that would otherwise need a build,
        /// as SERVICE=IMAGE. Repeat for several services.
        #[arg(long = "image", value_name = "SERVICE=IMAGE")]
        images: Vec<String>,
    },
    /// Recent events, for one app or for everything.
    Events {
        app: Option<String>,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
    },
    /// Ask Henry to keep an app running.
    Start { app: String },
    /// Ask Henry to stop an app and keep it stopped.
    Stop { app: String },
    /// Forget an app, its releases and its secrets.
    Remove {
        app: String,
        /// Confirm. Removal cannot be undone.
        #[arg(long)]
        yes: bool,
    },
    /// Set, list or remove secrets.
    #[command(subcommand)]
    Secret(SecretCmd),
    /// Install Henry, Caddy and (with [cloudflare]) cloudflared as LaunchAgents,
    /// so they start at login and stay up.
    Install {
        /// Print the agents instead of installing them.
        #[arg(long)]
        print: bool,
    },
    /// Remove the LaunchAgents. Apps keep running until the Mac restarts.
    Uninstall,
    /// The public routes Henry wants, from Canon and config.
    Routes,
    /// Tell the running daemon to reread henry.toml and Henry's own secrets.
    Reload,
    /// Show where Henry keeps its files.
    Where,
}

#[derive(Subcommand)]
enum SecretCmd {
    /// Set `<app>/<NAME>`, or `henry/<NAME>` for Henry's own keys. The value
    /// is read from stdin, never from arguments, so it stays out of your
    /// shell history and the process list.
    Set { secret: String },
    /// List the secret names an app (or `henry`) holds. Values are never shown.
    List { scope: String },
    /// Remove `<app>/<NAME>`.
    Rm { secret: String },
}

impl Args {
    pub fn is_daemon(&self) -> bool {
        matches!(self.cmd, Cmd::Run)
    }
}

pub fn run(args: Args) -> Result<ExitCode> {
    let home = Home::locate()?;
    match args.cmd {
        Cmd::Run => daemon::run(&home)?,
        Cmd::Check { lease } => return check(&home, &lease),
        Cmd::Onboard { repo, name, hostname, branch } => {
            onboard(&home, &repo, name.as_deref(), hostname.as_deref(), branch.as_deref())?
        }
        Cmd::Plans { app, limit } => plans(&home, &app, limit)?,
        Cmd::Add { lease, repo } => add(&home, &lease, &repo)?,
        Cmd::Apps => apps(&home)?,
        Cmd::Show { app } => show(&home, &app)?,
        Cmd::Events { app, limit } => events(&home, app.as_deref(), limit)?,
        Cmd::Ps => ps(&home)?,
        Cmd::Deploy { app, commit } => {
            let canon = canon(&home)?;
            canon.app(&app)?;
            let job = press::request(&canon, &app, commit.as_deref(), true)?;
            println!("deploy of {app} queued (job {}); the daemon builds it", job.id);
        }
        Cmd::Trial { app, images } => trial(&home, &app, &images)?,
        Cmd::Start { app } => canon(&home)?.set_desired(&app, Desired::Running)?,
        Cmd::Stop { app } => canon(&home)?.set_desired(&app, Desired::Stopped)?,
        Cmd::Remove { app, yes } => {
            if !yes {
                bail!("this forgets `{app}`, its releases and its secrets; run again with --yes to confirm");
            }
            canon(&home)?.remove_app(&app)?;
            println!("removed {app}");
        }
        Cmd::Secret(cmd) => secret(&home, cmd)?,
        Cmd::Install { print } => {
            let binary = std::env::current_exe()?.canonicalize()?;
            let config = Config::load(&home.config())?;
            let mut agents = vec![launchd::caddy(&home, &config), launchd::henry(&binary, &home)];
            if let Some(cf) = &config.cloudflare {
                agents.insert(1, launchd::cloudflared(&home, &cf.cloudflared));
            }
            if print {
                for a in &agents {
                    println!("<!-- {} -->\n{}", a.label, a.plist());
                }
            } else {
                let canon = Arc::new(canon(&home)?);
                let hush = open_hush(&home, canon)?;
                edge::prepare(&home, &config, &hush)?;
                if config.cloudflare.is_some() && !home.tunnel_token().exists() {
                    bail!("set henry/{} before installing the tunnel", edge::TUNNEL_TOKEN);
                }
                for a in &agents {
                    let path = launchd::install(a)?;
                    println!("installed {}", path.display());
                }
            }
        }
        Cmd::Uninstall => {
            for label in [launchd::LABEL, launchd::CLOUDFLARED, launchd::CADDY] {
                launchd::uninstall(label)?;
            }
            println!("uninstalled Henry's LaunchAgents");
        }
        Cmd::Routes => {
            let config = Config::load(&home.config())?;
            let mut routes = edge::own_routes(&config)?;
            routes.extend(hinge::desired(&canon(&home)?)?);
            if routes.is_empty() {
                println!("no public routes: an app is routed once a release of it is live");
            }
            for r in routes {
                println!("https://{}{:<20} → 127.0.0.1:{:<6} {}", r.host, r.path, r.upstream, r.owner);
            }
        }
        Cmd::Reload => {
            let pid: i32 = std::fs::read_to_string(home.lock())
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .context("Henry does not seem to be running")?;
            // SAFETY: sending a signal to a pid read from Henry's own lock file.
            if unsafe { libc::kill(pid, libc::SIGHUP) } != 0 {
                bail!("could not signal Henry (pid {pid}); is it running?");
            }
            println!("asked Henry (pid {pid}) to reload");
        }
        Cmd::Where => {
            println!("home       {}", home.root.display());
            println!("canon      {}", home.canon().display());
            println!("config     {}", home.config().display());
            println!("checkouts  {}", home.checkouts().display());
            println!("logs       {}", home.logs().display());
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn canon(home: &Home) -> Result<Canon> {
    home.ensure()?;
    Canon::open(&home.canon()).context("opening Canon")
}

pub fn open_hush(home: &Home, canon: Arc<Canon>) -> Result<Hush> {
    Ok(Hush::open(canon, &key_store(home))?)
}

/// On the Mac the master key lives in the login Keychain.
#[cfg(target_os = "macos")]
fn key_store(_home: &Home) -> impl hush::KeyStore {
    hush::Keychain::default()
}

/// Elsewhere (Linux, CI) it lives in a file only you can read.
#[cfg(not(target_os = "macos"))]
fn key_store(home: &Home) -> impl hush::KeyStore {
    hush::FileKey { path: home.key_file() }
}

fn read_lease(home: &Home, path: &std::path::Path) -> Result<(Result<Lease, Refusal>, String)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let limits = Config::load(&home.config())?.limits()?;
    Ok((Lease::parse_with(&text, &limits), text))
}

fn print_refusal(r: &Refusal) {
    match r {
        Refusal::Syntax(e) => eprintln!("{e}"),
        Refusal::Policy(violations) => {
            eprintln!("refused, {} problem(s):", violations.len());
            for v in violations {
                eprintln!("  {:<36} {}  [{}]", v.path, v.message, v.code);
            }
        }
    }
}

fn check(home: &Home, path: &std::path::Path) -> Result<ExitCode> {
    let (parsed, _) = read_lease(home, path)?;
    match parsed {
        Ok(lease) => {
            let names: Vec<&str> = lease.start_order().iter().map(|s| s.name.as_str()).collect();
            println!("ok: {} — {} service(s), started in order {}", lease.name, names.len(), names.join(" → "));
            for host in lease.hostnames() {
                println!("    public at https://{host}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(r) => {
            print_refusal(&r);
            Ok(ExitCode::FAILURE)
        }
    }
}

fn add(home: &Home, path: &std::path::Path, repo: &str) -> Result<()> {
    let (parsed, text) = read_lease(home, path)?;
    let lease = match parsed {
        Ok(l) => l,
        Err(r) => {
            print_refusal(&r);
            bail!("{} was not registered", path.display());
        }
    };
    let canon = Arc::new(canon(home)?);
    canon.insert_app(&lease, &text, repo, Source::Operator)?;
    println!("registered {}", lease.name);

    let hush = open_hush(home, canon)?;
    let wanted: Vec<String> = lease.services.iter().flat_map(|s| s.secrets.iter().cloned()).collect();
    let missing = hush.provision(&lease.name, &wanted)?.missing;
    if !missing.is_empty() {
        println!("it still needs {} secret(s):", missing.len());
        for name in missing {
            println!("    henry secret set {}/{name}", lease.name);
        }
    }
    Ok(())
}

fn apps(home: &Home) -> Result<()> {
    let canon = canon(home)?;
    let apps = canon.apps()?;
    // Onboardings that have not yet produced an app.
    let drafts: Vec<_> = canon.drafts()?.into_iter().filter(|d| !apps.iter().any(|a| a.name == d.app)).collect();
    if apps.is_empty() && drafts.is_empty() {
        println!("no apps yet; onboard one with `henry onboard <repo url>`");
        return Ok(());
    }
    println!("{:<20} {:<11} {:<8} {:<8} UPDATED", "APP", "STATUS", "WANTED", "RELEASE");
    for a in apps {
        let release = a.live_release.map_or_else(|| "-".to_owned(), |r| r.to_string());
        println!("{:<20} {:<11} {:<8} {:<8} {}", a.name, a.status, a.desired, release, utc(a.updated_at));
    }
    for d in drafts {
        let status = if d.stage == canon::DraftStage::Stuck { "stuck" } else { "onboarding" };
        println!("{:<20} {:<11} {:<8} {:<8} {}", d.app, status, "running", "-", utc(d.updated_at));
    }
    Ok(())
}

/// The app name a repo URL suggests: its last path segment, made lease-safe.
fn name_from_repo(repo: &str) -> Option<String> {
    let last = repo.trim_end_matches('/').trim_end_matches(".git").rsplit(['/', ':']).next()?;
    let name: String =
        last.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let name = name.trim_matches('-');
    (!name.is_empty()).then(|| name.chars().take(32).collect::<String>().trim_end_matches('-').to_owned())
}

fn onboard(home: &Home, repo: &str, name: Option<&str>, hostname: Option<&str>, branch: Option<&str>) -> Result<()> {
    let name = match name {
        Some(n) => n.to_owned(),
        None => name_from_repo(repo).context("cannot tell the app's name from the URL; pass --name")?,
    };
    let canon = canon(home)?;
    draft::start(&canon, &name, repo, branch, hostname)?;
    println!("onboarding {repo} as `{name}`; the daemon surveys, plans, builds and proves it");
    println!("follow it with `henry show {name}`");
    Ok(())
}

fn plans(home: &Home, app: &str, limit: usize) -> Result<()> {
    let canon = canon(home)?;
    let plans = canon.proposals(app, limit)?;
    if plans.is_empty() {
        println!("no plans for `{app}`");
    }
    for p in plans {
        let by = p.model.as_deref().map(|m| format!(" ({m})")).unwrap_or_default();
        println!("── plan {} · round {} · by {}{by} · {}", p.id, p.round, p.source, utc(p.created_at));
        match &p.verdict {
            Some(v) => println!("verdict: {}", v.lines().next().unwrap_or_default()),
            None => println!("verdict: being tried"),
        }
        if !p.notes.is_empty() {
            println!("notes: {}", p.notes);
        }
        println!("\n{}", p.lease.trim_end());
        if let Some(files) = p.files.as_object() {
            for (path, content) in files {
                println!("\n# {path}\n{}", content.as_str().unwrap_or_default().trim_end());
            }
        }
        if let Some(generate) = p.generate.as_array().filter(|g| !g.is_empty()) {
            let names: Vec<&str> = generate.iter().filter_map(|g| g["name"].as_str()).collect();
            println!("\ngenerated secrets: {}", names.join(", "));
        }
        println!();
    }
    Ok(())
}

fn show(home: &Home, name: &str) -> Result<()> {
    let canon = Arc::new(canon(home)?);
    let draft = canon.draft(name)?;
    let app = match canon.app(name) {
        Ok(app) => app,
        Err(canon::Error::NoSuchApp(_)) if draft.is_some() => {
            let d = draft.expect("checked above");
            println!("{}\n  repo      {}", d.app, d.repo);
            print_draft(&d);
            println!("  events");
            for e in canon.events(Some(name), 10)? {
                println!("    {}  {}", utc(e.at), e.message);
            }
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    let lease = app.lease()?;
    println!("{}", app.name);
    println!("  repo      {} ({})", app.repo, lease.branch);
    match &app.status_detail {
        Some(d) => println!("  status    {} — {d}", app.status),
        None => println!("  status    {}", app.status),
    }
    println!("  wanted    {}", app.desired);
    println!("  lease     written by {}", app.source);
    if let Some(d) = draft.filter(|d| d.stage != canon::DraftStage::Live) {
        print_draft(&d);
    }

    for r in canon.routes()?.into_iter().filter(|r| r.app == app.name) {
        println!("  route     https://{}{} → {}", r.hostname, r.path, r.service);
    }

    let hush = open_hush(home, canon.clone())?;
    let wanted: Vec<String> = lease.services.iter().flat_map(|s| s.secrets.iter().cloned()).collect();
    let p = hush.provision(&app.name, &wanted)?;
    if !wanted.is_empty() {
        let set: Vec<&str> = p.env.iter().map(|(n, _)| n.as_str()).collect();
        println!("  secrets   set: {}", if set.is_empty() { "none".into() } else { set.join(", ") });
        if !p.missing.is_empty() {
            println!("            missing: {}", p.missing.join(", "));
        }
    }

    let releases = canon.releases(&app.name, 5)?;
    if !releases.is_empty() {
        println!("  releases");
        for r in releases {
            let sha = &r.commit_sha[..r.commit_sha.len().min(10)];
            // The first line says what happened; the rest is a build log tail.
            let reason =
                r.reason.as_deref().and_then(|x| x.lines().next()).map(|x| format!(" — {x}")).unwrap_or_default();
            println!("    {:>5}  {sha:<10}  {:<10} {}{reason}", r.id, r.state, utc(r.created_at));
        }
    }
    println!("  events");
    for e in canon.events(Some(&app.name), 10)? {
        println!("    {}  {}", utc(e.at), e.message);
    }
    Ok(())
}

fn print_draft(d: &canon::Draft) {
    let detail =
        d.detail.as_deref().map(|x| format!(" — {}", x.lines().next().unwrap_or_default())).unwrap_or_default();
    println!("  onboard   {}{detail}", d.stage);
    println!("            round {}, {} AI tokens spent; `henry plans {}` shows each plan", d.round, d.spent, d.app);
}

fn ps(home: &Home) -> Result<()> {
    let config = Config::load(&home.config())?;
    let canon = canon(home)?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let containers = rt.block_on(async {
        let engine = Host::from_config(&config.engine)?.connect().await?;
        anyhow::Ok(engine.list().await?)
    })?;
    if containers.is_empty() {
        println!("no containers yet");
        return Ok(());
    }
    println!("{:<28} {:<10} {:<10} RELEASE", "CONTAINER", "STATE", "ROLE");
    for (app, list) in plumb::by_app(&containers) {
        let live = canon.app(&app).ok().and_then(|a| a.live_release).map(|r| r.to_string());
        for c in list {
            let state = format!("{:?}", c.state).to_lowercase();
            let release = c.label(hold::label::RELEASE);
            let role = match release {
                None => "stateful",
                Some(r) if Some(r) == live.as_deref() => "live",
                Some(_) => "candidate",
            };
            println!("{:<28} {:<10} {:<10} {}", c.name, state, role, release.unwrap_or("-"));
        }
    }
    Ok(())
}

fn trial(home: &Home, name: &str, overrides: &[String]) -> Result<()> {
    let canon = canon(home)?;
    let app = canon.app(name)?;
    let lease = app.lease()?;
    let mut images = serde_json::Map::new();
    for o in overrides {
        let (service, image) = o.split_once('=').context("write --image as SERVICE=IMAGE")?;
        if lease.service(service).is_none() {
            bail!("`{name}` has no service `{service}`");
        }
        images.insert(service.to_owned(), image.into());
    }
    let mut unbuilt = Vec::new();
    for s in &lease.services {
        if images.contains_key(&s.name) {
            continue;
        }
        match &s.build {
            lease::Build::Image(image) => {
                images.insert(s.name.clone(), image.clone().into());
            }
            _ => unbuilt.push(s.name.as_str()),
        }
    }
    if !unbuilt.is_empty() {
        bail!(
            "{} need(s) a build. Pass a prebuilt image with --image {}=<ref>, or `henry deploy` to build from source",
            unbuilt.join(", "),
            unbuilt[0]
        );
    }
    let release = canon.create_release(name, "images", &app.lease)?;
    canon.set_release_images(release.id, &serde_json::Value::Object(images))?;
    canon.set_release_state(release.id, ReleaseState::Trial, Some("tried by hand from existing images"))?;
    println!("release {} is on trial; Plumb starts it and Proof decides whether it goes live", release.id);
    Ok(())
}

fn events(home: &Home, app: Option<&str>, limit: usize) -> Result<()> {
    for e in canon(home)?.events(app, limit)?.into_iter().rev() {
        let who = e.app.as_deref().unwrap_or("henry");
        println!("{}  {:<16} {}", utc(e.at), who, e.message);
    }
    Ok(())
}

fn secret(home: &Home, cmd: SecretCmd) -> Result<()> {
    let canon = Arc::new(canon(home)?);
    // `henry` names Henry's own scope; anything else must be a registered app.
    let scope = |s: &str| -> Result<String> {
        if s == HOST_NAME {
            return Ok(HOST.to_owned());
        }
        canon.app(s)?;
        Ok(s.to_owned())
    };
    let split = |s: &str| -> Result<(String, String)> {
        match s.split_once('/') {
            Some((a, n)) if !n.is_empty() => Ok((scope(a)?, n.to_owned())),
            _ => bail!("name a secret as <app>/<NAME>, or henry/<NAME> for Henry's own"),
        }
    };
    match cmd {
        SecretCmd::Set { secret } => {
            let (scope, name) = split(&secret)?;
            let hush = open_hush(home, canon.clone())?;
            let value = read_value(&scope, &name)?;
            hush.set(&scope, &name, &value)?;
            println!("set {}", label(&scope, &name));
        }
        SecretCmd::List { scope: s } => {
            for name in canon.sealed_names(&scope(&s)?)? {
                println!("{name}");
            }
        }
        SecretCmd::Rm { secret } => {
            let (scope, name) = split(&secret)?;
            let hush = open_hush(home, canon.clone())?;
            if !hush.remove(&scope, &name)? {
                bail!("{} is not set", label(&scope, &name));
            }
            println!("removed {}", label(&scope, &name));
        }
    }
    Ok(())
}

/// Read a value from stdin. At a terminal, echo is off while you type.
fn read_value(scope: &str, name: &str) -> Result<Secret> {
    let mut value = String::new();
    if std::io::stdin().is_terminal() {
        eprint!("value for {} (hidden): ", label(scope, name));
        std::io::stderr().flush()?;
        let _ = Command::new("stty").arg("-echo").status();
        let read = std::io::stdin().read_line(&mut value);
        let _ = Command::new("stty").arg("echo").status();
        eprintln!();
        read?;
    } else {
        std::io::stdin().read_to_string(&mut value)?;
    }
    let value = value.strip_suffix('\n').unwrap_or(&value);
    let value = value.strip_suffix('\r').unwrap_or(value);
    if value.is_empty() {
        bail!("empty value; nothing was set");
    }
    Ok(Secret::new(value))
}

/// The scope name people type for Henry's own secrets.
const HOST_NAME: &str = "henry";

fn label(scope: &str, name: &str) -> String {
    if scope.is_empty() { format!("{HOST_NAME}/{name}") } else { format!("{scope}/{name}") }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::name_from_repo;

    #[test]
    fn a_repo_url_suggests_a_name() {
        assert_eq!(name_from_repo("https://github.com/me/Draw_Board.git").as_deref(), Some("draw-board"));
        assert_eq!(name_from_repo("git@github.com:me/site/").as_deref(), Some("site"));
    }

    #[test]
    fn the_command_line_is_well_formed() {
        super::Args::command().debug_assert();
    }
}
