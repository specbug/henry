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

        /// Path prefix (e.g., /app/myapp)
        #[arg(long)]
        path: Option<String>,

        /// Git branch to deploy
        #[arg(short, long, default_value = "main")]
        branch: String,

        /// Container port (auto-detected per service if not set)
        #[arg(short, long)]
        port: Option<u16>,
    },

    /// List deployed apps
    Apps,

    /// Show app logs
    Logs {
        /// App name
        name: String,

        /// Service name (for multi-service apps)
        #[arg(short, long)]
        service: Option<String>,

        /// Number of lines to show
        #[arg(short, long, default_value = "50")]
        tail: usize,
    },

    /// Install Henry as a macOS LaunchAgent (auto-starts on login)
    Install {
        /// Force reinstall if already installed
        #[arg(short, long)]
        force: bool,
    },

    /// Uninstall the Henry LaunchAgent
    Uninstall,
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
            path,
            branch,
            port,
        } => {
            deploy_app(cli.config, &repo, name, domain, path, &branch, port).await?;
        }
        Commands::Apps => {
            list_apps(cli.config).await?;
        }
        Commands::Logs {
            name,
            service,
            tail,
        } => {
            show_app_logs(cli.config, &name, service.as_deref(), tail).await?;
        }
        Commands::Install { force } => {
            install_daemon(cli.config, force).await?;
        }
        Commands::Uninstall => {
            uninstall_daemon().await?;
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

    // Show launchd status on macOS
    #[cfg(target_os = "macos")]
    {
        let launchd_status = henry_core::launchd::status();
        println!("  LaunchAgent: {}", launchd_status);
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
    println!("  - Container management (Podman)");
    println!("  - Telegram bot control");
    println!("  - HTTP/WebSocket API");
    println!("  - Media server (Jellyfin)");
    println!("  - Claude AI integration");
    println!("  - Multi-service app deployment");
    println!("  - macOS LaunchAgent support");
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
    path: Option<String>,
    branch: &str,
    port: Option<u16>,
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
    let app_path = path.unwrap_or_else(|| "/".to_string());

    println!("Deploying app...");
    println!("  Name:   {}", app_name);
    println!("  Repo:   {}", repo);
    println!("  Branch: {}", branch);
    println!("  Domain: {}", app_domain);
    println!("  Path:   {}", app_path);
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
    if let Some(p) = port {
        apps_config.default_port = p;
    }

    let mut apps_mgr = henry_apps::AppsManager::new(apps_config, containers.clone());

    // Add Claude manager if available
    if config.claude.enabled {
        let claude_mgr = henry_claude::ClaudeManager::new(config.claude.clone(), None);
        apps_mgr = apps_mgr.with_claude(Arc::new(RwLock::new(claude_mgr)));
    }

    // Initialize (creates workspace dir)
    apps_mgr.init().await.map_err(|e| anyhow::anyhow!("Failed to initialize apps manager: {}", e))?;

    // Create deploy request
    let request = henry_apps::CreateAppRequest {
        name: app_name.clone(),
        repo_url: repo.to_string(),
        domain: app_domain.clone(),
        path_prefix: app_path,
        branch: Some(branch.to_string()),
        env_vars: None,
        port,
    };

    // Deploy
    println!("Starting deployment...");
    match apps_mgr.deploy(request).await {
        Ok(app) => {
            println!();
            println!("Successfully deployed!");

            if app.services.len() > 1 {
                println!("  Services:");
                for svc in &app.services {
                    let svc_url = if svc.path_prefix == "/" {
                        format!("https://{}", app.domain)
                    } else {
                        format!("https://{}{}", app.domain, svc.path_prefix)
                    };
                    println!(
                        "    {:<10} -> {}:{:<5}  ({})",
                        svc.name, svc.container_name, svc.port, svc_url
                    );
                }
            } else {
                println!("  Container: {}", app.container_name);
                println!("  Image:     {}", app.image_name);
                println!("  URL:       {}", app.url());
            }

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

        if app.services.len() > 1 {
            for svc in &app.services {
                println!(
                    "    {} [{}] ({}) -> {}:{}",
                    svc.name,
                    svc.service_type,
                    svc.status,
                    svc.container_name,
                    svc.port
                );
            }
        }
    }

    Ok(())
}

async fn show_app_logs(
    config_path: Option<PathBuf>,
    name: &str,
    service: Option<&str>,
    tail: usize,
) -> anyhow::Result<()> {
    let config = load_config(config_path)?;

    let container_mgr = henry_server::ContainerManager::new(config.containers.clone())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to container runtime: {}", e))?;

    let containers = Arc::new(RwLock::new(container_mgr));
    let mut apps_mgr = henry_apps::AppsManager::new(config.apps.clone(), containers);
    apps_mgr.init().await?;

    let result = if let Some(svc_name) = service {
        apps_mgr.service_logs(name, svc_name, tail).await
    } else {
        apps_mgr.logs(name, tail).await
    };

    match result {
        Ok(logs) => {
            println!("{}", logs);
        }
        Err(henry_apps::AppsError::NotFound(msg)) => {
            error!("Not found: {}", msg);
            println!();
            println!("List apps with: henry apps");
        }
        Err(e) => {
            return Err(anyhow::anyhow!("Failed to get logs: {}", e));
        }
    }

    Ok(())
}

async fn install_daemon(config_path: Option<PathBuf>, force: bool) -> anyhow::Result<()> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (config_path, force);
        return Err(anyhow::anyhow!(
            "The install command is only supported on macOS (launchd)"
        ));
    }

    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let log_dir = PathBuf::from(&home).join(".henry").join("logs");

        let config = config_path.as_deref();

        match henry_core::launchd::install(config, &log_dir, force).await {
            Ok(()) => {
                println!("Henry LaunchAgent installed successfully!");
                println!();
                println!("Henry will now:");
                println!("  - Start automatically on login");
                println!("  - Restart automatically if it crashes");
                println!("  - Run in headless mode (no TUI)");
                println!();
                println!("Logs:");
                println!("  stdout: {}/henry.stdout.log", log_dir.display());
                println!("  stderr: {}/henry.stderr.log", log_dir.display());
                println!();
                println!("Plist: {}", henry_core::launchd::plist_path().display());
                println!();
                println!("To uninstall: henry uninstall");
            }
            Err(e) => {
                return Err(anyhow::anyhow!("Failed to install LaunchAgent: {}", e));
            }
        }

        Ok(())
    }
}

async fn uninstall_daemon() -> anyhow::Result<()> {
    #[cfg(not(target_os = "macos"))]
    {
        return Err(anyhow::anyhow!(
            "The uninstall command is only supported on macOS (launchd)"
        ));
    }

    #[cfg(target_os = "macos")]
    {
        match henry_core::launchd::uninstall().await {
            Ok(()) => {
                println!("Henry LaunchAgent uninstalled successfully.");
                println!("Henry will no longer start automatically.");
            }
            Err(e) => {
                return Err(anyhow::anyhow!("Failed to uninstall LaunchAgent: {}", e));
            }
        }

        Ok(())
    }
}
