//! Anthropic API client.
//!
//! Provides a simple HTTP client for the Anthropic Messages API.

use crate::types::{Message, MessageResponse};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::debug;

/// API client errors.
#[derive(Error, Debug)]
pub enum ApiError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("API error: {status} - {message}")]
    Api { status: u16, message: String },

    #[error("failed to parse response: {0}")]
    Parse(String),
}

/// Anthropic API client.
pub struct AnthropicClient {
    client: Client,
    api_key: String,
    base_url: String,
}

impl AnthropicClient {
    /// API version header value.
    const API_VERSION: &'static str = "2023-06-01";

    /// Default base URL for the API.
    const DEFAULT_BASE_URL: &'static str = "https://api.anthropic.com";

    /// Create a new API client with the given API key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
            base_url: Self::DEFAULT_BASE_URL.to_string(),
        }
    }

    /// Create a new API client with a custom base URL.
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
            base_url: base_url.into(),
        }
    }

    /// Send a message to Claude and get a response.
    pub async fn send_message(
        &self,
        messages: &[Message],
        model: &str,
        max_tokens: u32,
        system: Option<&str>,
    ) -> Result<MessageResponse, ApiError> {
        let url = format!("{}/v1/messages", self.base_url);

        let request = MessageRequest {
            model: model.to_string(),
            max_tokens,
            messages: messages.to_vec(),
            system: system.map(|s| s.to_string()),
        };

        debug!("Sending message to Anthropic API: model={}", model);

        let response = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", Self::API_VERSION)
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?;

        let status = response.status();

        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            return Err(ApiError::Api {
                status: status.as_u16(),
                message: error_body,
            });
        }

        let api_response: ApiMessageResponse = response.json().await?;

        // Extract text content from the response
        let content = api_response
            .content
            .into_iter()
            .filter_map(|block| {
                if block.content_type == "text" {
                    Some(block.text)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        Ok(MessageResponse {
            id: api_response.id,
            model: api_response.model,
            content,
            input_tokens: api_response.usage.input_tokens,
            output_tokens: api_response.usage.output_tokens,
            stop_reason: api_response.stop_reason,
        })
    }

    /// Check if the API is reachable and the key is valid.
    pub async fn ping(&self) -> bool {
        // Send a minimal request to validate the API key
        let messages = vec![Message::user("Hi")];
        match self.send_message(&messages, "claude-3-haiku-20240307", 10, None).await {
            Ok(_) => true,
            Err(ApiError::Api { status, .. }) if status == 401 => false,
            Err(_) => false,
        }
    }
}

/// Request body for the messages API.
#[derive(Debug, Serialize)]
struct MessageRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
}

/// Response from the messages API.
#[derive(Debug, Deserialize)]
struct ApiMessageResponse {
    id: String,
    model: String,
    content: Vec<ContentBlock>,
    stop_reason: Option<String>,
    usage: Usage,
}

/// Content block in the response.
#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    content_type: String,
    #[serde(default)]
    text: String,
}

/// Token usage information.
#[derive(Debug, Deserialize)]
struct Usage {
    input_tokens: u32,
    output_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_creation() {
        let client = AnthropicClient::new("test-key");
        assert_eq!(client.base_url, AnthropicClient::DEFAULT_BASE_URL);
    }

    #[test]
    fn test_client_custom_base_url() {
        let client = AnthropicClient::with_base_url("test-key", "https://custom.api.com");
        assert_eq!(client.base_url, "https://custom.api.com");
    }

    #[test]
    fn test_message_request_serialization() {
        let request = MessageRequest {
            model: "claude-3-sonnet".to_string(),
            max_tokens: 1024,
            messages: vec![Message::user("Hello")],
            system: Some("You are helpful.".to_string()),
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("claude-3-sonnet"));
        assert!(json.contains("Hello"));
        assert!(json.contains("system"));
    }

    #[test]
    fn test_message_request_no_system() {
        let request = MessageRequest {
            model: "claude-3-sonnet".to_string(),
            max_tokens: 1024,
            messages: vec![Message::user("Hello")],
            system: None,
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("system"));
    }
}
