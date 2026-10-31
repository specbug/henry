//! Ghost: the AI.
//!
//! A ghostwriter drafts and never signs. Ghost reads an unfamiliar repo and
//! proposes how to run it; deterministic code checks the proposal, builds it
//! and proves it. Ghost cannot mark anything live, open a port, or see a
//! secret's value, and everything it reads from a repo is untrusted data.
//!
//! Four providers sit behind one small interface. Meta's API, Gemini and
//! OpenAI all speak OpenAI-style chat with tool calls; Claude has its own
//! Messages API. Henry runs the tool loop itself ([`Ghost::propose`]), so no
//! agent framework and no provider is load-bearing.

mod claude;
mod openai;
mod propose;
pub mod tools;

use std::time::Duration;

use hush::Secret;
use lease::Provider;
use serde::Serialize;

pub use propose::{Attempt, Brief, Outcome, Planner, Settings};
pub use tools::Workspace;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Worth trying again later: the network, a timeout, a rate limit, an
    /// overloaded provider.
    #[error("{0}")]
    Transient(String),
    /// The provider refused the request: a bad key, an unknown model.
    #[error("{0}")]
    Provider(String),
    /// The model ran out of turns or tokens without a valid proposal.
    #[error("{0}")]
    Exhausted(String),
}

impl Error {
    pub fn is_transient(&self) -> bool {
        matches!(self, Error::Transient(_))
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Tokens a call used. The spend cap counts both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input + self.output
    }

    fn add(&mut self, other: Usage) {
        self.input += other.input;
        self.output += other.output;
    }
}

/// A tool the model may call.
#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema for the arguments.
    pub schema: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub id: String,
    pub content: String,
    pub is_error: bool,
}

/// One turn of a conversation, in Henry's terms. Each adapter maps these
/// onto its provider's wire format.
#[derive(Debug, Clone, PartialEq)]
pub enum Turn {
    User(String),
    Assistant { text: String, calls: Vec<ToolCall> },
    Results(Vec<ToolResult>),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reply {
    pub text: String,
    pub calls: Vec<ToolCall>,
    pub usage: Usage,
}

/// Where each provider lives, its default model, and the Hush name of its
/// key in Henry's own scope.
pub fn defaults(provider: Provider) -> (&'static str, &'static str, &'static str) {
    match provider {
        Provider::Meta => ("https://api.meta.ai/v1", "muse-spark-1.3", "META_API_KEY"),
        Provider::Gemini => {
            ("https://generativelanguage.googleapis.com/v1beta/openai", "gemini-2.5-pro", "GEMINI_API_KEY")
        }
        Provider::Claude => ("https://api.anthropic.com/v1", "claude-sonnet-5-5", "ANTHROPIC_API_KEY"),
        Provider::OpenAi => ("https://api.openai.com/v1", "gpt-5", "OPENAI_API_KEY"),
    }
}

/// The Hush name of a provider's API key, e.g. `META_API_KEY`.
pub fn key_name(provider: Provider) -> &'static str {
    defaults(provider).2
}

/// A model at one provider, ready to chat.
pub struct Model {
    provider: Provider,
    model: String,
    wire: Wire,
}

impl std::fmt::Debug for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Model({})", self.name())
    }
}

enum Wire {
    OpenAi(openai::OpenAi),
    Claude(claude::Claude),
}

impl Model {
    /// `base` overrides the provider's endpoint, e.g. for a proxy or a test.
    pub fn connect(provider: Provider, model: &str, key: Secret, base: Option<&str>) -> Result<Model> {
        let base = base.unwrap_or(defaults(provider).0).trim_end_matches('/').to_owned();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| Error::Provider(format!("http client: {e}")))?;
        let wire = match provider {
            Provider::Claude => Wire::Claude(claude::Claude { http, base, key, model: model.to_owned() }),
            _ => Wire::OpenAi(openai::OpenAi { http, base, key, model: model.to_owned() }),
        };
        Ok(Model { provider, model: model.to_owned(), wire })
    }

    /// `provider/model`, as leases and logs write it.
    pub fn name(&self) -> String {
        format!("{}/{}", self.provider.as_str(), self.model)
    }

    pub async fn chat(&self, system: &str, turns: &[Turn], tools: &[Tool]) -> Result<Reply> {
        match &self.wire {
            Wire::OpenAi(w) => w.chat(system, turns, tools).await,
            Wire::Claude(w) => w.chat(system, turns, tools).await,
        }
    }
}

/// Map an HTTP failure to transient or not, with the body's first line.
fn http_error(provider: &str, status: reqwest::StatusCode, body: &str) -> Error {
    let detail: String = body.lines().next().unwrap_or_default().chars().take(300).collect();
    let message = format!("{provider} answered {status}: {detail}");
    if status.as_u16() == 429 || status.is_server_error() {
        Error::Transient(message)
    } else {
        Error::Provider(message)
    }
}

fn send_error(provider: &str, e: reqwest::Error) -> Error {
    Error::Transient(format!("{provider}: {e}"))
}

#[cfg(test)]
mod tests;
