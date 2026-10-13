//! Henry: rules the home. Hosts your GitHub repos on a Mac mini.

mod cli;
mod daemon;
mod home;
mod host;
mod http;
mod launchd;
mod time;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let args = cli::Args::parse();
    if !args.is_daemon() {
        // A CLI piped into `head` should stop quietly when the pipe closes,
        // as every Unix tool does, not panic on the next println.
        // SAFETY: resetting a signal disposition before any threads start.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
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
