//! Core daemon functionality for Henry.
//!
//! Provides the main daemon loop, signal handling, and module coordination.

pub mod daemon;
pub mod signals;

pub use daemon::{Daemon, DaemonError};
