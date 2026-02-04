//! Unix signal handling for the daemon.
//!
//! Handles SIGTERM, SIGINT for graceful shutdown, and SIGUSR1 for config reload.

use tokio::sync::mpsc;
use tracing::{debug, info};

#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};

/// Signal handler for graceful shutdown and config reload.
pub struct SignalHandler {
    shutdown_tx: mpsc::Sender<()>,
    reload_tx: mpsc::Sender<()>,
}

impl SignalHandler {
    /// Create a new signal handler.
    pub fn new(shutdown_tx: mpsc::Sender<()>, reload_tx: mpsc::Sender<()>) -> Self {
        Self {
            shutdown_tx,
            reload_tx,
        }
    }

    /// Run the signal handler loop.
    #[cfg(unix)]
    pub async fn run(self) {
        let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM");
        let mut sigint = signal(SignalKind::interrupt()).expect("failed to register SIGINT");
        let mut sigusr1 = signal(SignalKind::user_defined1()).expect("failed to register SIGUSR1");
        let mut sighup = signal(SignalKind::hangup()).expect("failed to register SIGHUP");

        loop {
            tokio::select! {
                _ = sigterm.recv() => {
                    info!("Received SIGTERM, initiating shutdown");
                    let _ = self.shutdown_tx.send(()).await;
                    break;
                }
                _ = sigint.recv() => {
                    info!("Received SIGINT, initiating shutdown");
                    let _ = self.shutdown_tx.send(()).await;
                    break;
                }
                _ = sigusr1.recv() => {
                    info!("Received SIGUSR1, triggering config reload");
                    let _ = self.reload_tx.send(()).await;
                }
                _ = sighup.recv() => {
                    info!("Received SIGHUP, triggering config reload");
                    let _ = self.reload_tx.send(()).await;
                }
            }
        }

        debug!("Signal handler stopped");
    }

    /// Stub for non-Unix platforms.
    #[cfg(not(unix))]
    pub async fn run(self) {
        // On non-Unix, just wait for Ctrl+C
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for ctrl+c");
        info!("Received Ctrl+C, initiating shutdown");
        let _ = self.shutdown_tx.send(()).await;
    }
}
