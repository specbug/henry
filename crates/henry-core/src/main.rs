//! Henry - Personal Home Server Daemon
//!
//! A security-first, minimal Rust daemon for managing home server functions.

use clap::{Parser, Subcommand};
use henry_config::Config;
use henry_core::{Daemon, DaemonError};
use henry_lock::InstanceLock;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Parser)]
#[command(name = "henry")]
#[command(author, version, about = "Personal home server daemon", long_about = None)]
struct Cli {
    /// Configuration file path
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the daemon (default)
    Run {
        /// Run without TUI (headless mode)
        #[arg(long)]
        headless: bool,
    },

    /// Check daemon status
    Status,

    /// Validate configuration
    Check,

    /// Show daemon version and info
    Info,

    /// Generate example configuration
    Init {
        /// Output path for config file
        #[arg(short, long, default_value = "config.toml")]
        output: PathBuf,

        /// Overwrite existing file
        #[arg(short, long)]
        force: bool,
    },

    /// Deploy an app from a git repository
    Deploy {
        /// Git repository URL (HTTPS)
        repo: String,

        /// App name (derived from repo if not provided)
        #[arg(short, long)]
        name: Option<String>,

        /// Domain for the app (defaults to <name>.local)
        #[arg(short, long)]
        domain: Option<String>,

        /// Git branch to deploy
        #[arg(short, long, default_value = "main")]
        branch: String,

        /// Container port
        #[arg(short, long, default_value = "8080")]
        port: u16,
    },

    /// List deployed apps
    Apps,

