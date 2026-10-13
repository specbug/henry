//! Claude's Messages API.

use hush::Secret;
use serde_json::{Value, json};

use crate::{Reply, Result, Tool, ToolCall, Turn, Usage, http_error, send_error};

const VERSION: &str = "2023-06-01";
const MAX_TOKENS: u32 = 16_000;

pub(crate) struct Claude {
    pub http: reqwest::Client,
    pub base: String,
    pub key: Secret,
    pub model: String,
}

impl Claude {
    pub async fn chat(&self, system: &str, turns: &[Turn], tools: &[Tool]) -> Result<Reply> {
        let body = request(&self.model, system, turns, tools);
        let response = self
            .http
            .post(format!("{}/messages", self.base))
            .header("x-api-key", self.key.expose())
            .header("anthropic-version", VERSION)
            .json(&body)
            .send()
            .await
            .map_err(|e| send_error(&self.base, e))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| send_error(&self.base, e))?;
        // 529: overloaded.
        if !status.is_success() {
            if status.as_u16() == 529 {
                return Err(crate::Error::Transient(format!("{} is overloaded", self.base)));
            }
            return Err(http_error(&self.base, status, &text));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| crate::Error::Transient(format!("{}: unreadable reply: {e}", self.base)))?;
        Ok(reply(&v))
    }
}

pub(crate) fn request(model: &str, system: &str, turns: &[Turn], tools: &[Tool]) -> Value {
    let messages: Vec<Value> = turns
        .iter()
        .map(|turn| match turn {
            Turn::User(text) => json!({ "role": "user", "content": text }),
            Turn::Assistant { text, calls } => {
                let mut content = Vec::new();
                if !text.is_empty() {
                    content.push(json!({ "type": "text", "text": text }));
                }
                content.extend(
                    calls.iter().map(|c| json!({ "type": "tool_use", "id": c.id, "name": c.name, "input": c.input })),
                );
                json!({ "role": "assistant", "content": content })
            }
            Turn::Results(results) => {
                let content: Vec<Value> = results
                    .iter()
                    .map(|r| {
                        json!({ "type": "tool_result", "tool_use_id": r.id, "content": r.content, "is_error": r.is_error })
                    })
                    .collect();
                json!({ "role": "user", "content": content })
            }
        })
        .collect();
    let tools: Vec<Value> = tools
        .iter()
        .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.schema }))
        .collect();
    json!({ "model": model, "max_tokens": MAX_TOKENS, "system": system, "messages": messages, "tools": tools })
}

pub(crate) fn reply(v: &Value) -> Reply {
    let mut text = String::new();
    let mut calls = Vec::new();
    for block in v["content"].as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => text.push_str(block["text"].as_str().unwrap_or_default()),
            Some("tool_use") => calls.push(ToolCall {
                id: block["id"].as_str().unwrap_or_default().to_owned(),
                name: block["name"].as_str().unwrap_or_default().to_owned(),
                input: block["input"].clone(),
            }),
            _ => {}
        }
    }
    Reply {
        text,
        calls,
        usage: Usage {
            input: v["usage"]["input_tokens"].as_u64().unwrap_or(0),
            output: v["usage"]["output_tokens"].as_u64().unwrap_or(0),
        },
    }
}
