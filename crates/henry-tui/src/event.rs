//! Event handling for the TUI.

use crossterm::event::{self, KeyCode, KeyEvent, KeyModifiers};
use std::time::Duration;
use tokio::sync::mpsc;

/// TUI events.
#[derive(Debug, Clone)]
pub enum Event {
    /// Terminal tick for UI refresh.
    Tick,
    /// Key press event.
    Key(KeyEvent),
    /// Resize event.
    Resize(u16, u16),
    /// Quit request.
    Quit,
}

/// Event handler for terminal events.
pub struct EventHandler {
    rx: mpsc::Receiver<Event>,
    _tx: mpsc::Sender<Event>,
}

impl EventHandler {
    /// Create a new event handler.
    pub fn new(tick_rate: Duration) -> Self {
        let (tx, rx) = mpsc::channel(100);
        let event_tx = tx.clone();

        // Spawn event polling task
        tokio::spawn(async move {
            loop {
                if event::poll(tick_rate).unwrap_or(false) {
                    match event::read() {
                        Ok(event::Event::Key(key)) => {
                            // Handle quit shortcuts
                            if key.code == KeyCode::Char('c')
                                && key.modifiers.contains(KeyModifiers::CONTROL)
                            {
                                let _ = event_tx.send(Event::Quit).await;
                                break;
                            }
                            if key.code == KeyCode::Char('q') && key.modifiers.is_empty() {
                                let _ = event_tx.send(Event::Quit).await;
                                break;
                            }
                            let _ = event_tx.send(Event::Key(key)).await;
                        }
                        Ok(event::Event::Resize(w, h)) => {
                            let _ = event_tx.send(Event::Resize(w, h)).await;
                        }
                        _ => {}
                    }
                } else {
                    let _ = event_tx.send(Event::Tick).await;
                }
            }
        });

        Self { rx, _tx: tx }
    }

    /// Receive next event.
    pub async fn next(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
}