    /// Show app logs
    Logs {
        /// App name
        name: String,

        /// Number of lines to show
        #[arg(short, long, default_value = "50")]
        tail: usize,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Set up logging
    let filter = if cli.verbose {
        EnvFilter::new("debug")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
    };

    tracing_subscriber::registry()
        .with(fmt::layer().with_target(false))
        .with(filter)
        .init();

    match cli.command.unwrap_or(Commands::Run { headless: false }) {
        Commands::Run { headless } => {
            run_daemon(cli.config, headless).await?;
        }
        Commands::Status => {
            show_status(cli.config)?;
        }
        Commands::Check => {
            check_config(cli.config)?;
        }
        Commands::Info => {
            show_info();
        }
        Commands::Init { output, force } => {
            init_config(&output, force)?;
        }
        Commands::Deploy {
            repo,
            name,
            domain,
            branch,
            port,
        } => {
            deploy_app(cli.config, &repo, name, domain, &branch, port).await?;
        }
        Commands::Apps => {
            list_apps(cli.config).await?;
        }
        Commands::Logs { name, tail } => {
            show_app_logs(cli.config, &name, tail).await?;
        }
    }

    Ok(())
}

async fn run_daemon(config_path: Option<PathBuf>, headless: bool) -> Result<(), DaemonError> {
    info!("Starting Henry v{}", env!("CARGO_PKG_VERSION"));

    let daemon = Daemon::new(config_path).await?;

    if headless {
        daemon.run_headless().await
    } else {
        daemon.run_with_tui().await
    }
}

fn show_status(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let config = load_config(config_path)?;

    match InstanceLock::check_running(config.daemon.lock_port, &config.daemon.pid_file) {
        Some(pid) => {
            if pid == 0 {
                println!("Henry is running (PID unknown)");
            } else {
                println!("Henry is running (PID {})", pid);
            }
            println!("  Lock port: {}", config.daemon.lock_port);
            println!("  PID file:  {}", config.daemon.pid_file.display());
        }
        None => {
            println!("Henry is not running");
        }
    }

    Ok(())
}

fn check_config(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    match load_config(config_path) {
        Ok(config) => {
            println!("Configuration is valid");
            println!();
            println!("Enabled modules:");
            if config.containers.enabled {
                println!("  ✓ Containers (Podman: {})", config.containers.use_podman);
            }
            if config.telegram.enabled {
                println!(
                    "  ✓ Telegram ({} allowed users)",
                    config.telegram.allowed_users.len()
                );
            }
            if config.http.enabled {
                println!("  ✓ HTTP API (port {})", config.http.port);
            }
            if config.media.enabled {
                println!("  ✓ Media ({} library paths)", config.media.library_paths.len());
            }
            if config.claude.enabled {
                println!("  ✓ Claude (max {} sessions)", config.claude.max_sessions);
            }
            Ok(())
        }
        Err(e) => {
            error!("Configuration error: {}", e);
            std::process::exit(1);
        }
    }
}

fn show_info() {
    println!("Henry - Personal Home Server Daemon");
    println!();
    println!("Version:     {}", env!("CARGO_PKG_VERSION"));
    println!("Platform:    {} / {}", std::env::consts::OS, std::env::consts::ARCH);
    println!();
    println!("Features:");
    println!("  • Container management (Podman)");
    println!("  • Telegram bot control");
    println!("  • HTTP/WebSocket API");
    println!("  • Media server (Jellyfin)");
    println!("  • Claude AI integration");
    println!();
    println!("Config paths (in order of precedence):");
    for path in Config::default_paths() {
        let exists = if path.exists() { " ✓" } else { "" };
        println!("  {}{}", path.display(), exists);
    }
}

fn init_config(output: &PathBuf, force: bool) -> anyhow::Result<()> {
    if output.exists() && !force {
        error!(
            "File already exists: {}. Use --force to overwrite.",
            output.display()
        );
        std::process::exit(1);
    }

    let config = Config::default();
    let toml = config.to_toml()?;

    // Add header comment
    let content = format!(
        "# Henry Configuration\n\
         # See https://github.com/rishitv/henry for documentation\n\
         #\n\
         # Secret references use 1Password CLI format:\n\
         #   op://vault/item/field\n\
         #\n\n{}",
        toml
    );

    std::fs::write(output, content)?;
    println!("Created configuration file: {}", output.display());
    println!();
    println!("Edit this file to configure Henry, then run:");
    println!("  henry check    # Validate configuration");
    println!("  henry run      # Start the daemon");

    Ok(())
}

fn load_config(config_path: Option<PathBuf>) -> Result<Config, henry_config::ConfigError> {
    match config_path {
        Some(path) => Config::load(&path),
        None => Config::load_default(),
    }
}

/// Extract app name from a GitHub/GitLab URL
fn extract_app_name(repo_url: &str) -> Option<String> {
    // Handle URLs like https://github.com/user/repo or https://github.com/user/repo.git
    let url = repo_url.trim_end_matches(".git");
    url.rsplit('/').next().map(|s| s.to_lowercase())
}

async fn deploy_app(
    config_path: Option<PathBuf>,
    repo: &str,
    name: Option<String>,
    domain: Option<String>,
    branch: &str,
    port: u16,
) -> anyhow::Result<()> {
    // Derive app name from repo URL if not provided
    let app_name = match name {
        Some(n) => n,
        None => extract_app_name(repo).ok_or_else(|| {
            anyhow::anyhow!("Could not derive app name from repo URL. Please provide --name")
        })?,
    };

    // Default domain to <name>.local
    let app_domain = domain.unwrap_or_else(|| format!("{}.local", app_name));

    println!("Deploying app...");
    println!("  Name:   {}", app_name);
    println!("  Repo:   {}", repo);
    println!("  Branch: {}", branch);
    println!("  Domain: {}", app_domain);
    println!("  Port:   {}", port);
    println!();

    // Load or create config
    let config = match load_config(config_path.clone()) {
        Ok(c) => c,
        Err(_) => {
            println!("No config found, using defaults...");
            Config::default()
        }
    };

    // Initialize container manager
    let runtime_type = if config.containers.use_podman { "Podman" } else { "Docker" };
    println!("Using container runtime: {}", runtime_type);

    let container_mgr = henry_server::ContainerManager::new(config.containers.clone())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to {}: {}", runtime_type, e))?;

    let containers = Arc::new(RwLock::new(container_mgr));

    // Create apps manager
    let mut apps_config = config.apps.clone();
    apps_config.enabled = true;
    apps_config.default_port = port;

    let mut apps_mgr = henry_apps::AppsManager::new(apps_config, containers.clone());

    // Initialize (creates workspace dir)
    apps_mgr.init().await.map_err(|e| anyhow::anyhow!("Failed to initialize apps manager: {}", e))?;

    // Create deploy request
    let request = henry_apps::CreateAppRequest {
        name: app_name.clone(),
        repo_url: repo.to_string(),
        domain: app_domain.clone(),
        path_prefix: "/".to_string(),
        branch: Some(branch.to_string()),
        env_vars: None,
        port: Some(port),
    };

    // Deploy
    println!("Starting deployment...");
    match apps_mgr.deploy(request).await {
        Ok(app) => {
            println!();
            println!("Successfully deployed!");
            println!("  Container: {}", app.container_name);
            println!("  Image:     {}", app.image_name);
            println!("  URL:       {}", app.url());
            println!();
            println!("View logs: henry logs {}", app_name);
        }
        Err(e) => {
            error!("Deployment failed: {}", e);
            return Err(anyhow::anyhow!("Deployment failed: {}", e));
        }
    }

    Ok(())
}

async fn list_apps(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let config = load_config(config_path)?;

    let container_mgr = henry_server::ContainerManager::new(config.containers.clone())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to container runtime: {}", e))?;

    let containers = Arc::new(RwLock::new(container_mgr));
    let mut apps_mgr = henry_apps::AppsManager::new(config.apps.clone(), containers);
    apps_mgr.init().await?;

    let apps = apps_mgr.list().await;

    if apps.is_empty() {
        println!("No apps deployed yet.");
        println!();
        println!("Deploy an app:");
        println!("  henry deploy https://github.com/user/repo");
        return Ok(());
    }

    println!("Deployed apps:");
    println!();
    for app in apps {
        println!(
            "  {} ({}) - {}",
            app.name,
            app.status,
            app.url()
        );
    }

    Ok(())
}

async fn show_app_logs(config_path: Option<PathBuf>, name: &str, tail: usize) -> anyhow::Result<()> {
    let config = load_config(config_path)?;

    let container_mgr = henry_server::ContainerManager::new(config.containers.clone())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to container runtime: {}", e))?;

    let containers = Arc::new(RwLock::new(container_mgr));
    let mut apps_mgr = henry_apps::AppsManager::new(config.apps.clone(), containers);
    apps_mgr.init().await?;

    match apps_mgr.logs(name, tail).await {
        Ok(logs) => {
            println!("{}", logs);
        }
        Err(henry_apps::AppsError::NotFound(_)) => {
            error!("App not found: {}", name);
            println!();
            println!("List apps with: henry apps");
        }
        Err(e) => {
            return Err(anyhow::anyhow!("Failed to get logs: {}", e));
        }
    }

    Ok(())
}
