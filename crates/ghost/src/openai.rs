//! OpenAI-style chat completions with tool calls: Meta's API, Gemini's
//! OpenAI-compatible endpoint, and OpenAI itself.

use hush::Secret;
use serde_json::{Value, json};

use crate::{Reply, Result, Tool, ToolCall, Turn, Usage, http_error, send_error};

pub(crate) struct OpenAi {
    pub http: reqwest::Client,
    pub base: String,
    pub key: Secret,
    pub model: String,
}

impl OpenAi {
    pub async fn chat(&self, system: &str, turns: &[Turn], tools: &[Tool]) -> Result<Reply> {
        let body = request(&self.model, system, turns, tools);
        let response = self
            .http
            .post(format!("{}/chat/completions", self.base))
            .bearer_auth(self.key.expose())
            .json(&body)
            .send()
            .await
            .map_err(|e| send_error(&self.base, e))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| send_error(&self.base, e))?;
        if !status.is_success() {
            return Err(http_error(&self.base, status, &text));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| crate::Error::Transient(format!("{}: unreadable reply: {e}", self.base)))?;
        Ok(reply(&v))
    }
}

pub(crate) fn request(model: &str, system: &str, turns: &[Turn], tools: &[Tool]) -> Value {
    let mut messages = vec![json!({ "role": "system", "content": system })];
    for turn in turns {
        match turn {
            Turn::User(text) => messages.push(json!({ "role": "user", "content": text })),
            Turn::Assistant { text, calls } => {
                let mut m = json!({ "role": "assistant", "content": text });
                if !calls.is_empty() {
                    m["tool_calls"] = calls
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.id,
                                "type": "function",
                                "function": { "name": c.name, "arguments": c.input.to_string() },
                            })
                        })
                        .collect();
                }
                messages.push(m);
            }
            Turn::Results(results) => {
                for r in results {
                    let content = if r.is_error { format!("error: {}", r.content) } else { r.content.clone() };
                    messages.push(json!({ "role": "tool", "tool_call_id": r.id, "content": content }));
                }
            }
        }
    }
    let tools: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.schema } })
        })
        .collect();
    json!({ "model": model, "messages": messages, "tools": tools })
}

pub(crate) fn reply(v: &Value) -> Reply {
    let message = &v["choices"][0]["message"];
    let calls = message["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, c)| {
            let arguments = &c["function"]["arguments"];
            // Arguments arrive as a JSON string; some servers send an object.
            let input = match arguments {
                Value::String(s) => serde_json::from_str(s).unwrap_or(Value::String(s.clone())),
                other => other.clone(),
            };
            ToolCall {
                id: c["id"].as_str().map_or_else(|| format!("call_{i}"), str::to_owned),
                name: c["function"]["name"].as_str().unwrap_or_default().to_owned(),
                input,
            }
        })
        .collect();
    Reply {
        text: message["content"].as_str().unwrap_or_default().to_owned(),
        calls,
        usage: Usage {
            input: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            output: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        },
    }
}
