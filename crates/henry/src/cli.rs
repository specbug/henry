//! The `henry` command line.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use canon::{Canon, Desired, Source};
use clap::{Parser, Subcommand};
use hush::{HOST, Hush, Secret};
use lease::{Lease, Refusal};

use crate::home::{Config, Home};
use crate::time::utc;
use crate::{daemon, launchd};

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
    /// Install Henry as a LaunchAgent so it starts at login and stays up.
    Install {
        /// Print the agent instead of installing it.
        #[arg(long)]
        print: bool,
    },
    /// Remove the LaunchAgent. Apps keep running until the Mac restarts.
    Uninstall,
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

pub fn run(args: Args) -> Result<ExitCode> {
    let home = Home::locate()?;
    match args.cmd {
        Cmd::Run => daemon::run(&home)?,
        Cmd::Check { lease } => return check(&home, &lease),
        Cmd::Add { lease, repo } => add(&home, &lease, &repo)?,
        Cmd::Apps => apps(&home)?,
        Cmd::Show { app } => show(&home, &app)?,
        Cmd::Events { app, limit } => events(&home, app.as_deref(), limit)?,
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
            if print {
                print!("{}", launchd::plist(&binary, &home));
            } else {
                home.ensure()?;
                let path = launchd::install(&binary, &home)?;
                println!("installed {} and started Henry", path.display());
            }
        }
        Cmd::Uninstall => {
            launchd::uninstall()?;
            println!("uninstalled the LaunchAgent");
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
    let apps = canon(home)?.apps()?;
    if apps.is_empty() {
        println!("no apps yet; register one with `henry add lease.toml --repo <url>`");
        return Ok(());
    }
    println!("{:<20} {:<11} {:<8} {:<8} UPDATED", "APP", "STATUS", "WANTED", "RELEASE");
    for a in apps {
        let release = a.live_release.map_or_else(|| "-".to_owned(), |r| r.to_string());
        println!("{:<20} {:<11} {:<8} {:<8} {}", a.name, a.status, a.desired, release, utc(a.updated_at));
    }
    Ok(())
}

fn show(home: &Home, name: &str) -> Result<()> {
    let canon = Arc::new(canon(home)?);
    let app = canon.app(name)?;
    let lease = app.lease()?;
    println!("{}", app.name);
    println!("  repo      {} ({})", app.repo, lease.branch);
    match &app.status_detail {
        Some(d) => println!("  status    {} — {d}", app.status),
        None => println!("  status    {}", app.status),
    }
    println!("  wanted    {}", app.desired);
    println!("  lease     written by {}", app.source);

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
            let reason = r.reason.map(|x| format!(" — {x}")).unwrap_or_default();
            println!("    {:>5}  {sha:<10}  {:<10} {}{reason}", r.id, r.state, utc(r.created_at));
        }
    }
    println!("  events");
    for e in canon.events(Some(&app.name), 10)? {
        println!("    {}  {}", utc(e.at), e.message);
    }
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
