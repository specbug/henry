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
    /// Move an app over from hostd: seal its `.env` in Hush, copy hostd's
    /// volume, and onboard it. hostd keeps serving it until `henry cutover`.
    Adopt {
        /// The repo, e.g. https://github.com/you/odyssey.
        repo: String,
        /// The app's name. Default: the repo's name.
        #[arg(long)]
        name: Option<String>,
        /// The public hostname hostd serves it on, e.g. odyssey.example.com.
        #[arg(long)]
        hostname: Option<String>,
        /// The branch hostd deploys (its deploy.yml says).
        #[arg(long)]
        branch: Option<String>,
        /// The app's .env, as hostd ran it. Values are sealed, never shown.
        #[arg(long)]
        env: Option<PathBuf>,
        /// hostd's volume to copy, from deploy.yml's `backup.volume`.
        #[arg(long)]
        volume: Option<String>,
    },
    /// Send an adopted app's public traffic to Henry: a final copy of the
    /// data, then its DNS record. hostd must have let the app go.
    Cutover {
        app: String,
        /// Put the DNS record back as it was, for hostd.
        #[arg(long)]
        back: bool,
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
    /// Back up an app now, or `henry` for Henry's own state.
    Backup { app: String },
    /// An app's backups (or `henry`'s), and whether each has been restored
    /// in a drill.
    Backups { app: String },
    /// Restore `<app>/<service>/<volume>` over its live volume from its newest
    /// snapshot. Stop the app first. `henry restore henry` restores Henry's
    /// own state into this home, e.g. on a new Mac; the daemon must be stopped.
    Restore {
        target: String,
        /// A snapshot id instead of the newest.
        #[arg(long)]
        snapshot: Option<String>,
        /// Confirm. A restore replaces what is there.
        #[arg(long)]
        yes: bool,
    },
    /// Upkeep: each task's last run, or `henry upkeep run <task>` to run one
    /// now (disk, refresh, host, tools, bench).
    Upkeep {
        #[command(subcommand)]
        cmd: Option<UpkeepCmd>,
    },
    /// Check the Mac's settings an unattended host depends on, now.
    Host,
    /// The binaries Henry owns and upgrades, or adopt one:
    /// `henry tools adopt caddy` (or cloudflared, restic, railpack).
    Tools {
        #[command(subcommand)]
        cmd: Option<ToolsCmd>,
    },
    /// The model eval's latest scores, or `--run` to score now. Scoring
    /// spends model tokens.
    Bench {
        #[arg(long)]
        run: bool,
    },
    /// Export or import the master key that seals every secret. Keep a copy
    /// with your restic password: restoring Henry on a new Mac needs both.
    #[command(subcommand)]
    Key(KeyCmd),
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
    /// Open Glass, the web UI, signed in, in a browser on this Mac. From
    /// elsewhere, Glass is at `[glass] hostname` behind Cloudflare Access.
    Open {
        /// Print the sign-in link instead of opening it.
        #[arg(long)]
        print: bool,
    },
    /// Show where Henry keeps its files.
    Where,
}

#[derive(Subcommand)]
enum UpkeepCmd {
    /// Queue a task now.
    Run { task: String },
}

#[derive(Subcommand)]
enum ToolsCmd {
    /// Install a tool's newest release and run it from now on. Run
    /// `henry install` afterwards so Caddy's and cloudflared's agents use it.
    Adopt { name: String },
}

