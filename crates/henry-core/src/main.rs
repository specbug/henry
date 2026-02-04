//! Henry - Personal Home Server Daemon
//!
//! A security-first, minimal Rust daemon for managing home server functions.

use clap::{Parser, Subcommand};
use henry_config::Config;
use henry_core::{Daemon, DaemonError};
use henry_lock::InstanceLock;
use std::path::PathBuf;
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
