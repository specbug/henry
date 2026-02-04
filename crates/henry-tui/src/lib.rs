//! Terminal UI for Henry daemon using Ratatui.
//!
//! Provides a Claude Code-inspired interface with module status, activity logs,
//! and system metrics.

mod app;
pub mod ui;
mod event;

pub use app::{App, AppState};
pub use event::{Event, EventHandler};

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use std::io::{self, stdout};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum TuiError {
    #[error("terminal error: {0}")]
    Terminal(#[from] io::Error),

    #[error("event error: {0}")]
    Event(String),
}

/// Terminal wrapper for setup/teardown.
pub struct Terminal {
    terminal: ratatui::Terminal<CrosstermBackend<io::Stdout>>,
}

impl Terminal {
    /// Create and initialize the terminal.
    pub fn new() -> Result<Self, TuiError> {
        enable_raw_mode()?;
        let mut stdout = stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = ratatui::Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    /// Get mutable reference to the inner terminal.
    pub fn inner_mut(&mut self) -> &mut ratatui::Terminal<CrosstermBackend<io::Stdout>> {
        &mut self.terminal
    }

    /// Restore terminal to original state.
    pub fn restore(&mut self) -> Result<(), TuiError> {
        disable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        self.terminal.show_cursor()?;
        Ok(())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
