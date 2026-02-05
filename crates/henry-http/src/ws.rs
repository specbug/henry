//! WebSocket support for real-time event streaming.

use axum::{
    extract::{
        ws::{Message, WebSocket},
        Query, State, WebSocketUpgrade,
    },
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{auth, AppState};

/// Events pushed to WebSocket clients.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsEvent {
    Connected {
        client_id: String,
    },
    HealthUpdate {
        cpu_percent: f32,
        memory_percent: f32,
        disk_percent: f32,
        uptime_secs: u64,
    },
    ContainerStateChanged {
        name: String,
        old_state: String,
        new_state: String,
    },
    AnomalyDetected {
        id: String,
        event_type: String,
        severity: String,
    },
    AnomalyResolved {
        id: String,
    },
    ActionExecuted {
        id: String,
        action_type: String,
        status: String,
    },
    AgentTaskUpdate {
        task_id: String,
        status: String,
        progress: String,
    },
    Pong,
}

/// Messages from WebSocket clients.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsClientMessage {
    Ping,
}

/// Query parameters for WebSocket connection.
#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub token: Option<String>,
}

/// WebSocket upgrade handler.
pub async fn ws_handler(
    State(state): State<AppState>,
    Query(query): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    // Authenticate if token is configured
    if let Some(ref expected_token) = state.api_token {
        let provided_token = query.token.as_deref().unwrap_or("");
        if !auth::verify_token(provided_token, expected_token) {
            warn!("WebSocket connection rejected: invalid token");
            return Response::builder()
                .status(401)
                .body("Unauthorized".into())
                .unwrap();
        }
    }

    let event_rx = state.event_tx.subscribe();
    ws.on_upgrade(move |socket| handle_socket(socket, event_rx))
}

/// Handle a WebSocket connection.
async fn handle_socket(socket: WebSocket, mut event_rx: broadcast::Receiver<WsEvent>) {
    let client_id = Uuid::new_v4().to_string();
    debug!("WebSocket client connected: {}", client_id);

    let (mut sender, mut receiver) = socket.split();

    // Send connected event
    let connected_event = WsEvent::Connected {
        client_id: client_id.clone(),
    };
    if let Ok(json) = serde_json::to_string(&connected_event) {
        if sender.send(Message::Text(json.into())).await.is_err() {
            return;
        }
    }

    // Spawn task to forward broadcast events to client
    let client_id_clone = client_id.clone();
    let send_task = tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    if let Ok(json) = serde_json::to_string(&event) {
                        if sender.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("WebSocket client {} lagged by {} messages", client_id_clone, n);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    });

    // Handle incoming messages from client
    while let Some(result) = receiver.next().await {
        match result {
            Ok(Message::Text(text)) => {
                if let Ok(msg) = serde_json::from_str::<WsClientMessage>(&text) {
                    match msg {
                        WsClientMessage::Ping => {
                            // Pong is handled by the send task via broadcast
                            debug!("Ping received from client {}", client_id);
                        }
                    }
                }
            }
            Ok(Message::Close(_)) => {
                debug!("WebSocket client {} disconnected", client_id);
                break;
            }
            Err(e) => {
                warn!("WebSocket error for client {}: {}", client_id, e);
                break;
            }
            _ => {}
        }
    }

    // Abort the send task when client disconnects
    send_task.abort();
    debug!("WebSocket client {} cleanup complete", client_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ws_event_serialization() {
        let event = WsEvent::Connected {
            client_id: "test-123".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"connected\""));
        assert!(json.contains("\"client_id\":\"test-123\""));

        let health = WsEvent::HealthUpdate {
            cpu_percent: 45.5,
            memory_percent: 60.2,
            disk_percent: 75.0,
            uptime_secs: 3600,
        };
        let json = serde_json::to_string(&health).unwrap();
        assert!(json.contains("\"type\":\"health_update\""));
        assert!(json.contains("\"cpu_percent\":45.5"));

        let container = WsEvent::ContainerStateChanged {
            name: "nginx".to_string(),
            old_state: "stopped".to_string(),
            new_state: "running".to_string(),
        };
        let json = serde_json::to_string(&container).unwrap();
        assert!(json.contains("\"type\":\"container_state_changed\""));
        assert!(json.contains("\"name\":\"nginx\""));
    }

    #[test]
    fn test_ws_client_message_parsing() {
        let json = r#"{"type":"ping"}"#;
        let msg: WsClientMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, WsClientMessage::Ping));
    }
}