#[derive(Subcommand)]
enum KeyCmd {
    /// Print the key, as hex, to stdout.
    Export,
    /// Read a key from stdin and store it. Refuses if this Mac has one.
    Import,
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
        Cmd::Adopt { repo, name, hostname, branch, env, volume } => {
            let name = match name {
                Some(n) => n,
                None => draft::name_for(&repo).context("cannot tell the app's name from the URL; pass --name")?,
            };
            let config = Config::load(&home.config())?;
            let canon = Arc::new(canon(&home)?);
            let hush = open_hush(&home, canon.clone())?;
            let a = crate::adopt::Adopt {
                repo: &repo,
                name: &name,
                hostname: hostname.as_deref(),
                branch: branch.as_deref(),
                env: env.as_deref(),
                volume: volume.as_deref(),
            };
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            for line in rt.block_on(crate::adopt::adopt(&config, &canon, &hush, &a))? {
                println!("{line}");
            }
        }
        Cmd::Cutover { app, back } => {
            let config = Config::load(&home.config())?;
            let canon = Arc::new(canon(&home)?);
            let hush = open_hush(&home, canon.clone())?;
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            let said = if back {
                rt.block_on(crate::adopt::back(&config, &canon, &hush, &app))?
            } else {
                rt.block_on(crate::adopt::cutover(&config, canon.clone(), &hush, &app))?
            };
            for line in said {
                println!("{line}");
            }
        }
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
        Cmd::Backup { app } => {
            let canon = canon(&home)?;
            if app != ark::HENRY {
                canon.app(&app)?;
            }
            let job = canon.enqueue(ark::JOB, Some(&app), &serde_json::json!({ "force": true }))?;
            println!("backup of {app} queued (job {}); `henry backups {app}` shows how it went", job.id);
        }
        Cmd::Backups { app } => backups(&home, &app)?,
        Cmd::Restore { target, snapshot, yes } => restore(&home, &target, snapshot.as_deref(), yes)?,
        Cmd::Key(cmd) => key(&home, cmd)?,
        Cmd::Upkeep { cmd } => upkeep(&home, cmd)?,
        Cmd::Host => {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            let checks = rt.block_on(mend::host::checks());
            if checks.is_empty() {
                println!("not a Mac: nothing to check");
            }
            for c in checks {
                let mark = match c.ok {
                    Some(true) => "ok   ",
                    Some(false) => "DRIFT",
                    None => "?    ",
                };
                println!("{mark}  {:<26} want {:<28} found {}", c.name, c.want, c.got);
            }
        }
        Cmd::Tools { cmd } => tools(&home, cmd)?,
        Cmd::Bench { run } => bench(&home, run)?,
        Cmd::Install { print } => {
            let binary = std::env::current_exe()?.canonicalize()?;
            let config = Config::load(&home.config())?;
            let mut agents = vec![launchd::caddy(&home, &config), launchd::henry(&binary, &home)];
            if let Some(cf) = &config.cloudflare {
                agents.insert(1, launchd::cloudflared(&home, &home.tool("cloudflared", &cf.cloudflared)));
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
        Cmd::Open { print } => {
            let config = Config::load(&home.config())?;
            home.ensure()?;
            let token = home.ensure_glass_token()?;
            let url = format!("http://{}/login?token={token}", config.http.listen);
            if print || !cfg!(target_os = "macos") {
                println!("{url}");
                eprintln!("the link signs a browser on this Mac in to Glass; keep it to yourself");
            } else {
                let status = Command::new("open").arg(&url).status().context("running `open`")?;
                if !status.success() {
                    bail!("`open` failed; run `henry open --print` and paste the link");
                }
            }
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

fn onboard(home: &Home, repo: &str, name: Option<&str>, hostname: Option<&str>, branch: Option<&str>) -> Result<()> {
    let name = match name {
        Some(n) => n.to_owned(),
        None => draft::name_for(repo).context("cannot tell the app's name from the URL; pass --name")?,
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

fn backups(home: &Home, app: &str) -> Result<()> {
    let canon = canon(home)?;
    let names: Vec<String> = if app == ark::HENRY {
        vec!["canon".into(), "config".into()]
    } else {
        canon.app(app)?;
        ark::targets(&canon, app)?.iter().map(ark::Target::name).collect()
    };
    if names.is_empty() {
        println!("`{app}` has no volumes on a live release; there is nothing to back up yet");
    }
    for name in names {
        println!("{name}");
        match canon.last_backup(app, &name)? {
            None => println!("  backup  never"),
            Some(b) if b.ok => {
                let snap = b.snapshot.as_deref().unwrap_or_default();
                let size = lease::format_bytes(b.bytes.unwrap_or(0).max(0) as u64);
                println!("  backup  {} — snapshot {}, {size}", utc(b.at), &snap[..snap.len().min(8)]);
            }
            Some(b) => {
                println!("  backup  FAILED {} — {}", utc(b.at), b.detail.as_deref().unwrap_or_default());
                if let Some(g) = canon.last_good_backup(app, &name)? {
                    println!("          last good {}", utc(g.at));
                }
            }
        }
        match (canon.last_drill(app, &name)?, canon.last_good_drill(app, &name)?) {
            (None, _) if name == "config" => println!("  drill   none needed: a copy of henry.toml"),
            (None, _) => println!("  drill   never: not yet proven restorable"),
            (Some(d), _) if d.ok => println!("  drill   passed {} — {}", utc(d.at), d.detail.unwrap_or_default()),
            (Some(d), good) => {
                println!("  drill   FAILED {} — {}", utc(d.at), d.detail.unwrap_or_default());
                match good {
                    Some(g) => println!("          last passed {}", utc(g.at)),
                    None => println!("          never passed: not yet proven restorable"),
                }
            }
        }
    }
    Ok(())
}

fn restore(home: &Home, target: &str, snapshot: Option<&str>, yes: bool) -> Result<()> {
    if !yes {
        bail!("a restore replaces what is there now; run again with --yes to confirm");
    }
    let config = Config::load(&home.config())?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    if target == ark::HENRY {
        // Henry's own state: only with the daemon stopped, since it is
        // replacing the very database the daemon runs on.
        let _lock = daemon::lock(home).context("stop Henry first (`henry uninstall`, or stop `henry run`)")?;
        let restored = home.root.join("canon.db.restored");
        {
            let canon = Arc::new(canon(home)?);
            let hush = open_hush(home, canon.clone())?;
            let ark = crate::backup::ark(home, &config, canon, &hush)?;
            rt.block_on(async {
                let snap = match snapshot {
                    Some(s) => s.to_owned(),
                    None => {
                        ark.latest(ark::HENRY, "canon")
                            .await?
                            .context("no backup of Henry's state in the repository")?
                            .id
                    }
                };
                ark.restore_file(&snap, "henry/canon.db", &restored).await?;
                println!("restored Henry's state from {}", &snap[..snap.len().min(8)]);
                anyhow::Ok(())
            })?;
        }
        Canon::open(&restored).context("the restored Canon does not open")?;
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", home.canon().display()));
        }
        std::fs::rename(&restored, home.canon())?;
        println!("start Henry again; it picks up where the backup left off");
        return Ok(());
    }

    let mut parts = target.splitn(3, '/');
    let (Some(app), Some(service), Some(volume)) = (parts.next(), parts.next(), parts.next()) else {
        bail!("write the target as <app>/<service>/<volume>, or `henry`");
    };
    let canon = Arc::new(canon(home)?);
    let hush = open_hush(home, canon.clone())?;
    let t = ark::targets(&canon, app)?
        .into_iter()
        .find(|t| t.service == service && t.volume == volume)
        .with_context(|| format!("`{app}` has no volume {service}/{volume} on its live release"))?;
    let password = hush.get(app, "POSTGRES_PASSWORD")?;
    let ark = crate::backup::ark(home, &config, canon.clone(), &hush)?;
    rt.block_on(async {
        let snap = match snapshot {
            Some(s) => s.to_owned(),
            None => ark.latest(app, &t.name()).await?.context("no snapshot of it in the repository")?.id,
        };
        println!("{}", ark.restore(&t, &snap, password.as_ref()).await?);
        anyhow::Ok(())
    })?;
    println!("start it again with `henry start {app}`");
    Ok(())
}

fn upkeep(home: &Home, cmd: Option<UpkeepCmd>) -> Result<()> {
    let canon = canon(home)?;
    if let Some(UpkeepCmd::Run { task }) = cmd {
        let t = mend::Task::parse(&task)
            .with_context(|| format!("no task `{task}`: disk, refresh, host, tools or bench"))?;
        let job = canon.enqueue(mend::JOB, Some(t.as_str()), &serde_json::json!({}))?;
        println!("{task} queued (job {}); the daemon runs it", job.id);
        return Ok(());
    }
    let config = Config::load(&home.config())?;
    let on = config.mend.tasks();
    for t in mend::Task::ALL {
        let state = if on.contains(&t) { "" } else { " (off)" };
        match canon.upkeep(t.as_str())? {
            None => println!("{:<8}{state} never run", t.as_str()),
            Some(u) => println!(
                "{:<8}{state} {} {} — {}",
                t.as_str(),
                if u.ok { "ok    " } else { "FAILED" },
                utc(u.last_at),
                u.detail.unwrap_or_default()
            ),
        }
    }
    Ok(())
}

fn tools(home: &Home, cmd: Option<ToolsCmd>) -> Result<()> {
    let canon = Arc::new(canon(home)?);
    match cmd {
        Some(ToolsCmd::Adopt { name }) => {
            let config = Config::load(&home.config())?;
            let hush = open_hush(home, canon.clone())?;
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            let v = rt.block_on(crate::upkeep::adopt(home, &config, &canon, &hush, &name))?;
            println!("{name} {v} installed in {}", home.tools().join(&name).display());
            println!("Henry upgrades it weekly from now on; run `henry install` so agents use it");
        }
        None => {
            let managed = canon.tools()?;
            if managed.is_empty() {
                println!("Henry manages no tools yet; `henry tools adopt caddy` (or cloudflared, restic, railpack)");
            }
            for t in managed {
                let prev = t.previous.map(|p| format!(", rolls back to {p}")).unwrap_or_default();
                println!("{:<12} {}{prev} — since {}", t.name, t.current, utc(t.updated_at));
            }
        }
    }
    Ok(())
}

fn bench(home: &Home, run: bool) -> Result<()> {
    let canon = Arc::new(canon(home)?);
    if run {
        let config = Config::load(&home.config())?;
        let hush = open_hush(home, canon.clone())?;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let (summary, reports) = rt.block_on(crate::upkeep::bench(&config, &canon, &hush))?;
        for r in &reports {
            print!("{}", r.summary());
        }
        println!("{summary}");
        return Ok(());
    }
    let runs = canon.bench_latest()?;
    if runs.is_empty() {
        println!("no Bench runs yet; `henry bench --run` scores the configured models");
    }
    for r in runs {
        println!("{:<44} {}/{}  {} tokens  {}", r.model, r.passed, r.total, r.tokens, utc(r.at));
    }
    Ok(())
}

fn key(home: &Home, cmd: KeyCmd) -> Result<()> {
    use hush::KeyStore as _;
    let store = key_store(home);
    match cmd {
        KeyCmd::Export => {
            let key = store.load()?.context("this Mac has no Henry key yet")?;
            eprintln!("Henry's master key. Anyone with it and your Canon can read every secret; store it offline.");
            println!("{}", key.to_hex().as_str());
        }
        KeyCmd::Import => {
            let mut text = zeroize::Zeroizing::new(String::new());
            std::io::stdin().read_to_string(&mut text)?;
            store.import(&hush::Key::from_hex(&text)?)?;
            println!("imported the key; `henry restore henry` can now bring back Henry's state");
        }
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

    #[test]
    fn the_command_line_is_well_formed() {
        super::Args::command().debug_assert();
    }
}
