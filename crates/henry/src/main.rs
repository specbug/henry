//! Henry: rules the home. Hosts your GitHub repos on a Mac mini.

mod cli;
mod daemon;
mod home;
mod launchd;
mod time;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let args = cli::Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("HENRY_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(if args.verbose { "debug" } else { "info" })),
        )
        .with_writer(std::io::stderr)
        .init();

    match cli::run(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("henry: {e:#}");
            ExitCode::FAILURE
        }
    }
}
